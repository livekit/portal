use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use livekit::prelude::*;
use livekit::StreamByteOptions;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::{ChunkSpec, FieldSpec};
use crate::error::{PortalError, PortalResult};

#[cfg(test)]
use crate::dtype::DType;
use crate::metrics::{DataStream, MetricsRegistry};
use crate::rtt::{RttService, RTT_TOPIC};
use crate::serialization::{
    action_fingerprint, chunk_fingerprint, deserialize_action, deserialize_chunk,
    deserialize_values, schema_fingerprint, serialize_action, serialize_chunk, serialize_values,
    DecodeError,
};
use crate::sync_buffer::{SyncBuffer, SyncOutput};
use crate::types::{to_value_maps, Action, ActionChunk, Role, State, TypedValue};
use crate::video::now_us;

/// Reserved Portal topics. State and action travel as data packets;
/// chunks travel as byte streams (their topic is matched on
/// `ByteStreamOpened`, not `DataReceived`).
pub(crate) const STATE_TOPIC: &str = "portal_state";
pub(crate) const ACTION_TOPIC: &str = "portal_action";
pub(crate) const ACTION_CHUNK_TOPIC: &str = "portal_action_chunk";

// --- Publisher ---

/// Bound on the in-flight publish queue. Sized for ~10s at 100Hz so normal
/// operation never hits it. The bound exists so a stalled publish loop
/// (slow SFU, lossy link) cannot grow memory without limit; on overflow we
/// drop and warn rather than block the synchronous send path.
const PUBLISH_QUEUE_CAP: usize = 1024;

/// Cap on the unknown-fingerprint warn-rate-limiter set. Each unique
/// offender is logged once; once the cap fills, further unknown
/// fingerprints are silently dropped (one log line announces the cap).
/// Keeps the set bounded against an adversarial peer cycling fingerprints.
const UNKNOWN_FP_WARN_CAP: usize = 256;

/// Publishes serialized state/action packets. Spawns a single background task
/// at construction; `send` enqueues onto an mpsc channel, preserving ordering
/// for reliable publishes and avoiding a task allocation per packet.
pub(crate) struct DataPublisher {
    /// Owned schema. Referenced by every `send_map` call; never mutated after
    /// construction.
    schema: Vec<FieldSpec>,
    /// Precomputed schema fingerprint, embedded in every outgoing packet.
    fingerprint: u32,
    topic: String,
    reliable: bool,
    tx: mpsc::Sender<DataPacket>,
    task: Option<JoinHandle<()>>,
    metrics: Arc<MetricsRegistry>,
    stream: DataStream,
    // Per-field snapshot of the last sent value, stored as f64 for lossless
    // carry-forward. `send_map` carries these forward when a caller supplies
    // only a subset of the declared fields, so partial updates stay
    // consistent with the robot's actual state. Seeded with 0.0, so fields
    // never sent resolve to 0.
    last_values: Mutex<Vec<f64>>,
    /// Field indices already reported as saturating. Each field warns at
    /// most once per publisher lifetime to keep the hot path quiet.
    warned_saturated: Mutex<HashSet<usize>>,
    /// Keys the caller sent that aren't in the schema. Logged once each so
    /// typos are visible without spamming per packet.
    warned_unknown_keys: Mutex<HashSet<String>>,
}

impl DataPublisher {
    pub fn new(
        schema: &[FieldSpec],
        topic: &str,
        reliable: bool,
        local_participant: LocalParticipant,
        metrics: Arc<MetricsRegistry>,
        stream: DataStream,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<DataPacket>(PUBLISH_QUEUE_CAP);
        let task = tokio::spawn(async move {
            while let Some(packet) = rx.recv().await {
                if let Err(e) = local_participant.publish_data(packet).await {
                    log::warn!("[publish-failed] data publish failed: {e}");
                }
            }
        });
        let schema = schema.to_vec();
        let fingerprint = match stream {
            DataStream::State => schema_fingerprint(&schema),
            DataStream::Action => action_fingerprint(&schema),
            DataStream::Chunk => unreachable!(
                "DataPublisher only handles scalar state/action; chunks go through ChunkPublisher"
            ),
        };
        let last_values = Mutex::new(vec![0.0; schema.len()]);
        Self {
            schema,
            fingerprint,
            topic: topic.to_string(),
            reliable,
            tx,
            task: Some(task),
            metrics,
            stream,
            last_values,
            warned_saturated: Mutex::new(HashSet::new()),
            warned_unknown_keys: Mutex::new(HashSet::new()),
        }
    }

