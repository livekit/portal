use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use livekit::prelude::*;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::config::{ChunkSpec, FieldSpec, PortalConfig};
use crate::data::{
    ACTION_CHUNK_TOPIC, ACTION_TOPIC, ActionSlot, ChunkPublisher, ChunkSlot, DataPublisher,
    STATE_TOPIC, StateSlot, dispatch_chunk_payload, handle_data_received,
};
use crate::error::{PortalError, PortalResult};
use crate::frame_video::{
    FRAME_VIDEO_TOPIC, FrameVideoPublisher, FrameVideoTrackEntry, dispatch_frame_payload,
};
use crate::metrics::{DataStream, MetricsRegistry, PortalMetrics};
use crate::rpc::{RpcError, RpcHandler, RpcInvocationData};
use crate::rtt::RttService;
use crate::serialization::{action_fingerprint, schema_fingerprint};
use crate::sync_buffer::{SyncBuffer, SyncOutput};
use crate::types::*;
use crate::video::{VideoPublisher, VideoReceiver, VideoTrackSlots};

/// Participant attribute keys used by Portal for role discovery and the
/// multi-controller pointer. Namespaced to avoid colliding with any
/// application-level attributes the user may also be setting.
pub const ROLE_ATTR_KEY: &str = "lk.portal.role";
pub const ACTIVE_OPERATOR_ATTR_KEY: &str = "lk.portal.active_operator";
const ROLE_VALUE_ROBOT: &str = "robot";
const ROLE_VALUE_OPERATOR: &str = "operator";
/// RPC method registered by Robot-side Portals so any participant can request
/// a change to the active operator pointer. Payload is the new identity, or
/// the empty string to clear. Result is the empty string on success.
pub const SET_ACTIVE_OPERATOR_RPC: &str = "portal.set_active_operator";

/// App-level RPC error code (outside the SDK's 1001-1999 reserved range)
/// returned when the robot has not yet finished setting up its
/// LocalParticipant.
const RPC_NOT_CONNECTED: u32 = 2001;
/// App-level RPC error code returned when the SDK's `set_attributes` fails
/// while the robot is processing a `set_active_operator` request.
const RPC_SET_ATTRIBUTES_FAILED: u32 = 2002;

type ObservationCb = Box<dyn Fn(&Observation) + Send + Sync>;
type DropCb = Box<dyn Fn(Vec<HashMap<String, TypedValue>>) + Send + Sync>;
type IdentityCb = Box<dyn Fn(&str) + Send + Sync>;
type OptIdentityCb = Box<dyn Fn(Option<&str>) + Send + Sync>;

/// State for the v0.2 multi-controller layer. Lives in an `Arc` so the room
/// event handler and the `set_active_operator` RPC handler can share it
/// without copying.
pub(crate) struct ControllerState {
    /// On Robot side: source of truth, mirrored as own `lk.portal.active_operator`
    /// attribute. On Operator side: a mirror of the robot's attribute, updated
    /// from `ParticipantAttributesChanged` events.
    pub(crate) active_operator: Mutex<Option<String>>,
    /// Identities of currently-connected operators (excluding self), populated
    /// from `ParticipantConnected` plus the initial remote-participants
    /// snapshot.
    pub(crate) operators: Mutex<HashSet<String>>,
    /// The robot's identity, discovered by reading the `lk.portal.role`
    /// attribute. Operators use this to address `set_active_operator` RPCs.
    pub(crate) robot_identity: Mutex<Option<String>>,

    on_operator_joined: Mutex<Option<IdentityCb>>,
    on_operator_left: Mutex<Option<IdentityCb>>,
    on_active_operator_changed: Mutex<Option<OptIdentityCb>>,
}

impl ControllerState {
    fn new() -> Self {
        Self {
            active_operator: Mutex::new(None),
            operators: Mutex::new(HashSet::new()),
            robot_identity: Mutex::new(None),
            on_operator_joined: Mutex::new(None),
            on_operator_left: Mutex::new(None),
            on_active_operator_changed: Mutex::new(None),
        }
    }

    fn fire_op_joined(&self, identity: &str) {
        if let Some(cb) = self.on_operator_joined.lock().as_ref() {
            let result = catch_unwind(AssertUnwindSafe(|| cb(identity)));
            if result.is_err() {
                log::error!("[callback-panic] on_operator_joined callback panicked");
            }
        }
    }

    fn fire_op_left(&self, identity: &str) {
        if let Some(cb) = self.on_operator_left.lock().as_ref() {
            let result = catch_unwind(AssertUnwindSafe(|| cb(identity)));
            if result.is_err() {
                log::error!("[callback-panic] on_operator_left callback panicked");
            }
        }
    }

    fn fire_active_changed(&self, identity: Option<&str>) {
        if let Some(cb) = self.on_active_operator_changed.lock().as_ref() {
            let result = catch_unwind(AssertUnwindSafe(|| cb(identity)));
            if result.is_err() {
                log::error!("[callback-panic] on_active_operator_changed callback panicked");
            }
        }
    }

    fn clear(&self) {
        *self.active_operator.lock() = None;
        self.operators.lock().clear();
        *self.robot_identity.lock() = None;
    }

    /// Partial clear used on `RoomEvent::Reconnected`. Drops the per-room
    /// rosters (`operators`, `robot_identity`) so post-reconnect
    /// `ParticipantConnected` events can rebuild them from scratch, but
    /// keeps `active_operator` pinned. Two reasons:
    ///
    /// * **Robot side.** The robot's `active_operator` is the source of
    ///   truth (mirrored as its own `lk.portal.active_operator` attribute).
    ///   The robot ignores attribute events on its local identity (see
    ///   `ParticipantAttributesChanged` filter), and the SDK never fires
    ///   `ParticipantConnected` for self, so a full clear here would leave
    ///   the gate stuck at `None` until something explicitly re-set it —
    ///   silently halting control across a transient reconnect.
    /// * **Operator side.** The mirror is reseeded by the next
    ///   `ParticipantConnected` for the robot (via `classify_and_update`).
    ///   `classify_and_update` only fires `on_active_operator_changed` on
    ///   a value change, so retaining a stale value across the reconnect
    ///   does not produce a spurious callback when it gets re-read.
    fn clear_for_reconnect(&self) {
        self.operators.lock().clear();
        *self.robot_identity.lock() = None;
    }
}

/// Classify a participant by their `lk.portal.role` attribute. Returns
/// `None` if the attribute is absent or has an unknown value.
fn classify_role(attrs: &HashMap<String, String>) -> Option<Role> {
    match attrs.get(ROLE_ATTR_KEY).map(String::as_str) {
        Some(ROLE_VALUE_ROBOT) => Some(Role::Robot),
        Some(ROLE_VALUE_OPERATOR) => Some(Role::Operator),
        _ => None,
    }
}

/// Drains the buffers returned by `SyncBuffer::push_*` and dispatches them to
/// the user — callback first (by reference, no clone), then into the pull-based
/// observation buffer. Kept separate from `SyncBuffer` so callbacks run with no
/// sync-buffer lock held.
pub(crate) struct ObservationSink {
    observation_cb: Mutex<Option<ObservationCb>>,
    drop_cb: Mutex<Option<DropCb>>,
    // Latest-wins slot. Consumers peek via `get()` (clone). Consumers that
    // want history register `on_observation` and buffer on their own side.
    latest: Mutex<Option<Observation>>,
}

impl ObservationSink {
    pub(crate) fn new() -> Self {
        Self {
            observation_cb: Mutex::new(None),
            drop_cb: Mutex::new(None),
            latest: Mutex::new(None),
        }
    }

    pub(crate) fn dispatch(&self, output: SyncOutput) {
        let SyncOutput { observations, drops } = output;

        // User callbacks run on the tokio worker dispatching room events.
        // A panic here would abort the whole event loop, so we catch and
        // log and keep going.
        if !observations.is_empty() {
            {
                let cb_slot = self.observation_cb.lock();
                if let Some(cb) = cb_slot.as_ref() {
                    for obs in &observations {
                        let result = catch_unwind(AssertUnwindSafe(|| cb(obs)));
                        if result.is_err() {
                            log::error!(
                                "[callback-panic] observation callback panicked, event loop continues"
                            );
                        }
                    }
                }
            }
            // Latest-wins: only the final observation needs to reach the pull
            // slot — intermediates are discarded either way.
            if let Some(last_obs) = observations.into_iter().last() {
                *self.latest.lock() = Some(last_obs);
            }
        }

        if !drops.is_empty() {
            if let Some(cb) = self.drop_cb.lock().as_ref() {
                let result = catch_unwind(AssertUnwindSafe(|| cb(drops)));
                if result.is_err() {
                    log::error!("[callback-panic] drop callback panicked, event loop continues");
                }
            }
        }
    }

