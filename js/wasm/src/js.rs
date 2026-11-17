// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The plain JS shapes that cross the boundary. Outputs are tagged objects
//! (`type` for outputs, `kind` for events) that the TS driver switches on.

use std::collections::HashMap;

use livekit_portal_core::config::FieldSpec;
use livekit_portal_core::livekit;
use livekit_portal_core::metrics::PortalMetrics;
use livekit_portal_core::rpc::RpcError;
use livekit_portal_core::transport::Delivery;
use livekit_portal_core::types::{FrameSource, TimeSyncSource, TypedValue};
use livekit_portal_core::{Event, Output};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use wasm_bindgen::prelude::*;

use crate::frame::WebFrame;

pub fn to_value<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    // Maps become objects and `None` becomes `null`. Not `json_compatible()`,
    // which would turn byte buffers into number arrays instead of `Uint8Array`.
    let serializer = serde_wasm_bindgen::Serializer::new()
        .serialize_maps_as_objects(true)
        .serialize_missing_as_null(true);
    value.serialize(&serializer).map_err(|e| JsError::new(&e.to_string()))
}

pub fn from_value<T: for<'de> Deserialize<'de>>(value: JsValue) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(value).map_err(|e| JsError::new(&e.to_string()))
}

/// A field value as JS spells it.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Num(f64),
}

impl From<TypedValue> for Value {
    fn from(v: TypedValue) -> Self {
        match v {
            TypedValue::Bool(b) => Value::Bool(b),
            other => Value::Num(other.as_f64()),
        }
    }
}

type Values = HashMap<String, Value>;

fn values(typed: HashMap<String, TypedValue>) -> Values {
    typed.into_iter().map(|(k, v)| (k, v.into())).collect()
}