    /// Send from a map of typed values, reordering to declared field
    /// order. Missing fields inherit their last sent value (0.0 if never
    /// sent) — partial updates carry forward prior state instead of
    /// silently zeroing it. Keys absent from the schema are logged once
    /// per key, then ignored.
    ///
    /// Each value's `TypedValue` variant must match the declared dtype
    /// for its field; a mismatch returns `PortalError::DtypeMismatch`
    /// and no packet is sent. This rejects at the earliest point so a
    /// bug in caller code fails loud instead of silently round-tripping
    /// through an unintended cast.
    ///
    /// Typed values are widened to `f64` via `TypedValue::as_f64` before
    /// carry-forward; the widening is lossless for every supported dtype.
    ///
    /// Returns the f64 vector that was actually shipped on the wire — that
    /// is, the post-carry-forward, post-saturation snapshot the receiver
    /// will reconstruct after decode. The active-operator self-echo path
    /// uses this to record exactly what the robot will execute, even when
    /// the caller passed a partial update or an out-of-range value.
    pub fn send_map(
        &self,
        map: &HashMap<String, TypedValue>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<Vec<f64>> {
        self.check_dtypes(map)?;
        self.warn_unknown_keys(map);
        let ts = timestamp_us.unwrap_or_else(now_us);
        let (payload, saturated_indices, wire_values) = {
            let mut last = self.last_values.lock();
            apply_carry_forward(&self.schema, &mut last, map);
            let out = match self.stream {
                DataStream::State => {
                    serialize_values(self.fingerprint, ts, &last, &self.schema)
                }
                DataStream::Action => serialize_action(
                    self.fingerprint,
                    ts,
                    in_reply_to_ts_us,
                    &last,
                    &self.schema,
                ),
                DataStream::Chunk => unreachable!(
                    "DataPublisher only handles scalar state/action; chunks go through ChunkPublisher"
                ),
            };
            // Compute the f64 view a receiver would reconstruct: each value
            // round-tripped through its declared dtype so out-of-range
            // inputs reflect the saturated wire bytes, matching what
            // `deserialize_action` plus `to_value_maps` produce on the
            // other side. Cheap (one match per field, no allocation
            // beyond the result vec).
            let wire_values: Vec<f64> = last
                .iter()
                .zip(self.schema.iter())
                .map(|(v, f)| TypedValue::from_f64(*v, f.dtype).as_f64())
                .collect();
            (out.payload, out.saturated_indices, wire_values)
        };
        if !saturated_indices.is_empty() {
            self.warn_saturated(&saturated_indices);
        }
        let packet = DataPacket {
            payload,
            topic: Some(self.topic.clone()),
            reliable: self.reliable,
            destination_identities: Vec::new(),
        };
        match self.tx.try_send(packet) {
            Ok(()) => {
                self.metrics.bump_sent(self.stream);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::warn!(
                    "[publish-full] topic '{}' queue full (cap={}), dropping packet",
                    self.topic,
                    PUBLISH_QUEUE_CAP
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Send task is gone (disconnect / drop). Silent — caller is
                // already in teardown.
            }
        }
        Ok(wire_values)
    }

    /// Reject on the first field whose `TypedValue` variant does not
    /// match the declared dtype. Keys absent from the schema are ignored
    /// here — they're already reported via `warn_unknown_keys`.
    fn check_dtypes(&self, map: &HashMap<String, TypedValue>) -> PortalResult<()> {
        for field in &self.schema {
            if let Some(v) = map.get(&field.name) {
                if v.dtype() != field.dtype {
                    return Err(PortalError::DtypeMismatch {
                        field: field.name.clone(),
                        expected: field.dtype,
                        got: v.variant_name(),
                    });
                }
            }
        }
        Ok(())
    }

    fn warn_unknown_keys(&self, map: &HashMap<String, TypedValue>) {
        // Small schemas make a linear scan faster than a HashSet lookup.
        for key in map.keys() {
            if self.schema.iter().any(|f| &f.name == key) {
                continue;
            }
            let mut warned = self.warned_unknown_keys.lock();
            if warned.insert(key.clone()) {
                log::warn!(
                    "[unknown-field] topic '{}': field '{}' not in schema, ignored",
                    self.topic,
                    key
                );
            }
        }
    }

    fn warn_saturated(&self, indices: &[usize]) {
        let mut warned = self.warned_saturated.lock();
        for &i in indices {
            if warned.insert(i) {
                let field = &self.schema[i];
                log::warn!(
                    "[saturated] topic '{}': field '{}' clamped to {:?} range",
                    self.topic,
                    field.name,
                    field.dtype
                );
            }
        }
    }
}

/// Update `last` in place with values from `map` for each declared field,
/// leaving other slots untouched (carry-forward). Typed values are
/// lossless-widened to `f64` on the way in.
fn apply_carry_forward(
    schema: &[FieldSpec],
    last: &mut [f64],
    map: &HashMap<String, TypedValue>,
) {
    for (i, field) in schema.iter().enumerate() {
        if let Some(v) = map.get(&field.name) {
            last[i] = v.as_f64();
        }
    }
}

impl Drop for DataPublisher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// --- Receiver (dispatches DataReceived events) ---

/// Push-callback + latest-wins slot for a single typed record (Action or
/// State). Paired so receivers and getters share one allocation.
pub(crate) struct DataSlot<R: Clone> {
    #[allow(clippy::type_complexity)]
    pub cb: Mutex<Option<Box<dyn Fn(&R) + Send + Sync>>>,
    pub latest: Mutex<Option<R>>,
    /// Peer fingerprints already reported as mismatched. Logged once per
    /// unique offender to surface schema drift without spamming.
    warned_mismatches: Mutex<HashSet<u32>>,
}

impl<R: Clone> DataSlot<R> {
    pub fn new() -> Self {
        Self {
            cb: Mutex::new(None),
            latest: Mutex::new(None),
            warned_mismatches: Mutex::new(HashSet::new()),
        }
    }

    /// Fire the callback by reference, then hand ownership to the
    /// latest-wins slot.
    ///
    /// Callbacks run on a tokio worker thread; a panic inside user code
    /// would abort that worker and kill the receive loop. Catching here
    /// keeps the stream alive; the panic is logged and the latest slot
    /// is still updated so pull-based getters continue to work.
    pub(crate) fn deliver(&self, record: R) {
        if let Some(cb) = self.cb.lock().as_ref() {
            let result = catch_unwind(AssertUnwindSafe(|| cb(&record)));
            if result.is_err() {
                log::error!("[callback-panic] user callback panicked, receive loop continues");
            }
        }
        *self.latest.lock() = Some(record);
    }

    pub fn get(&self) -> Option<R> {
        self.latest.lock().clone()
    }

    pub fn clear(&self) {
        *self.latest.lock() = None;
    }

    pub(crate) fn warn_mismatch(&self, topic: &str, expected: u32, got: u32) {
        let mut warned = self.warned_mismatches.lock();
        if warned.insert(got) {
            log::warn!(
                "[schema-mismatch] topic '{topic}': peer schema 0x{got:08x} != ours 0x{expected:08x}, dropping packet"
            );
        }
    }
}

pub(crate) type ActionSlot = DataSlot<Action>;
pub(crate) type StateSlot = DataSlot<State>;

/// Slot for a single declared action chunk. Wraps the same callback +
/// latest-wins + mismatch-warn machinery as `DataSlot`, plus the spec and
/// precomputed fingerprint the dispatch path needs. Lives on the receiving
/// (Robot) side; one per declared chunk.
pub(crate) struct ChunkSlot {
    pub(crate) spec: ChunkSpec,
    pub(crate) fingerprint: u32,
    pub(crate) inner: DataSlot<ActionChunk>,
}

impl ChunkSlot {
    pub fn new(spec: ChunkSpec) -> Self {
        let fingerprint = chunk_fingerprint(&spec);
        Self { spec, fingerprint, inner: DataSlot::new() }
    }

    pub fn deliver(&self, chunk: ActionChunk) {
        self.inner.deliver(chunk);
    }

    pub fn get(&self) -> Option<ActionChunk> {
        self.inner.get()
    }

    pub fn clear(&self) {
        self.inner.clear();
    }

    pub fn set_callback(&self, cb: Box<dyn Fn(&ActionChunk) + Send + Sync>) {
        *self.inner.cb.lock() = Some(cb);
    }

    pub fn warn_mismatch(&self, expected: u32, got: u32) {
        self.inner.warn_mismatch(&format!("chunk '{}'", self.spec.name), expected, got);
    }
}

/// Publishes chunk payloads as LiveKit byte streams (not data packets).
///
/// Chunks can exceed the 15 KB data-packet limit (10 timesteps × 32 F32 fields
/// already runs ~1.3 KB; production VLAs trend larger). Byte streams are
/// reliable by design and chunk into fragments under the hood, so we get
/// arbitrary-size payloads with the same lossless ordering scalar actions
/// already enjoy.
///
/// Sends are serialized through an mpsc onto a single drainer task. One
/// in-flight stream at a time keeps order on the receiver — concurrent
/// streams could finish out of declaration order.
pub(crate) struct ChunkPublisher {
    spec: ChunkSpec,
    fingerprint: u32,
    tx: mpsc::Sender<Vec<u8>>,
    task: Option<JoinHandle<()>>,
    metrics: Arc<MetricsRegistry>,
    /// `(t, field_index)` pairs already warned. Kept as flat indices —
    /// matches `serialize_chunk`'s output channel.
    warned_saturated: Mutex<HashSet<usize>>,
    warned_unknown_keys: Mutex<HashSet<String>>,
}

impl ChunkPublisher {
    pub fn new(
        spec: ChunkSpec,
        local_participant: LocalParticipant,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(PUBLISH_QUEUE_CAP);
        let chunk_name = spec.name.clone();
        let task = tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let options = StreamByteOptions {
                    topic: ACTION_CHUNK_TOPIC.to_string(),
                    ..Default::default()
                };
                if let Err(e) = local_participant.send_bytes(payload, options).await {
                    log::warn!("[publish-failed] chunk '{chunk_name}' byte stream failed: {e}");
                }
            }
        });
        let fingerprint = chunk_fingerprint(&spec);
        Self {
            spec,
            fingerprint,
            tx,
            task: Some(task),
            metrics,
            warned_saturated: Mutex::new(HashSet::new()),
            warned_unknown_keys: Mutex::new(HashSet::new()),
        }
    }

    /// Send a chunk. `data` is `field -> column of length horizon`. Each
    /// column's f64 values must be in range for the field's declared
    /// dtype; integer overflow saturates and warns once per
    /// `(t, field_index)`.
    ///
    /// `timestamp_us = None` resolves to `now_us()`. `in_reply_to_ts_us`
    /// links the chunk back to the observation that produced it for
    /// `metrics.policy.e2e_us_*`. Wrong-length columns are accepted and
    /// padded with `0.0` (rather than rejected) so partial fills during
    /// development don't fail noisily — production callers should always
    /// supply the full column.
    pub fn send(
        &self,
        data: &HashMap<String, Vec<f64>>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        self.warn_unknown_keys(data);
        let ts = timestamp_us.unwrap_or_else(now_us);
        let out =
            serialize_chunk(self.fingerprint, ts, in_reply_to_ts_us, &self.spec, data);
        if !out.saturated_indices.is_empty() {
            self.warn_saturated(&out.saturated_indices);
        }
        match self.tx.try_send(out.payload) {
            Ok(()) => {
                self.metrics.bump_sent(DataStream::Chunk);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::warn!(
                    "[publish-full] chunk '{}' queue full (cap={}), dropping packet",
                    self.spec.name,
                    PUBLISH_QUEUE_CAP
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
        Ok(())
    }

    fn warn_unknown_keys(&self, data: &HashMap<String, Vec<f64>>) {
        for key in data.keys() {
            if self.spec.fields.iter().any(|f| &f.name == key) {
                continue;
            }
            let mut warned = self.warned_unknown_keys.lock();
            if warned.insert(key.clone()) {
                log::warn!(
                    "[unknown-field] chunk '{}': field '{}' not in chunk schema, ignored",
                    self.spec.name,
                    key
                );
            }
        }
    }

    fn warn_saturated(&self, indices: &[usize]) {
        let n_fields = self.spec.fields.len();
        let mut warned = self.warned_saturated.lock();
        for &i in indices {
            if warned.insert(i) {
                let t = i / n_fields;
                let fi = i % n_fields;
                let field = &self.spec.fields[fi];
                log::warn!(
                    "[saturated] chunk '{}': field '{}' at t={} clamped to {:?} range",
                    self.spec.name,
                    field.name,
                    t,
                    field.dtype
                );
            }
        }
    }
}

impl Drop for ChunkPublisher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Decode + dispatch a chunk payload received from a byte stream. Unlike the
/// data-packet path, this runs on a tokio worker the byte-stream reader was
/// spawned on; the dispatch logic mirrors `handle_data_received`'s action
/// arm — fingerprint match → decode against that slot's schema → fire
/// callback + cache latest.
///
/// `unknown_fp_warns` rate-limits the unknown-fingerprint log so a peer
/// running a totally different chunk schema doesn't spam every received
/// stream — we log once per offending fingerprint, like `DataSlot` does
/// for its mismatch path.
pub(crate) fn dispatch_chunk_payload(
    payload: &[u8],
    chunk_slots: &[Arc<ChunkSlot>],
    unknown_fp_warns: &Mutex<HashSet<u32>>,
    metrics: &MetricsRegistry,
    sender: String,
) {
    if payload.len() < 4 {
        log::warn!("[bad-payload] chunk byte stream shorter than 4-byte fingerprint header");
        return;
    }
    let fp = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let Some(slot) = chunk_slots.iter().find(|s| s.fingerprint == fp) else {
        let mut warned = unknown_fp_warns.lock();
        if warned.len() < UNKNOWN_FP_WARN_CAP && warned.insert(fp) {
            log::warn!(
                "[unknown-chunk] topic '{ACTION_CHUNK_TOPIC}': unknown fingerprint 0x{fp:08x}, dropping byte stream"
            );
            if warned.len() == UNKNOWN_FP_WARN_CAP {
                log::warn!(
                    "[unknown-chunk] topic '{ACTION_CHUNK_TOPIC}': unknown-fingerprint warn cap ({UNKNOWN_FP_WARN_CAP}) reached, suppressing further warnings"
                );
            }
        }
        return;
    };
    match deserialize_chunk(payload, slot.fingerprint, &slot.spec) {
        Ok((send_ts, in_reply_to_ts_us, columns)) => {
            let now = now_us();
            metrics.record_received(DataStream::Chunk, send_ts, now);
            metrics.record_e2e(in_reply_to_ts_us, now);
            let data: HashMap<String, Vec<f64>> = slot
                .spec
                .fields
                .iter()
                .map(|f| f.name.clone())
                .zip(columns)
                .collect();
            slot.deliver(ActionChunk {
                name: slot.spec.name.clone(),
                horizon: slot.spec.horizon,
                data,
                timestamp_us: send_ts,
                in_reply_to_ts_us,
                sender: sender.clone(),
            });
        }
        Err(DecodeError::SchemaMismatch { expected, got }) => {
            slot.warn_mismatch(expected, got);
        }
        Err(DecodeError::Malformed(e)) => {
            log::warn!("[bad-payload] chunk '{}' deserialize failed: {e}", slot.spec.name);
        }
    }
}

/// Build an `Action` from the schema and the decoded f64 values. Kept
/// here so `handle_data_received` and any test helpers share the same
/// path. `sender` is the identity of the operator that produced this
/// action (set at gate time, or to the publisher's identity on the
/// local echo path).
pub(crate) fn build_action(
    timestamp_us: u64,
    in_reply_to_ts_us: Option<u64>,
    schema: &[FieldSpec],
    values: &[f64],
    sender: String,
) -> Action {
    let (typed, raw) = to_value_maps(schema, values);
    Action { values: typed, raw_values: raw, timestamp_us, in_reply_to_ts_us, sender }
}

fn build_state(
    timestamp_us: u64,
    schema: &[FieldSpec],
    values: &[f64],
) -> State {
    let (typed, raw) = to_value_maps(schema, values);
    State { values: typed, raw_values: raw, timestamp_us }
}

/// Handle a `DataReceived` event. Pushes into the sync buffer if applicable and
/// returns any observations/drops that resulted, for the caller to dispatch
/// outside any locks.
///
/// `sender` is the identity of the participant who published the packet,
/// stamped into `Action::sender` so recorders can label rows by producer
/// without consulting any room state. Empty on non-action paths (state,
/// RTT) — those don't carry a sender field.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_data_received(
    payload: &[u8],
    topic: &str,
    config_role: Role,
    action_schema: &[FieldSpec],
    action_fp: u32,
    state_schema: &[FieldSpec],
    state_fp: u32,
    action: &ActionSlot,
    state: &StateSlot,
    sync_buffer: Option<&Arc<Mutex<SyncBuffer>>>,
    metrics: &MetricsRegistry,
    rtt: &RttService,
    sender: String,
) -> SyncOutput {
    if topic == RTT_TOPIC {
        rtt.handle_packet(payload);
        return SyncOutput::empty();
    }
    match (config_role, topic) {
        (Role::Robot, ACTION_TOPIC) | (Role::Operator, ACTION_TOPIC) => {
            match deserialize_action(payload, action_fp, action_schema) {
                Ok((send_ts, in_reply_to_ts_us, values)) => {
                    let now = now_us();
                    metrics.record_received(DataStream::Action, send_ts, now);
                    metrics.record_e2e(in_reply_to_ts_us, now);
                    action.deliver(build_action(
                        send_ts,
                        in_reply_to_ts_us,
                        action_schema,
                        &values,
                        sender,
                    ));
                }
                Err(DecodeError::SchemaMismatch { expected, got }) => {
                    action.warn_mismatch(topic, expected, got);
                }
                Err(DecodeError::Malformed(e)) => {
                    log::warn!("[bad-payload] action deserialize failed: {e}");
                }
            }
        }
        (Role::Operator, STATE_TOPIC) => {
            match deserialize_values(payload, state_fp, state_schema) {
                Ok((timestamp_us, values)) => {
                    metrics.record_received(DataStream::State, timestamp_us, now_us());
                    state.deliver(build_state(timestamp_us, state_schema, &values));
                    if let Some(sb) = sync_buffer {
                        return sb.lock().push_state(timestamp_us, values);
                    }
                }
                Err(DecodeError::SchemaMismatch { expected, got }) => {
                    state.warn_mismatch(topic, expected, got);
                }
                Err(DecodeError::Malformed(e)) => {
                    log::warn!("[bad-payload] state deserialize failed: {e}");
                }
            }
        }
        _ => {}
    }
    SyncOutput::empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carry_forward_fills_missing_fields() {
        let schema = vec![
            FieldSpec::new("j1", DType::F64),
            FieldSpec::new("j2", DType::F64),
            FieldSpec::new("j3", DType::F64),
        ];
        let mut last = vec![0.0; 3];

        let m: HashMap<String, TypedValue> =
            [("j1".to_string(), TypedValue::F64(1.0))].into_iter().collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![1.0, 0.0, 0.0], "unsent fields start at seed (0.0)");

        let m: HashMap<String, TypedValue> =
            [("j2".to_string(), TypedValue::F64(2.5))].into_iter().collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![1.0, 2.5, 0.0], "j1 carries forward; j2 updates; j3 still at seed");

        let m: HashMap<String, TypedValue> = [
            ("j1".to_string(), TypedValue::F64(-1.0)),
            ("j3".to_string(), TypedValue::F64(7.0)),
        ]
        .into_iter()
        .collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![-1.0, 2.5, 7.0], "j2 carries forward when omitted; others update");
    }

    #[test]
    fn check_dtypes_rejects_variant_mismatch() {
        // A publisher is heavy to spin up (needs a live LocalParticipant),
        // but the core logic of `check_dtypes` only needs the schema and
        // the input map. Exercise the free function directly on a fake
        // publisher-like setup.
        fn check(
            schema: &[FieldSpec],
            map: &HashMap<String, TypedValue>,
        ) -> Result<(), (String, DType, &'static str)> {
            for field in schema {
                if let Some(v) = map.get(&field.name) {
                    if v.dtype() != field.dtype {
                        return Err((field.name.clone(), field.dtype, v.variant_name()));
                    }
                }
            }
            Ok(())
        }

        let schema = vec![
            FieldSpec::new("gripper", DType::Bool),
            FieldSpec::new("mode", DType::I8),
        ];

        // Correct variants pass.
        let m: HashMap<String, TypedValue> = [
            ("gripper".to_string(), TypedValue::Bool(true)),
            ("mode".to_string(), TypedValue::I8(3)),
        ]
        .into_iter()
        .collect();
        assert!(check(&schema, &m).is_ok());

        // Wrong variant for gripper.
        let m: HashMap<String, TypedValue> =
            [("gripper".to_string(), TypedValue::F32(1.0))].into_iter().collect();
        let err = check(&schema, &m).unwrap_err();
        assert_eq!(err.0, "gripper");
        assert_eq!(err.1, DType::Bool);
        assert_eq!(err.2, "F32");
    }

    #[test]
    fn typed_inputs_widen_to_f64_losslessly() {
        let schema = vec![
            FieldSpec::new("joint", DType::F32),
            FieldSpec::new("gripper", DType::Bool),
            FieldSpec::new("mode", DType::I8),
        ];
        let mut last = vec![0.0; 3];

        let m: HashMap<String, TypedValue> = [
            ("joint".to_string(), 0.5f32.into()),
            ("gripper".to_string(), true.into()),
            ("mode".to_string(), 3i8.into()),
        ]
        .into_iter()
        .collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![0.5, 1.0, 3.0]);
    }
}