    pub(crate) fn get(&self) -> Option<Observation> {
        self.latest.lock().clone()
    }

    pub(crate) fn clear(&self) {
        *self.latest.lock() = None;
    }

    pub(crate) fn set_observation_cb(&self, cb: ObservationCb) {
        *self.observation_cb.lock() = Some(cb);
    }

    pub(crate) fn set_drop_cb(&self, cb: DropCb) {
        *self.drop_cb.lock() = Some(cb);
    }
}

struct ConnectionState {
    room: Option<Room>,
    event_task: Option<JoinHandle<()>>,
    rtt: Option<Arc<RttService>>,
}

pub struct Portal {
    config: PortalConfig,

    // Serializes connect()/disconnect() so a disconnect() yielding on
    // room.close().await can't be overtaken by a concurrent connect()
    // whose newly-populated state would then be clobbered by the
    // disconnect's cleanup path.
    lifecycle: tokio::sync::Mutex<()>,

    // Lifecycle state (connect/disconnect).
    conn: Mutex<ConnectionState>,

    // Video receivers are spawned by the event loop (on TrackSubscribed) and
    // torn down by `disconnect`, so they live in an Arc shared with both.
    video_receivers: Arc<Mutex<HashMap<String, VideoReceiver>>>,

    // Hot-path publishers. Each is guarded by its own mutex so send methods
    // can clone the Arc out and drop the lock before doing any IO.
    video_publishers: Mutex<HashMap<String, Arc<VideoPublisher>>>,
    /// Robot-side: one publisher per declared frame-video track. Frame-video
    /// frames travel as byte streams (per-frame RGB encode), bypassing the
    /// WebRTC media path.
    frame_video_publishers: Mutex<HashMap<String, Arc<FrameVideoPublisher>>>,
    state_publisher: Mutex<Option<Arc<DataPublisher>>>,
    action_publisher: Mutex<Option<Arc<DataPublisher>>>,
    /// Operator-side: one publisher per declared action chunk.
    chunk_publishers: Mutex<HashMap<String, Arc<ChunkPublisher>>>,

    // Operator-side sync + dispatch.
    sync_buffer: Mutex<Option<Arc<Mutex<SyncBuffer>>>>,
    obs_sink: Arc<ObservationSink>,

    // Push callback + pull latest-wins slot, bundled per stream.
    action: Arc<ActionSlot>,
    state: Arc<StateSlot>,
    /// Robot-side: one slot per declared action chunk. Fixed at construction
    /// (keyed by chunk name) so the receive path doesn't lock the map.
    chunk_slots: HashMap<String, Arc<ChunkSlot>>,
    /// Rate-limit set for unknown chunk fingerprints — the byte-stream
    /// equivalent of `DataSlot::warned_mismatches`, but lives at the
    /// dispatcher level because no slot owns "unknown" packets.
    unknown_chunk_fp_warns: Arc<Mutex<HashSet<u32>>>,
    // Fixed at construction (keyed by declared video_tracks) — no lock on the map itself.
    video_tracks: HashMap<String, Arc<VideoTrackSlots>>,
    /// Names of all video tracks (WebRTC + frame video) in declaration
    /// order. Used by `setup_operator` to size the sync buffer over the
    /// union of transports. Computed once at `Portal::new` so the connect
    /// hot path doesn't re-walk the config.
    all_track_names: Vec<String>,
    /// Per-track frame-video entries (spec + slots + metrics fused). Fixed
    /// at construction and shared as an `Arc<HashMap>` so the receive
    /// dispatch can fan out to per-frame spawn tasks via a refcount bump
    /// instead of cloning the whole map (which would allocate one `String`
    /// per declared track per received frame).
    frame_video_entries: Arc<HashMap<String, Arc<FrameVideoTrackEntry>>>,

    metrics: Arc<MetricsRegistry>,

    // RPC methods the caller has registered. Applied to the LocalParticipant
    // on connect(); survives disconnect so reconnects reapply them.
    rpc_handlers: Arc<Mutex<HashMap<String, RpcHandler>>>,

    // Local-participant handle held in its own `Arc<Mutex<...>>` so the
    // built-in `set_active_operator` RPC handler can clone access to it
    // without sharing the broader `ConnectionState`.
    local_participant: Arc<Mutex<Option<LocalParticipant>>>,

    // Multi-controller state (v0.2). Shared with the room event handler so
    // attribute-change and participant-connect events can update operators,
    // robot_identity, and active_operator without taking a Portal-level lock.
    controller: Arc<ControllerState>,
}

impl Portal {
    pub fn new(config: PortalConfig) -> Self {
        // Slots and metrics cover both transports. Frame-video and WebRTC
        // tracks share the same VideoFrameData / VideoTrackSlots / sync
        // buffer, so the consumer-facing API is identical.
        let all_track_names = combined_track_names(&config);
        let video_tracks: HashMap<String, Arc<VideoTrackSlots>> = all_track_names
            .iter()
            .map(|name| (name.clone(), Arc::new(VideoTrackSlots::new())))
            .collect();

        let metrics = Arc::new(MetricsRegistry::new(&all_track_names));
        let obs_sink = Arc::new(ObservationSink::new());

        // Build chunk slots once at construction so the dispatch table is
        // immutable for the Portal's lifetime — `handle_room_event` reads
        // them without taking any Portal-level lock.
        let chunk_slots: HashMap<String, Arc<ChunkSlot>> = config
            .action_chunks
            .iter()
            .map(|spec| (spec.name.clone(), Arc::new(ChunkSlot::new(spec.clone()))))
            .collect();

        // Same idea for frame-video entries: the dispatch path reads them
        // per packet, so freezing the map at construction lets the hot path
        // skip a Portal-level lock and the per-connect rebuild. Each entry
        // bundles spec + slots + metrics so dispatch is a single lookup.
        // Wrapped in `Arc<HashMap>` so per-frame fan-out is a refcount bump
        // rather than a `String`-cloning map clone.
        let frame_video_entries: Arc<HashMap<String, Arc<FrameVideoTrackEntry>>> = Arc::new(
            config
                .frame_video_tracks
                .iter()
                .map(|spec| {
                    let slots = video_tracks
                        .get(&spec.name)
                        .expect("video_tracks contains every frame-video name")
                        .clone();
                    let track_metrics = metrics
                        .track(&spec.name)
                        .expect("track metrics registered for every frame-video name");
                    (
                        spec.name.clone(),
                        Arc::new(FrameVideoTrackEntry {
                            spec: spec.clone(),
                            metrics: track_metrics,
                            slots,
                        }),
                    )
                })
                .collect(),
        );

        Self {
            config,
            lifecycle: tokio::sync::Mutex::new(()),
            conn: Mutex::new(ConnectionState { room: None, event_task: None, rtt: None }),
            video_receivers: Arc::new(Mutex::new(HashMap::new())),
            video_publishers: Mutex::new(HashMap::new()),
            frame_video_publishers: Mutex::new(HashMap::new()),
            state_publisher: Mutex::new(None),
            action_publisher: Mutex::new(None),
            chunk_publishers: Mutex::new(HashMap::new()),
            sync_buffer: Mutex::new(None),
            obs_sink,
            action: Arc::new(ActionSlot::new()),
            state: Arc::new(StateSlot::new()),
            chunk_slots,
            unknown_chunk_fp_warns: Arc::new(Mutex::new(HashSet::new())),
            video_tracks,
            all_track_names,
            frame_video_entries,
            metrics,
            rpc_handlers: Arc::new(Mutex::new(HashMap::new())),
            local_participant: Arc::new(Mutex::new(None)),
            controller: Arc::new(ControllerState::new()),
        }
    }