/// Type JS values per the declared schema. A field the schema doesn't name
/// goes through as `f64` for the core to reject with its own message.
pub fn typed(input: Values, schema: &[FieldSpec]) -> HashMap<String, TypedValue> {
    input
        .into_iter()
        .map(|(name, value)| {
            let v = match value {
                Value::Bool(b) => f64::from(u8::from(b)),
                Value::Num(n) => n,
            };
            let typed = match schema.iter().find(|f| f.name == name) {
                Some(f) => TypedValue::from_f64(v, f.dtype),
                None => TypedValue::F64(v),
            };
            (name, typed)
        })
        .collect()
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum JsOutput {
    Publish { topic: &'static str, payload: ByteBuf, reliable: bool, to: Option<String> },
    SetAttributes { id: Option<u64>, attributes: HashMap<String, String> },
    Rpc { id: u64, to: String, method: String, payload: String, timeout_ms: Option<u32> },
    RpcReply { id: u64, result: RpcResult },
    Event { event: JsEvent },
    Wake { at_us: Option<u64> },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcResult {
    pub ok: Option<String>,
    pub error: Option<JsRpcError>,
}

#[derive(Serialize, Deserialize)]
pub struct JsRpcError {
    pub code: u32,
    pub message: String,
    pub data: Option<String>,
}

impl From<Result<String, RpcError>> for RpcResult {
    fn from(result: Result<String, RpcError>) -> Self {
        match result {
            Ok(ok) => Self { ok: Some(ok), error: None },
            Err(e) => Self {
                ok: None,
                error: Some(JsRpcError { code: e.code, message: e.message, data: e.data }),
            },
        }
    }
}

impl From<RpcResult> for Result<String, RpcError> {
    fn from(result: RpcResult) -> Self {
        match result.error {
            Some(e) => Err(RpcError::new(e.code, e.message, e.data)),
            None => Ok(result.ok.unwrap_or_default()),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum JsEvent {
    Action {
        values: Values,
        raw_values: HashMap<String, f64>,
        timestamp_us: u64,
        in_reply_to_ts_us: Option<u64>,
        sender: String,
        active: bool,
    },
    State {
        values: Values,
        raw_values: HashMap<String, f64>,
        timestamp_us: u64,
    },
    Telemetry {
        values: Values,
        raw_values: HashMap<String, f64>,
        timestamp_us: u64,
        sender: String,
    },
    VideoFrame {
        track: String,
        frame: JsFrame,
    },
    Observation {
        state: Values,
        raw_state: HashMap<String, f64>,
        frames: HashMap<String, JsFrame>,
        timestamp_us: u64,
    },
    Drops {
        states: Vec<Values>,
    },
    Keypoint {
        #[serde(rename = "type")]
        kind: String,
        payload: serde_json::Map<String, serde_json::Value>,
        timestamp_us: u64,
        sender: String,
    },
    TimeSynced,
    OperatorJoined {
        identity: String,
    },
    OperatorLeft {
        identity: String,
    },
    ActiveOperatorChanged {
        identity: Option<String>,
    },
    RpcResult {
        id: u64,
        result: RpcResult,
    },
    RpcRequest {
        id: u64,
        from: String,
        method: String,
        payload: String,
    },
}

/// A frame as JS sees it: a `handle` to a browser `VideoFrame` the driver
/// holds, or decoded RGB `data`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsFrame {
    handle: Option<u32>,
    data: Option<ByteBuf>,
    width: u32,
    height: u32,
    timestamp_us: u64,
    source: &'static str,
}

impl From<WebFrame> for JsFrame {
    fn from(frame: WebFrame) -> Self {
        match frame {
            WebFrame::Pixels(f) => Self {
                handle: None,
                data: Some(ByteBuf::from(f.data.to_vec())),
                width: f.width,
                height: f.height,
                timestamp_us: f.timestamp_us,
                source: source_name(f.source),
            },
            WebFrame::Handle(h) => Self {
                handle: Some(h.id),
                data: None,
                width: h.width,
                height: h.height,
                timestamp_us: h.timestamp_us,
                source: source_name(h.source),
            },
        }
    }
}

fn source_name(source: FrameSource) -> &'static str {
    match source {
        FrameSource::Live => "live",
        FrameSource::Stale => "stale",
        FrameSource::Omitted => "omitted",
    }
}

impl From<Event<WebFrame>> for JsEvent {
    fn from(event: Event<WebFrame>) -> Self {
        match event {
            Event::Action(a) => JsEvent::Action {
                values: values(a.values),
                raw_values: a.raw_values,
                timestamp_us: a.timestamp_us,
                in_reply_to_ts_us: a.in_reply_to_ts_us,
                sender: a.sender,
                active: a.active,
            },
            Event::State(s) => JsEvent::State {
                values: values(s.values),
                raw_values: s.raw_values,
                timestamp_us: s.timestamp_us,
            },
            Event::Telemetry(t) => JsEvent::Telemetry {
                values: values(t.values),
                raw_values: t.raw_values,
                timestamp_us: t.timestamp_us,
                sender: t.sender,
            },
            Event::VideoFrame { track, frame } => {
                JsEvent::VideoFrame { track, frame: frame.into() }
            }
            Event::Observation(o) => JsEvent::Observation {
                state: values(o.state),
                raw_state: o.raw_state,
                frames: o.frames.into_iter().map(|(k, f)| (k, f.into())).collect(),
                timestamp_us: o.timestamp_us,
            },
            Event::Drops(states) => {
                JsEvent::Drops { states: states.into_iter().map(values).collect() }
            }
            Event::Keypoint(k) => JsEvent::Keypoint {
                kind: k.kind,
                payload: k.payload,
                timestamp_us: k.timestamp_us,
                sender: k.sender,
            },
            Event::TimeSynced => JsEvent::TimeSynced,
            Event::OperatorJoined(identity) => JsEvent::OperatorJoined { identity },
            Event::OperatorLeft(identity) => JsEvent::OperatorLeft { identity },
            Event::ActiveOperatorChanged(identity) => JsEvent::ActiveOperatorChanged { identity },
            Event::RpcResult { id, result } => {
                JsEvent::RpcResult { id: id.0, result: result.into() }
            }
            Event::RpcRequest { id, from, method, payload } => {
                JsEvent::RpcRequest { id: id.0, from, method, payload }
            }
        }
    }
}

impl JsOutput {
    /// Map a core output onto LiveKit, the way the native driver does.
    /// `None` for a stream LiveKit carries natively (control, presence),
    /// which the core never publishes under LiveKit's caps.
    pub fn from_core(output: Output<WebFrame>) -> Option<Self> {
        Some(match output {
            Output::Publish { stream, payload, delivery, to } => {
                let Some(topic) = livekit::topic(stream) else {
                    log::warn!("[publish-failed] stream '{}' has no LiveKit topic", stream.name());
                    return None;
                };
                JsOutput::Publish {
                    topic,
                    payload: ByteBuf::from(payload),
                    reliable: delivery == Delivery::Reliable,
                    to,
                }
            }
            Output::SetPresence { id, presence } => JsOutput::SetAttributes {
                id: id.map(|id| id.0),
                attributes: livekit::active_operator_attributes(&presence),
            },
            Output::Rpc { id, to, method, payload, timeout_ms } => {
                JsOutput::Rpc { id: id.0, to, method, payload, timeout_ms }
            }
            Output::RpcReply { id, result } => {
                JsOutput::RpcReply { id: id.0, result: result.into() }
            }
            Output::Emit(event) => JsOutput::Event { event: event.into() },
            Output::ScheduleWake { at_us } => JsOutput::Wake { at_us },
        })
    }
}

pub fn outputs(out: Vec<Output<WebFrame>>) -> Result<JsValue, JsError> {
    let out: Vec<JsOutput> = out.into_iter().filter_map(JsOutput::from_core).collect();
    to_value(&out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    sync: SyncMetrics,
    transport: TransportMetrics,
    buffers: BufferMetrics,
    rtt: RttMetrics,
    time_sync: TimeSyncMetrics,
    policy: PolicyMetrics,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncMetrics {
    observations_emitted: u64,
    stale_observations_emitted: u64,
    states_dropped: u64,
    frames_omitted: HashMap<String, u64>,
    match_delta_us_p50: Option<u64>,
    match_delta_us_p95: Option<u64>,
    last_blocker_track: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TransportMetrics {
    frames_sent: HashMap<String, u64>,
    frames_received: HashMap<String, u64>,
    frames_dropped_publisher_full: HashMap<String, u64>,
    bytes_sent: HashMap<String, u64>,
    bytes_received: HashMap<String, u64>,
    states_sent: u64,
    states_received: u64,
    actions_sent: u64,
    actions_received: u64,
    actions_dropped_stale: u64,
    telemetry_sent: u64,
    telemetry_received: u64,
    frame_jitter_us: HashMap<String, u64>,
    state_jitter_us: u64,
    action_jitter_us: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BufferMetrics {
    video_fill: HashMap<String, usize>,
    state_fill: usize,
    evictions: HashMap<String, u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RttMetrics {
    rtt_us_last: Option<u64>,
    rtt_us_mean: Option<u64>,
    rtt_us_p95: Option<u64>,
    pings_sent: u64,
    pongs_received: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TimeSyncMetrics {
    source: &'static str,
    synced: bool,
    offset_us: i64,
    uncertainty_us: Option<u64>,
    measured_offset_us: Option<i64>,
    resyncs: u64,
    samples_rejected: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PolicyMetrics {
    e2e_us_p50: Option<u64>,
    e2e_us_p95: Option<u64>,
    correlated_received: u64,
}

impl From<PortalMetrics> for Metrics {
    fn from(m: PortalMetrics) -> Self {
        let PortalMetrics { sync, transport, buffers, rtt, time_sync, policy } = m;
        Self {
            sync: SyncMetrics {
                observations_emitted: sync.observations_emitted,
                stale_observations_emitted: sync.stale_observations_emitted,
                states_dropped: sync.states_dropped,
                frames_omitted: sync.frames_omitted,
                match_delta_us_p50: sync.match_delta_us_p50,
                match_delta_us_p95: sync.match_delta_us_p95,
                last_blocker_track: sync.last_blocker_track,
            },
            transport: TransportMetrics {
                frames_sent: transport.frames_sent,
                frames_received: transport.frames_received,
                frames_dropped_publisher_full: transport.frames_dropped_publisher_full,
                bytes_sent: transport.bytes_sent,
                bytes_received: transport.bytes_received,
                states_sent: transport.states_sent,
                states_received: transport.states_received,
                actions_sent: transport.actions_sent,
                actions_received: transport.actions_received,
                actions_dropped_stale: transport.actions_dropped_stale,
                telemetry_sent: transport.telemetry_sent,
                telemetry_received: transport.telemetry_received,
                frame_jitter_us: transport.frame_jitter_us,
                state_jitter_us: transport.state_jitter_us,
                action_jitter_us: transport.action_jitter_us,
            },
            buffers: BufferMetrics {
                video_fill: buffers.video_fill,
                state_fill: buffers.state_fill,
                evictions: buffers.evictions,
            },
            rtt: RttMetrics {
                rtt_us_last: rtt.rtt_us_last,
                rtt_us_mean: rtt.rtt_us_mean,
                rtt_us_p95: rtt.rtt_us_p95,
                pings_sent: rtt.pings_sent,
                pongs_received: rtt.pongs_received,
            },
            time_sync: TimeSyncMetrics {
                source: match time_sync.source {
                    TimeSyncSource::Portal => "portal",
                    TimeSyncSource::System => "system",
                },
                synced: time_sync.synced,
                offset_us: time_sync.offset_us,
                uncertainty_us: time_sync.uncertainty_us,
                measured_offset_us: time_sync.measured_offset_us,
                resyncs: time_sync.resyncs,
                samples_rejected: time_sync.samples_rejected,
            },
            policy: PolicyMetrics {
                e2e_us_p50: policy.e2e_us_p50,
                e2e_us_p95: policy.e2e_us_p95,
                correlated_received: policy.correlated_received,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use livekit_portal_core::dtype::DType;

    use super::*;

    #[test]
    fn js_values_take_the_declared_dtype() {
        let schema = [FieldSpec::new("g", DType::Bool), FieldSpec::new("m", DType::I8)];
        let input = HashMap::from([
            ("g".to_string(), Value::Bool(true)),
            ("m".to_string(), Value::Num(3.0)),
            ("x".to_string(), Value::Num(0.5)),
        ]);
        let typed = typed(input, &schema);
        assert_eq!(typed["g"], TypedValue::Bool(true));
        assert_eq!(typed["m"], TypedValue::I8(3));
        assert_eq!(typed["x"], TypedValue::F64(0.5));
    }

    #[test]
    fn rpc_results_round_trip() {
        let err: Result<String, RpcError> = Err(RpcError::new(1401, "nope", None));
        let back: Result<String, RpcError> = RpcResult::from(err).into();
        assert_eq!(back.unwrap_err().code, 1401);
        let back: Result<String, RpcError> = RpcResult::from(Ok("x".to_string())).into();
        assert_eq!(back.unwrap(), "x");
    }
}