    pub async fn connect(&self, url: &str, token: &str) -> PortalResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.conn.lock().room.is_some() {
            return Err(PortalError::AlreadyConnected);
        }

        let mut options = RoomOptions::default();
        options.auto_subscribe = true;
        if let Some(key) = &self.config.shared_key {
            use livekit::E2eeOptions;
            use livekit::e2ee::{
                EncryptionType,
                key_provider::{KeyProvider, KeyProviderOptions},
            };
            let key_provider =
                KeyProvider::with_shared_key(KeyProviderOptions::default(), key.clone());
            options.encryption =
                Some(E2eeOptions { key_provider, encryption_type: EncryptionType::Gcm });
        }

        log::info!("[{}] connecting as {:?} to {}", self.config.session, self.config.role, url);

        let (room, events) = Room::connect(url, token, options)
            .await
            .map_err(|e| PortalError::Room(e.to_string()))?;

        // Store the LocalParticipant before applying handlers so a concurrent
        // `register_rpc_method` either (a) inserts before we iterate and gets
        // picked up, or (b) inserts after we've stored LP and forwards the
        // handler itself. Overlap is idempotent — the SDK's rpc handler map
        // is last-writer-wins.
        let local_participant = room.local_participant();
        *self.local_participant.lock() = Some(local_participant.clone());

        // Robot-side: register the built-in `set_active_operator` RPC. The
        // handler clones the LP slot and the controller Arc so the closure
        // can update both the attribute and the local mirror without holding
        // any Portal-level lock.
        if self.config.role == Role::Robot {
            let lp_slot = self.local_participant.clone();
            let controller = self.controller.clone();
            let handler: RpcHandler = Arc::new(move |data: RpcInvocationData| {
                let lp_slot = lp_slot.clone();
                let controller = controller.clone();
                Box::pin(
                    async move { set_active_operator_rpc_impl(&lp_slot, &controller, data).await },
                )
            });
            self.register_rpc_method(SET_ACTIVE_OPERATOR_RPC, handler);
        }

        self.apply_rpc_handlers(&local_participant);

        // Self-set the role attribute so other participants can discover us.
        // Token-mint may also have set this key; in that case `set_attributes`
        // is effectively a no-op for the same value.
        let role_value = match self.config.role {
            Role::Robot => ROLE_VALUE_ROBOT,
            Role::Operator => ROLE_VALUE_OPERATOR,
        };
        let mut role_attrs = HashMap::new();
        role_attrs.insert(ROLE_ATTR_KEY.to_string(), role_value.to_string());
        if let Err(e) = local_participant.set_attributes(role_attrs).await {
            // Most common cause: the token grant did not include
            // `canUpdateOwnMetadata`. Surface a clear error so callers fix
            // their token-mint script rather than silently leaving the
            // participant unidentified. Roll back the partial state we
            // already wrote (LP slot, RPC handler bindings) so a retry
            // starts from a clean slate.
            self.rollback_partial_connect();
            let _ = room.close().await;
            return Err(PortalError::Room(format!(
                "failed to publish role attribute (token may be missing canUpdateOwnMetadata): {e}"
            )));
        }

        // Robot-side: if the token seeded `lk.portal.active_operator`,
        // mirror it locally so the action gate sees the configured pointer
        // before anyone calls `set_active_operator`.
        if self.config.role == Role::Robot {
            let attrs = local_participant.attributes();
            if let Some(seed) = attrs.get(ACTIVE_OPERATOR_ATTR_KEY) {
                let value = if seed.is_empty() { None } else { Some(seed.clone()) };
                *self.controller.active_operator.lock() = value;
            }
        }

        // Walk the room snapshot once at connect so any participant that
        // joined before us is already in `operators` / `robot_identity`. New
        // joiners get added by the `ParticipantConnected` event handler.
        for (_sid, participant) in room.remote_participants() {
            classify_and_update(
                &self.controller,
                self.config.role,
                &participant.identity(),
                &participant.attributes(),
            );
        }

        let setup_result = match self.config.role {
            Role::Robot => self.setup_robot(&room).await,
            Role::Operator => {
                self.setup_operator(&room);
                Ok(())
            }
        };
        if let Err(e) = setup_result {
            // setup_robot can fail mid-way through publishing video tracks.
            // Its own rollback already clears the partial video_publishers
            // map; we still need to undo the LP slot, controller mirror,
            // and any other state written above before bailing.
            self.rollback_partial_connect();
            let _ = room.close().await;
            return Err(e);
        }

        let rtt = Arc::new(RttService::spawn(
            local_participant.clone(),
            self.config.ping_ms,
            self.metrics.clone(),
        ));

        log::info!("[{}] connected as {:?}", self.config.session, self.config.role);

        // Event dispatch runs off a snapshot of the fields it touches, not the
        // whole Portal, so it doesn't need any outer lock.
        let action_schema_fp = action_fingerprint(&self.config.action_schema);
        let state_schema_fp = schema_fingerprint(&self.config.state_schema);
        // The dispatch path needs a slice for fingerprint lookup; the map
        // form is for `get_action_chunk` / `on_action_chunk` name lookups.
        // Build the slice once per connect so the event loop iterates a
        // plain Vec, not a HashMap.
        let chunk_slots_for_dispatch: Vec<Arc<ChunkSlot>> =
            self.chunk_slots.values().cloned().collect();
        let local_identity = local_participant.identity().as_str().to_string();
        let ctx = EventContext {
            config: self.config.clone(),
            action_schema_fp,
            state_schema_fp,
            sync_buffer: self.sync_buffer.lock().clone(),
            obs_sink: self.obs_sink.clone(),
            action: self.action.clone(),
            state: self.state.clone(),
            chunk_slots: chunk_slots_for_dispatch,
            unknown_chunk_fp_warns: self.unknown_chunk_fp_warns.clone(),
            video_tracks: self.video_tracks.clone(),
            video_receivers: self.video_receivers.clone(),
            frame_video_entries: self.frame_video_entries.clone(),
            metrics: self.metrics.clone(),
            rtt: rtt.clone(),
            controller: self.controller.clone(),
            local_identity,
        };
        let event_handle = tokio::spawn(async move {
            let mut events = events;
            while let Some(event) = events.recv().await {
                handle_room_event(&ctx, event);
            }
        });

        let mut state = self.conn.lock();
        state.room = Some(room);
        state.event_task = Some(event_handle);
        state.rtt = Some(rtt);
        Ok(())
    }

    pub fn send_video_frame(
        &self,
        track_name: &str,
        rgb_data: &[u8],
        width: u32,
        height: u32,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        // Two transports, one user-facing method. WebRTC publishers and
        // frame-video publishers are populated by `add_video` at config
        // time — codec selection routes the spec to one list or the
        // other — and names are unique across both, so a track lives in
        // exactly one map.
        if let Some(publisher) = self.video_publishers.lock().get(track_name).cloned() {
            return publisher.send_frame(rgb_data, width, height, timestamp_us);
        }
        if let Some(publisher) = self.frame_video_publishers.lock().get(track_name).cloned() {
            return publisher.send_frame(rgb_data, width, height, timestamp_us);
        }
        // Distinguish wrong-role (track is declared but no publisher exists
        // because send is operator-side) from genuinely unknown-track. The
        // operator never spawns video publishers, so a declared name with
        // no publisher means "wrong role" — same shape as `send_state` /
        // `send_action_chunk`.
        if self.config.role != Role::Robot
            && (self.config.video_tracks.iter().any(|s| s.name == track_name)
                || self.config.frame_video_tracks.iter().any(|s| s.name == track_name))
        {
            return Err(PortalError::WrongRole(self.config.role));
        }
        Err(PortalError::UnknownVideoTrack { name: track_name.to_string() })
    }

    /// Publish a state sample (robot only). Values are typed — build the
    /// map with `TypedValue::Bool(true)`, `0.5f32.into()`, etc. The
    /// pipeline internally widens to `f64` for carry-forward and casts
    /// back to the declared dtype at the wire boundary.
    pub fn send_state(
        &self,
        values: &HashMap<String, TypedValue>,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        let publisher =
            self.state_publisher.lock().clone().ok_or(PortalError::WrongRole(Role::Operator))?;
        // State has no echo path; drop the wire-values vector that
        // `send_map` returns for action callers.
        publisher.send_map(values, timestamp_us, None).map(|_| ())
    }

    /// Publish an action (operator only).
    ///
    /// `in_reply_to_ts_us` is the timestamp of the observation this action
    /// was produced from — pass `Some(obs.timestamp_us)` to give the
    /// receiver the data it needs to compute true end-to-end policy
    /// latency (`metrics.policy.e2e_us_*`). Pass `None` for unsolicited
    /// publishes (teleop, idle commands).
    pub fn send_action(
        &self,
        values: &HashMap<String, TypedValue>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        let publisher =
            self.action_publisher.lock().clone().ok_or(PortalError::WrongRole(Role::Robot))?;
        // Resolve the actual send timestamp the publisher will stamp onto
        // the wire so the local echo (if any) sees the same value the
        // robot sees. `send_map` would default `None` to `now_us()` and
        // we'd pick a slightly later timestamp here.
        let send_ts = timestamp_us.unwrap_or_else(crate::video::now_us);
        let wire_values = publisher.send_map(values, Some(send_ts), in_reply_to_ts_us)?;
        // Echo path. LiveKit does not fan out a publisher's own data
        // packets, so without this an active operator would never see its
        // own action through `on_action`. We only echo when subscription
        // is on AND we are the active operator: otherwise this would just
        // be local noise that nobody else in the room sees either.
        //
        // `wire_values` is what the receiver will reconstruct after decode:
        // post-carry-forward (so omitted fields keep their last-sent value
        // rather than reading as 0.0) and post-saturation (so out-of-range
        // inputs match the clipped wire bytes). Building the echo from the
        // caller's input map directly would silently diverge whenever a
        // partial update or saturating value is involved.
        if self.config.action_subscription && self.is_self_active() {
            // We only echo when self is the active operator, which means
            // we are connected and have a local identity. Unwrap is safe.
            let local_id =
                self.local_identity().expect("local_identity is Some when self == active_operator");
            let action = crate::data::build_action(
                send_ts,
                in_reply_to_ts_us,
                &self.config.action_schema,
                &wire_values,
                local_id,
            );
            self.action.deliver(action);
        }
        Ok(())
    }

    /// Publish an action chunk on the named chunk schema (operator only).
    ///
    /// `data` is `field -> column of length horizon`. Columns shorter than
    /// `horizon` are zero-padded, longer columns are truncated, and unknown
    /// keys are warned-and-ignored once each. Use `in_reply_to_ts_us` the
    /// same way as `send_action` to feed `metrics.policy.e2e_us_*`.
    pub fn send_action_chunk(
        &self,
        chunk_name: &str,
        data: &HashMap<String, Vec<f64>>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        let publisher = {
            let map = self.chunk_publishers.lock();
            map.get(chunk_name).cloned()
        };
        let Some(publisher) = publisher else {
            // No publisher resolves to one of three precise errors so the
            // caller sees the actual mistake instead of a generic refusal:
            // wrong role, undeclared chunk name, or operator-but-not-yet
            // connected (publishers are spawned in `setup_operator`).
            return if self.config.role != Role::Operator {
                Err(PortalError::WrongRole(Role::Robot))
            } else if !self.chunk_slots.contains_key(chunk_name) {
                Err(PortalError::UnknownChunk { name: chunk_name.to_string() })
            } else {
                Err(PortalError::NotConnected)
            };
        };
        let send_ts = timestamp_us.unwrap_or_else(crate::video::now_us);
        publisher.send(data, Some(send_ts), in_reply_to_ts_us)?;
        // Echo path: same conditions as `send_action`. Unlike scalar
        // actions where we rebuild the typed values, chunks already carry
        // raw `f64` columns — we hand the same `data` map straight to the
        // slot, padded/truncated to the declared horizon to match what
        // the wire path emits.
        if self.config.action_subscription && self.is_self_active() {
            if let Some(slot) = self.chunk_slots.get(chunk_name) {
                let local_id = self
                    .local_identity()
                    .expect("local_identity is Some when self == active_operator");
                let horizon = slot.spec.horizon as usize;
                let normalized: HashMap<String, Vec<f64>> = slot
                    .spec
                    .fields
                    .iter()
                    .map(|f| {
                        let mut col = data.get(&f.name).cloned().unwrap_or_default();
                        if col.len() < horizon {
                            col.resize(horizon, 0.0);
                        } else if col.len() > horizon {
                            col.truncate(horizon);
                        }
                        (f.name.clone(), col)
                    })
                    .collect();
                slot.deliver(ActionChunk {
                    name: slot.spec.name.clone(),
                    horizon: slot.spec.horizon,
                    data: normalized,
                    timestamp_us: send_ts,
                    in_reply_to_ts_us,
                    sender: local_id,
                });
            }
        }
        Ok(())
    }

    // --- RPC ---

    /// Declared state schema (field names + dtypes), in declaration order.
    /// Bindings mirror this snapshot internally; reading from the Portal
    /// keeps the snapshot single-sourced.
    pub fn state_schema(&self) -> &[FieldSpec] {
        self.config.state_schema()
    }

    /// Declared action schema, same semantics as `state_schema`.
    pub fn action_schema(&self) -> &[FieldSpec] {
        self.config.action_schema()
    }

    // --- Multi-controller surface (v0.2) ---

    /// This Portal's own LiveKit identity once connected. Reads from the
    /// stored `LocalParticipant`. `None` before `connect()` succeeds.
    pub fn local_identity(&self) -> Option<String> {
        self.local_participant.lock().as_ref().map(|lp| lp.identity().as_str().to_string())
    }

    /// Identity of the operator the robot is currently listening to, or
    /// `None` if no operator is selected. On Robot side this is the local
    /// pointer (also broadcast as the `lk.portal.active_operator` attribute).
    /// On Operator side it is a mirror of the robot's attribute.
    pub fn active_operator(&self) -> Option<String> {
        self.controller.active_operator.lock().clone()
    }

    /// `true` iff this Portal's local identity is the current
    /// `active_operator`. Used internally by the echo path; exposed so
    /// callers can decide whether to record their own outgoing actions.
    fn is_self_active(&self) -> bool {
        let local = self.local_identity();
        let active = self.controller.active_operator.lock().clone();
        match (local, active) {
            (Some(local), Some(active)) => local == active,
            _ => false,
        }
    }

    /// Set the active operator. On Robot side this updates the local pointer
    /// and broadcasts via the robot's own attributes. On Operator side this
    /// dispatches a `portal.set_active_operator` RPC to the robot.
    ///
    /// Pass `None` to clear and drop all incoming actions.
    pub async fn set_active_operator(&self, identity: Option<String>) -> PortalResult<()> {
        match self.config.role {
            Role::Robot => {
                let lp = self.local_participant.lock().clone().ok_or(PortalError::NotConnected)?;
                let prev = self.controller.active_operator.lock().clone();
                let mut attrs = HashMap::new();
                attrs.insert(
                    ACTIVE_OPERATOR_ATTR_KEY.to_string(),
                    identity.clone().unwrap_or_default(),
                );
                lp.set_attributes(attrs).await.map_err(|e| PortalError::Room(e.to_string()))?;
                *self.controller.active_operator.lock() = identity.clone();
                if prev != identity {
                    self.controller.fire_active_changed(identity.as_deref());
                }
                Ok(())
            }
            Role::Operator => {
                // Cached robot identity (populated by attribute events) is
                // the fast path. The common pattern is:
                //
                //   await op.connect(...)
                //   await op.set_active_operator(op.local_identity())
                //
                // immediately after connect, before the SDK has surfaced the
                // robot's attributes via `ParticipantAttributesChanged`. To
                // make that work without forcing every caller to manually
                // wait, we scan `remote_participants()` and, if still empty,
                // poll briefly. Bounded at ~1.5 s — long enough for the
                // initial attribute event on a healthy LAN, short enough to
                // surface NoPeer quickly when there really is no robot.
                let robot = self.resolve_robot_identity().await?;
                let payload = identity.unwrap_or_default();
                self.perform_rpc(Some(&robot), SET_ACTIVE_OPERATOR_RPC, payload, None).await?;
                Ok(())
            }
        }
    }

    /// Currently-connected operator identities (excluding self).
    pub fn operators(&self) -> Vec<String> {
        let mut v: Vec<String> = self.controller.operators.lock().iter().cloned().collect();
        v.sort();
        v
    }

    /// Identity of the robot in the room, or `None` if none has been seen.
    /// Operator-side helper, derived from the robot's `lk.portal.role`
    /// attribute.
    pub fn robot_identity(&self) -> Option<String> {
        self.controller.robot_identity.lock().clone()
    }

    /// Fire when an operator joins the room. Identity is the new operator's
    /// participant identity. Only one callback is stored; subsequent calls
    /// overwrite.
    pub fn on_operator_joined(&self, callback: impl Fn(&str) + Send + Sync + 'static) {
        *self.controller.on_operator_joined.lock() = Some(Box::new(callback));
    }

    /// Fire when an operator leaves the room. The robot's `active_operator`
    /// attribute is **not** auto-cleared on disconnect; the pointer stays
    /// pinned so a reconnect with the same identity resumes control.
    pub fn on_operator_left(&self, callback: impl Fn(&str) + Send + Sync + 'static) {
        *self.controller.on_operator_left.lock() = Some(Box::new(callback));
    }

    /// Fire when the robot's `active_operator` attribute changes (or, on the
    /// Robot side, when the local pointer is updated via `set_active_operator`
    /// or the RPC handler). The argument is the new identity, or `None` if
    /// the pointer was cleared.
    pub fn on_active_operator_changed(
        &self,
        callback: impl Fn(Option<&str>) + Send + Sync + 'static,
    ) {
        *self.controller.on_active_operator_changed.lock() = Some(Box::new(callback));
    }

    /// Register an RPC method handler. Handlers can be registered before or
    /// after `connect()`; stored handlers are (re)applied to the
    /// `LocalParticipant` on each connect.
    pub fn register_rpc_method(&self, method: &str, handler: RpcHandler) {
        {
            let mut map = self.rpc_handlers.lock();
            map.insert(method.to_string(), handler.clone());
        }
        if let Some(lp) = self.local_participant.lock().clone() {
            register_handler_on(&lp, method.to_string(), handler);
        }
    }

    /// Remove a previously registered RPC method handler.
    pub fn unregister_rpc_method(&self, method: &str) {
        self.rpc_handlers.lock().remove(method);
        if let Some(lp) = self.local_participant.lock().clone() {
            lp.unregister_rpc_method(method.to_string());
        }
    }

    /// Invoke a registered method on the peer. `destination` is optional;
    /// when omitted, the call is routed to the obvious counterpart — robot
    /// for an Operator, the active operator for a Robot — falling back to
    /// the single remote participant if neither pointer is set yet. Errors
    /// with `NoPeer` or `AmbiguousPeer` when no unique destination
    /// resolves.
    pub async fn perform_rpc(
        &self,
        destination: Option<&str>,
        method: &str,
        payload: String,
        response_timeout: Option<Duration>,
    ) -> PortalResult<String> {
        let destination = match destination {
            Some(id) => id.to_string(),
            None => self.resolve_peer()?,
        };
        let lp = self.local_participant.lock().clone().ok_or(PortalError::NotConnected)?;

        let mut data = PerformRpcData {
            destination_identity: destination,
            method: method.to_string(),
            payload,
            ..Default::default()
        };
        if let Some(t) = response_timeout {
            data.response_timeout = t;
        }

        lp.perform_rpc(data).await.map_err(|e| PortalError::Rpc(e.into()))
    }

    /// Walk `room.remote_participants()` looking for one whose attributes
    /// declare `role=robot`. Synchronous one-shot lookup.
    fn find_robot_in_room(&self) -> Option<String> {
        let conn = self.conn.lock();
        let room = conn.room.as_ref()?;
        for (_sid, participant) in room.remote_participants() {
            let attrs = participant.attributes();
            if classify_role(&attrs) == Some(Role::Robot) {
                let id = participant.identity().as_str().to_string();
                // Cache for subsequent calls so the slow path runs at most
                // once per session.
                *self.controller.robot_identity.lock() = Some(id.clone());
                return Some(id);
            }
        }
        None
    }

    /// Resolve the robot's identity for an operator-side RPC. Tries the
    /// cached value first (populated by attribute events), then a synchronous
    /// scan of `room.remote_participants()`, then a short polling loop so
    /// `set_active_operator` works immediately after `connect()` without
    /// racing the initial attribute-propagation event. Returns `NoPeer`
    /// after the timeout if no participant with `role=robot` ever appears.
    async fn resolve_robot_identity(&self) -> PortalResult<String> {
        if let Some(id) = self.controller.robot_identity.lock().clone() {
            return Ok(id);
        }
        if let Some(id) = self.find_robot_in_room() {
            return Ok(id);
        }
        // Poll for ~1.5s in 50ms ticks. On a healthy LAN the first
        // ParticipantAttributesChanged event lands well within this window.
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(id) = self.controller.robot_identity.lock().clone() {
                return Ok(id);
            }
            if let Some(id) = self.find_robot_in_room() {
                return Ok(id);
            }
        }
        Err(PortalError::NoPeer)
    }

    /// Resolve a default destination for `perform_rpc(None, ...)`. Reads
    /// the multi-controller mirrors first (operator → robot, robot →
    /// active operator), then falls back to a single-remote-participant
    /// snapshot for setups that haven't designated control yet.
    fn resolve_peer(&self) -> PortalResult<String> {
        match self.config.role {
            Role::Operator => {
                if let Some(id) = self.controller.robot_identity.lock().clone() {
                    return Ok(id);
                }
            }
            Role::Robot => {
                if let Some(id) = self.controller.active_operator.lock().clone() {
                    return Ok(id);
                }
            }
        }
        let conn = self.conn.lock();
        let room = conn.room.as_ref().ok_or(PortalError::NotConnected)?;
        let remotes = room.remote_participants();
        match remotes.len() {
            0 => Err(PortalError::NoPeer),
            1 => {
                let (id, _) = remotes.into_iter().next().expect("remotes has one entry");
                Ok(id.as_str().to_string())
            }
            _ => Err(PortalError::AmbiguousPeer),
        }
    }

    /// Apply every stored handler to a freshly-connected LocalParticipant.
    /// Called once from `connect()` after the Room is up.
    fn apply_rpc_handlers(&self, lp: &LocalParticipant) {
        let handlers = self.rpc_handlers.lock().clone();
        for (method, handler) in handlers {
            register_handler_on(lp, method, handler);
        }
    }

    /// Reset Portal-side state written during a `connect()` that failed
    /// before reaching the final commit (where `conn.room` / `conn.event_task`
    /// would be stored). Mirrors the cleanup `disconnect()` does, except it
    /// (a) doesn't take the lifecycle lock — `connect()` already holds it —
    /// and (b) leaves the room handle for the caller to close, since the
    /// failing connect path holds it as a local. Without this, a failed
    /// connect would leave a stale `LocalParticipant` slot, RPC handler
    /// bindings on a dropped LP, and partial publisher maps that the next
    /// `connect()` (or any pre-connect getter) would still see.
    fn rollback_partial_connect(&self) {
        *self.local_participant.lock() = None;
        self.controller.clear();
        {
            let mut receivers = self.video_receivers.lock();
            for receiver in receivers.values() {
                receiver.abort();
            }
            receivers.clear();
        }
        self.video_publishers.lock().clear();
        self.frame_video_publishers.lock().clear();
        *self.state_publisher.lock() = None;
        *self.action_publisher.lock() = None;
        self.chunk_publishers.lock().clear();
        if let Some(sb) = self.sync_buffer.lock().take() {
            sb.lock().clear();
        }
        self.obs_sink.clear();
        self.action.clear();
        self.state.clear();
        for slot in self.chunk_slots.values() {
            slot.clear();
        }
        for slots in self.video_tracks.values() {
            slots.clear();
        }
    }

    pub async fn disconnect(&self) -> PortalResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let room = self.conn.lock().room.take();
        log::info!("disconnecting");

        // close() is best-effort; cleanup must happen even if it errors,
        // otherwise the Portal would be half-disconnected (room=None but
        // tasks/publishers still running) and the next connect() would race.
        let close_result = match room {
            Some(room) => room.close().await.map_err(|e| PortalError::Room(e.to_string())),
            None => Ok(()),
        };

        {
            let mut state = self.conn.lock();
            if let Some(task) = state.event_task.take() {
                task.abort();
            }
            state.rtt = None;
        }
        *self.local_participant.lock() = None;
        // Multi-controller state (operators, robot_identity, active_operator
        // mirror) is per-connection and cleared so a subsequent connect()
        // starts from a clean slate.
        self.controller.clear();
        {
            let mut receivers = self.video_receivers.lock();
            for receiver in receivers.values() {
                receiver.abort();
            }
            receivers.clear();
        }

        self.video_publishers.lock().clear();
        self.frame_video_publishers.lock().clear();
        *self.state_publisher.lock() = None;
        *self.action_publisher.lock() = None;
        self.chunk_publishers.lock().clear();

        if let Some(sb) = self.sync_buffer.lock().take() {
            sb.lock().clear();
        }
        self.obs_sink.clear();
        self.action.clear();
        self.state.clear();
        for slot in self.chunk_slots.values() {
            slot.clear();
        }
        for slots in self.video_tracks.values() {
            slots.clear();
        }

        close_result
    }

    // --- Pull API (latest-wins, peek semantics) ---

    /// Clone of the latest observation, or `None` if none received yet.
    /// Consumers wanting a history of observations should register
    /// `on_observation` and buffer on their own side.
    pub fn get_observation(&self) -> Option<Observation> {
        self.obs_sink.get()
    }

    /// Clone of the latest action received (Robot side), or `None`.
    /// `.values` holds typed values per the declared schema; `.raw_values`
    /// is the lossless `f64` view.
    pub fn get_action(&self) -> Option<Action> {
        self.action.get()
    }

    /// Clone of the latest state received (Operator side), or `None`.
    /// Typed per the declared schema.
    pub fn get_state(&self) -> Option<State> {
        self.state.get()
    }

    /// Clone of the latest frame received for `track_name`, or `None`.
    pub fn get_video_frame(&self, track_name: &str) -> Option<VideoFrameData> {
        self.video_tracks.get(track_name).and_then(|s| s.latest.lock().clone())
    }

    /// Clone of the latest chunk received for `chunk_name`, or `None` if
    /// none received yet (or the chunk wasn't declared).
    pub fn get_action_chunk(&self, chunk_name: &str) -> Option<ActionChunk> {
        self.chunk_slots.get(chunk_name).and_then(|s| s.get())
    }

    /// All declared action chunk schemas, in declaration order.
    pub fn action_chunks(&self) -> &[ChunkSpec] {
        self.config.action_chunks()
    }

    // --- Callback registration (push API) ---

    /// Fire on every received action. The `Action` record exposes typed
    /// values per the declared schema plus `raw_values` for the lossless
    /// `f64` view.
    pub fn on_action(&self, callback: impl Fn(&Action) + Send + Sync + 'static) {
        *self.action.cb.lock() = Some(Box::new(callback));
    }

    /// Fire on every received chunk for the named declaration. Only one
    /// callback per chunk; calling twice overwrites. Unknown names are
    /// logged and ignored — they aren't a hard error because the chunk
    /// schema may have been intentionally omitted on this peer.
    pub fn on_action_chunk(
        &self,
        chunk_name: &str,
        callback: impl Fn(&ActionChunk) + Send + Sync + 'static,
    ) {
        match self.chunk_slots.get(chunk_name) {
            Some(slot) => slot.set_callback(Box::new(callback)),
            None => log::warn!(
                "[unknown-chunk] on_action_chunk: chunk '{chunk_name}' not declared, callback ignored"
            ),
        }
    }

    pub fn on_observation(&self, callback: impl Fn(&Observation) + Send + Sync + 'static) {
        self.obs_sink.set_observation_cb(Box::new(callback));
    }

    /// Fire on every received state. Semantics mirror `on_action`.
    pub fn on_state(&self, callback: impl Fn(&State) + Send + Sync + 'static) {
        *self.state.cb.lock() = Some(Box::new(callback));
    }

    pub fn on_video_frame(
        &self,
        track_name: &str,
        callback: impl Fn(&str, &VideoFrameData) + Send + Sync + 'static,
    ) {
        match self.video_tracks.get(track_name) {
            Some(slots) => *slots.cb.lock() = Some(Box::new(callback)),
            None => log::warn!(
                "[unknown-track] on_video_frame: track '{track_name}' not registered, callback ignored"
            ),
        }
    }

    /// Fire on every batch of state samples that couldn't be matched to a
    /// video frame. Each entry is the typed state payload (same shape as
    /// `Observation.state`).
    pub fn on_drop(
        &self,
        callback: impl Fn(Vec<HashMap<String, TypedValue>>) + Send + Sync + 'static,
    ) {
        self.obs_sink.set_drop_cb(Box::new(callback));
    }

    // --- Internal ---

    async fn setup_robot(&self, room: &Room) -> PortalResult<()> {
        let lp = room.local_participant();

        for spec in &self.config.video_tracks {
            let track_name = &spec.name;
            let track_metrics =
                self.metrics.track(track_name).expect("track metrics registered at construction");
            let publisher = VideoPublisher::new(
                track_name,
                track_metrics,
                self.config.fps,
                spec.codec,
                spec.max_bitrate_kbps,
            );
            if let Err(e) = publisher.publish(&lp).await {
                // Roll back any earlier publishers so their send tasks stop
                // and connect() leaves Portal in a clean state.
                self.video_publishers.lock().clear();
                return Err(e);
            }
            log::info!("[{}] published video track '{track_name}'", self.config.session);
            self.video_publishers.lock().insert(track_name.clone(), Arc::new(publisher));
        }

        // Frame-video publishers don't go through `LocalParticipant.publish_track`
        // — they emit one byte stream per frame instead. So no async setup
        // here, just spawn the per-track drainer task.
        for spec in &self.config.frame_video_tracks {
            let track_metrics =
                self.metrics.track(&spec.name).expect("track metrics registered at construction");
            let publisher = FrameVideoPublisher::new(spec.clone(), lp.clone(), track_metrics);
            log::info!(
                "[{}] ready to publish frame-video track '{}' via byte stream (codec={:?}, quality={})",
                self.config.session,
                spec.name,
                spec.codec,
                spec.quality
            );
            self.frame_video_publishers.lock().insert(spec.name.clone(), Arc::new(publisher));
        }

        if !self.config.state_schema.is_empty() {
            let publisher = DataPublisher::new(
                &self.config.state_schema,
                STATE_TOPIC,
                self.config.state_reliable,
                lp.clone(),
                self.metrics.clone(),
                DataStream::State,
            );
            let mode = if self.config.state_reliable { "reliable" } else { "unreliable" };
            log::info!(
                "[{}] ready to publish state via {mode} data ({} fields)",
                self.config.session,
                self.config.state_schema.len()
            );
            *self.state_publisher.lock() = Some(Arc::new(publisher));
        }

        Ok(())
    }

    fn setup_operator(&self, room: &Room) {
        let lp = room.local_participant();

        // Sync buffer treats both transports the same way — it tracks frame
        // arrivals by name, regardless of whether they came from a WebRTC
        // RTP track or a frame-video byte stream. `all_track_names` was
        // computed once at construction.
        let sync_buffer = Arc::new(Mutex::new(SyncBuffer::new(
            &self.all_track_names,
            self.config.state_schema.clone(),
            self.config.sync_config(),
            self.metrics.clone(),
        )));
        *self.sync_buffer.lock() = Some(sync_buffer);

        if !self.config.action_schema.is_empty() {
            let mode = if self.config.action_reliable { "reliable" } else { "unreliable" };
            log::info!(
                "[{}] ready to publish action via {mode} data ({} fields)",
                self.config.session,
                self.config.action_schema.len()
            );
            let publisher = DataPublisher::new(
                &self.config.action_schema,
                ACTION_TOPIC,
                self.config.action_reliable,
                lp.clone(),
                self.metrics.clone(),
                DataStream::Action,
            );
            *self.action_publisher.lock() = Some(Arc::new(publisher));
        }

        if !self.config.action_chunks.is_empty() {
            for spec in &self.config.action_chunks {
                log::info!(
                    "[{}] ready to publish chunk '{}' via byte stream (horizon={}, {} fields)",
                    self.config.session,
                    spec.name,
                    spec.horizon,
                    spec.fields.len()
                );
                let publisher = ChunkPublisher::new(spec.clone(), lp.clone(), self.metrics.clone());
                self.chunk_publishers.lock().insert(spec.name.clone(), Arc::new(publisher));
            }
        }
    }

    /// Snapshot of metrics since construction or the last `reset_metrics()`.
    pub fn metrics(&self) -> PortalMetrics {
        let (video_fill, state_fill) = match self.sync_buffer.lock().as_ref() {
            Some(sb) => {
                let sb = sb.lock();
                (sb.video_fill_snapshot(), sb.state_fill())
            }
            None => (HashMap::new(), 0),
        };
        self.metrics.snapshot(video_fill, state_fill)
    }

    pub fn reset_metrics(&self) {
        self.metrics.reset();
    }
}

/// Wrap a Portal `RpcHandler` in the signature the SDK expects and install
/// it on the given LocalParticipant. Payload types are converted at the
/// boundary — the SDK's `RpcInvocationData` / `RpcError` never leak into
/// caller-facing code.
/// Names of every video track on a config, regardless of transport. Used
/// when registering metrics and sync-buffer slots, since the consumer-facing
/// API doesn't distinguish WebRTC and frame-video tracks.
fn combined_track_names(config: &PortalConfig) -> Vec<String> {
    let mut names: Vec<String> = config.video_tracks.iter().map(|s| s.name.clone()).collect();
    names.extend(config.frame_video_tracks.iter().map(|s| s.name.clone()));
    names
}

fn register_handler_on(lp: &LocalParticipant, method: String, handler: RpcHandler) {
    lp.register_rpc_method(method, move |data| {
        let handler = handler.clone();
        Box::pin(async move {
            let core_data: crate::rpc::RpcInvocationData = data.into();
            handler(core_data).await.map_err(Into::into)
        })
    });
}

/// Snapshot of the fields the room event loop needs, so it doesn't take any
/// Portal-level lock on the hot path.
struct EventContext {
    config: PortalConfig,
    /// Cached schema fingerprints so the receive hot path doesn't recompute
    /// them per packet. Matches the peer's fingerprint when schemas agree;
    /// a mismatch logs once per offending value and drops the packet.
    action_schema_fp: u32,
    state_schema_fp: u32,
    sync_buffer: Option<Arc<Mutex<SyncBuffer>>>,
    obs_sink: Arc<ObservationSink>,
    action: Arc<ActionSlot>,
    state: Arc<StateSlot>,
    chunk_slots: Vec<Arc<ChunkSlot>>,
    unknown_chunk_fp_warns: Arc<Mutex<HashSet<u32>>>,
    video_tracks: HashMap<String, Arc<VideoTrackSlots>>,
    video_receivers: Arc<Mutex<HashMap<String, VideoReceiver>>>,
    /// Frame-video entries (spec + slots + metrics fused) keyed by track
    /// name. Shared as `Arc<HashMap>` so per-frame fan-out into spawn
    /// tasks bumps a refcount instead of cloning the map.
    frame_video_entries: Arc<HashMap<String, Arc<FrameVideoTrackEntry>>>,
    metrics: Arc<MetricsRegistry>,
    rtt: Arc<RttService>,
    /// Multi-controller state, shared with `Portal` so attribute and
    /// participant lifecycle events can update it directly without going
    /// through the Portal struct.
    controller: Arc<ControllerState>,
    /// Cached at connect time. Used to skip self when classifying participants
    /// observed via `ParticipantConnected` / `ParticipantAttributesChanged` —
    /// our own attribute updates also fire these events on the local participant.
    local_identity: String,
}

/// Classify a remote participant by their `lk.portal.role` attribute and
/// reconcile controller state. Idempotent: re-observing the same participant
/// does not re-fire `on_operator_joined`. Used by the connect-time snapshot
/// and the ongoing `ParticipantConnected` / `ParticipantAttributesChanged`
/// handlers.
fn classify_and_update(
    controller: &ControllerState,
    self_role: Role,
    identity: &ParticipantIdentity,
    attrs: &HashMap<String, String>,
) {
    let id = identity.as_str().to_string();
    match classify_role(attrs) {
        Some(Role::Robot) => {
            {
                let mut slot = controller.robot_identity.lock();
                if slot.as_deref() != Some(id.as_str()) {
                    *slot = Some(id.clone());
                }
            }
            // Operator-side: mirror the robot's `active_operator` attribute.
            if self_role == Role::Operator {
                let new_value = attrs
                    .get(ACTIVE_OPERATOR_ATTR_KEY)
                    .and_then(|v| if v.is_empty() { None } else { Some(v.clone()) });
                let mut slot = controller.active_operator.lock();
                if *slot != new_value {
                    *slot = new_value.clone();
                    drop(slot);
                    controller.fire_active_changed(new_value.as_deref());
                }
            }
        }
        Some(Role::Operator) => {
            let inserted = controller.operators.lock().insert(id.clone());
            if inserted {
                controller.fire_op_joined(&id);
            }
        }
        None => {
            // Role attribute not yet visible; wait for a follow-up
            // ParticipantAttributesChanged event.
        }
    }
}

/// Implementation of the `portal.set_active_operator` RPC, registered on the
/// Robot side at connect. Anyone in the room may call this; payload is the
/// new identity (or empty string to clear). The handler updates the local
/// pointer and the broadcast attribute, then fires
/// `on_active_operator_changed` if the value actually moved.
async fn set_active_operator_rpc_impl(
    lp_slot: &Mutex<Option<LocalParticipant>>,
    controller: &ControllerState,
    data: RpcInvocationData,
) -> Result<String, RpcError> {
    let identity = if data.payload.is_empty() { None } else { Some(data.payload.clone()) };
    let lp = lp_slot.lock().clone();
    let Some(lp) = lp else {
        return Err(RpcError::new(RPC_NOT_CONNECTED, "robot not connected", None));
    };
    let prev = controller.active_operator.lock().clone();
    let mut attrs = HashMap::new();
    attrs.insert(ACTIVE_OPERATOR_ATTR_KEY.to_string(), identity.clone().unwrap_or_default());
    if let Err(e) = lp.set_attributes(attrs).await {
        return Err(RpcError::new(
            RPC_SET_ATTRIBUTES_FAILED,
            format!("set_attributes failed: {e}"),
            None,
        ));
    }
    *controller.active_operator.lock() = identity.clone();
    if prev != identity {
        controller.fire_active_changed(identity.as_deref());
    }
    Ok(String::new())
}

fn handle_room_event(ctx: &EventContext, event: RoomEvent) {
    match event {
        RoomEvent::TrackSubscribed { track, publication, .. } => {
            if ctx.config.role != Role::Operator {
                return;
            }
            if let RemoteTrack::Video(video_track) = track {
                let track_name = publication.name();
                if ctx.config.video_tracks.iter().any(|s| s.name == track_name) {
                    log::info!("[{}] subscribed to video track '{track_name}'", ctx.config.session);
                    if let Some(sync_buffer) = &ctx.sync_buffer {
                        let slots = ctx
                            .video_tracks
                            .get(track_name.as_str())
                            .cloned()
                            .unwrap_or_else(|| Arc::new(VideoTrackSlots::new()));
                        let track_metrics = ctx
                            .metrics
                            .track(track_name.as_str())
                            .expect("track metrics registered at construction");

                        let stream = NativeVideoStream::new(video_track.rtc_track());
                        let receiver = VideoReceiver::spawn(
                            track_name.to_string(),
                            stream,
                            sync_buffer.clone(),
                            slots,
                            ctx.obs_sink.clone(),
                            track_metrics,
                        );
                        ctx.video_receivers.lock().insert(track_name.to_string(), receiver);
                    }
                }
            }
        }
        RoomEvent::DataReceived { payload, topic: Some(topic), participant, .. } => {
            // Active-operator gate. Drop incoming actions whose sender does
            // not match `active_operator`. Applies to both the robot (always
            // processes ACTION_TOPIC) and operators with subscription on
            // (recorders, shadow eval, live monitoring). Operators without
            // subscription short-circuit before the deserialize so the
            // receive hot path costs nothing for the common controller-only
            // case. Non-action topics (state, RTT) bypass the gate and pass
            // an empty sender — those records don't carry a sender field.
            let gate_sender: String = match (ctx.config.role, topic.as_str()) {
                (Role::Robot, ACTION_TOPIC) => {
                    let Some(p) = &participant else {
                        return;
                    };
                    let sender_id = p.identity().as_str().to_string();
                    let active = ctx.controller.active_operator.lock().clone();
                    if active.as_deref() != Some(sender_id.as_str()) {
                        return;
                    }
                    sender_id
                }
                (Role::Operator, ACTION_TOPIC) => {
                    if !ctx.config.action_subscription {
                        return;
                    }
                    let Some(p) = &participant else {
                        return;
                    };
                    let sender_id = p.identity().as_str().to_string();
                    let active = ctx.controller.active_operator.lock().clone();
                    if active.as_deref() != Some(sender_id.as_str()) {
                        return;
                    }
                    sender_id
                }
                _ => String::new(),
            };
            let output = handle_data_received(
                &payload,
                &topic,
                ctx.config.role,
                &ctx.config.action_schema,
                ctx.action_schema_fp,
                &ctx.config.state_schema,
                ctx.state_schema_fp,
                &ctx.action,
                &ctx.state,
                ctx.sync_buffer.as_ref(),
                &ctx.metrics,
                &ctx.rtt,
                gate_sender,
            );
            if !output.is_empty() {
                ctx.obs_sink.dispatch(output);
            }
        }
        RoomEvent::ByteStreamOpened { reader, topic, participant_identity } => {
            // Two Portal byte-stream topics, each owned by a different role:
            //   * `portal_action_chunk` — operator → robot. Action chunks
            //     too big to fit in a 15 KB data packet.
            //   * `portal_frame_video`  — robot → operator. Per-frame
            //     RGB/PNG/MJPEG payloads that bypass the WebRTC media path.
            // We `take_if` on the topic so this Portal only consumes streams
            // it owns; other applications using byte streams on unrelated
            // topics are left untouched.
            match (ctx.config.role, topic.as_str()) {
                (Role::Robot, ACTION_CHUNK_TOPIC) | (Role::Operator, ACTION_CHUNK_TOPIC) => {
                    // Robot always consumes chunks. Operators only consume
                    // when subscription is on (HITL recording, shadow eval).
                    // Bail out early on operators with subscription off so
                    // we never spawn the read task.
                    if matches!(ctx.config.role, Role::Operator) && !ctx.config.action_subscription
                    {
                        return;
                    }
                    let Some(reader) = reader.take_if(|info| info.topic == ACTION_CHUNK_TOPIC)
                    else {
                        return;
                    };
                    let chunk_slots = ctx.chunk_slots.clone();
                    let unknown_fp_warns = ctx.unknown_chunk_fp_warns.clone();
                    let metrics = ctx.metrics.clone();
                    let controller = ctx.controller.clone();
                    let sender_id = participant_identity.as_str().to_string();
                    tokio::spawn(async move {
                        use livekit::StreamReader;
                        match reader.read_all().await {
                            Ok(payload) => {
                                // Apply the active-operator gate at delivery
                                // time. Sender at delivery wins; a chunk
                                // started under one operator and finishing
                                // under another is dropped if the new active
                                // is different.
                                let active = controller.active_operator.lock().clone();
                                if active.as_deref() != Some(sender_id.as_str()) {
                                    return;
                                }
                                dispatch_chunk_payload(
                                    &payload,
                                    &chunk_slots,
                                    &unknown_fp_warns,
                                    &metrics,
                                    sender_id,
                                )
                            }
                            Err(e) => {
                                log::warn!("[bad-payload] failed to read chunk byte stream: {e}")
                            }
                        }
                    });
                }
                (Role::Operator, FRAME_VIDEO_TOPIC) => {
                    // Operator-side: each byte stream carries one frame for
                    // some declared frame-video track. The header in the
                    // payload routes it to the right entry (spec + slots
                    // + metrics fused; one HashMap lookup at dispatch).
                    if ctx.frame_video_entries.is_empty() {
                        return;
                    }
                    let Some(reader) = reader.take_if(|info| info.topic == FRAME_VIDEO_TOPIC)
                    else {
                        return;
                    };
                    let Some(sync_buffer) = ctx.sync_buffer.clone() else {
                        return;
                    };
                    let _ = participant_identity;
                    // Refcount bumps only — no map or HashMap clone.
                    let entries = ctx.frame_video_entries.clone();
                    let obs_sink = ctx.obs_sink.clone();
                    tokio::spawn(async move {
                        use livekit::StreamReader;
                        match reader.read_all().await {
                            // `Bytes::from(Vec)` is a move. Subsequent
                            // `Bytes::slice(...)` in the dispatch path is a
                            // refcount bump, so the `Raw` codec gets a
                            // zero-copy view of the wire payload all the
                            // way to `VideoFrameData.data`.
                            Ok(payload) => dispatch_frame_payload(
                                payload,
                                &entries,
                                &sync_buffer,
                                &obs_sink,
                            ),
                            Err(e) => log::warn!(
                                "[bad-payload] failed to read frame_video byte stream: {e}"
                            ),
                        }
                    });
                }
                _ => {}
            }
        }
        RoomEvent::ParticipantConnected(participant) => {
            // Snapshot the peer's attributes once they are visible. We may
            // observe an empty attribute map if the new participant has not
            // yet completed their `set_attributes` call; the
            // `ParticipantAttributesChanged` event will reclassify them when
            // the role attribute lands.
            let identity = participant.identity();
            let attrs = participant.attributes();
            classify_and_update(&ctx.controller, ctx.config.role, &identity, &attrs);
        }
        RoomEvent::ParticipantAttributesChanged { participant, .. } => {
            let identity = participant.identity();
            // Skip our own attribute updates: when we self-set `role` /
            // `active_operator`, the SDK echoes the change back through this
            // event for the local participant.
            if identity.as_str() == ctx.local_identity {
                return;
            }
            let attrs = participant.attributes();
            classify_and_update(&ctx.controller, ctx.config.role, &identity, &attrs);
        }
        RoomEvent::ParticipantDisconnected(participant) => {
            // Multi-controller bookkeeping. The `active_operator` pointer
            // stays pinned by design (see spec.md §Defaults: "stays pinned");
            // a same-identity reconnect resumes control, a different operator
            // claims explicitly via `set_active_operator`.
            let identity = participant.identity();
            let id_str = identity.as_str().to_string();
            log::info!("[{}] participant '{}' disconnected", ctx.config.session, id_str);
            if ctx.controller.operators.lock().remove(&id_str) {
                ctx.controller.fire_op_left(&id_str);
            }
            let mut robot_slot = ctx.controller.robot_identity.lock();
            if robot_slot.as_deref() == Some(id_str.as_str()) {
                *robot_slot = None;
            }
        }
        RoomEvent::Reconnected => {
            log::info!(
                "[{}] reconnected, clearing sync buffers and latest slots",
                ctx.config.session
            );
            if let Some(sb) = &ctx.sync_buffer {
                sb.lock().clear();
            }
            // Pre-reconnect data is stale by definition; consumers calling
            // get_* after a reconnect should see None until fresh packets
            // arrive, matching the semantics already applied to sync_buffer.
            ctx.obs_sink.clear();
            ctx.action.clear();
            ctx.state.clear();
            for slot in &ctx.chunk_slots {
                slot.clear();
            }
            for slots in ctx.video_tracks.values() {
                slots.clear();
            }
            // Reset the per-room rosters but keep `active_operator` pinned —
            // the robot has no self-event to re-read its own attribute, and
            // the operator-side mirror gets reseeded by the post-reconnect
            // `ParticipantConnected` for the robot (idempotent on equal
            // values). Clearing it here would silently stall control on the
            // robot side across any transient reconnect.
            ctx.controller.clear_for_reconnect();
        }
        _ => {}
    }
}
