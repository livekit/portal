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

use std::collections::HashMap;

use bytes::Bytes;
use livekit_portal_core::frame_video::{FrameVideoTracks, RgbFrame, build_frame_payload};
use livekit_portal_core::livekit;
use livekit_portal_core::types::FrameSource;
use livekit_portal_core::{
    Connect, CoreInit, Input, OpId, Output, PortalCore, RpcId, RpcRequest, SendAction,
    SendKeypoint, SendState, SendTelemetry, SendVideoFrame, SetActiveOperator, Step,
    VideoTransport,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use wasm_bindgen::prelude::*;

use crate::config::Config;
use crate::frame::{FrameHandle, WebFrame};
use crate::js::{self, Metrics, RpcResult, Value, outputs};

type Attributes = HashMap<String, String>;

/// The sans-IO Portal core. Every method takes the local raw time and
/// returns the outputs for the driver to carry out.
#[wasm_bindgen]
pub struct Core {
    inner: PortalCore<WebFrame>,
    frame_video: FrameVideoTracks,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Peer {
    identity: String,
    attributes: Attributes,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectInput {
    local_identity: String,
    local_attributes: Attributes,
    peers: Vec<Peer>,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerInput {
    identity: String,
    #[serde(default)]
    attributes: Attributes,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataInput {
    topic: String,
    from: Option<String>,
    payload: ByteBuf,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameBytesInput {
    payload: ByteBuf,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoFrameInput {
    track: String,
    handle: u32,
    width: u32,
    height: u32,
    timestamp_us: f64,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcRequestInput {
    id: u64,
    method: String,
    from: String,
    payload: String,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcResponseInput {
    id: u64,
    result: RpcResult,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceSetInput {
    id: u64,
    error: Option<String>,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendValuesInput {
    values: HashMap<String, Value>,
    timestamp_us: Option<f64>,
    in_reply_to_ts_us: Option<f64>,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendKeypointInput {
    #[serde(rename = "type")]
    kind: String,
    payload: serde_json::Map<String, serde_json::Value>,
    timestamp_us: Option<f64>,
    now_us: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendVideoFrameInput {
    track: String,
    timestamp_us: Option<f64>,
    now_us: f64,
    /// RGB24 pixels, read only for byte-frame tracks.
    rgb: Option<ByteBuf>,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetActiveOperatorInput {
    identity: Option<String>,
    now_us: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VideoRouteOutput {
    timestamp_us: u64,
    /// `"transport"`: hand the frame to the published WebRTC track.
    /// `"bytes"`: send `payload` on the frame-video byte stream.
    transport: &'static str,
    payload: Option<ByteBuf>,
}

fn us(t: f64) -> u64 {
    t as u64
}

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

#[wasm_bindgen]
impl Core {
    #[wasm_bindgen(constructor)]
    pub fn new(config: &Config) -> Core {
        let config = config.inner.clone();
        Core {
            frame_video: FrameVideoTracks::new(&config),
            inner: PortalCore::new(CoreInit { config, caps: livekit::CAPS }),
        }
    }

    pub fn connect(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let ConnectInput { local_identity, local_attributes, peers, now_us } =
            js::from_value(input)?;
        let out = self
            .inner
            .connect(Connect {
                local_peer: local_identity,
                active_operator: livekit::active_operator(&local_attributes),
                peers: peers
                    .into_iter()
                    .map(|p| (p.identity, livekit::presence(&p.attributes)))
                    .collect(),
                now_us: us(now_us),
            })
            .map_err(err)?;
        outputs(out)
    }

    pub fn disconnect(&mut self) -> Result<JsValue, JsError> {
        outputs(self.inner.disconnect())
    }

    /// Attributes this peer sets on itself at connect.
    #[wasm_bindgen(js_name = roleAttributes)]
    pub fn role_attributes(&self) -> Result<JsValue, JsError> {
        js::to_value(&livekit::role_attributes(self.inner.config().role()))
    }

    /// A participant connected, with whatever attributes it has so far.
    #[wasm_bindgen(js_name = peerJoined)]
    pub fn peer_joined(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let PeerInput { identity, attributes, now_us } = js::from_value(input)?;
        let mut out = self.step(Input::PeerJoined { peer: &identity }, now_us);
        let presence = livekit::presence(&attributes);
        out.extend(self.step(Input::PresenceChanged { peer: &identity, presence }, now_us));
        outputs(out)
    }

    #[wasm_bindgen(js_name = attributesChanged)]
    pub fn attributes_changed(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let PeerInput { identity, attributes, now_us } = js::from_value(input)?;
        let presence = livekit::presence(&attributes);
        outputs(self.step(Input::PresenceChanged { peer: &identity, presence }, now_us))
    }

    #[wasm_bindgen(js_name = peerLeft)]
    pub fn peer_left(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let PeerInput { identity, now_us, .. } = js::from_value(input)?;
        outputs(self.step(Input::PeerLeft { peer: &identity }, now_us))
    }

    /// A data packet. Topics that aren't Portal's are ignored.
    pub fn data(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let DataInput { topic, from, payload, now_us } = js::from_value(input)?;
        let Some(stream) = livekit::stream(&topic) else {
            return outputs(Vec::new());
        };
        let input = Input::Data { stream, from: from.as_deref(), payload: &payload };
        outputs(self.step(input, now_us))
    }

    /// One frame-video byte stream, read whole.
    #[wasm_bindgen(js_name = frameBytes)]
    pub fn frame_bytes(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let FrameBytesInput { payload, now_us } = js::from_value(input)?;
        let Some(received) = self.frame_video.decode(Bytes::from(payload.into_vec())) else {
            return outputs(Vec::new());
        };
        let track = received.track_name;
        if let Some(metrics) = self.inner.metrics_registry().track(&track) {
            metrics.record_received_bytes(
                received.frame.timestamp_us,
                us(now_us),
                received.wire_len,
            );
        }
        let frame = WebFrame::Pixels(received.frame);
        outputs(self.step(Input::VideoFrame { track: &track, frame }, now_us))
    }

    /// A decoded WebRTC frame the driver holds as `handle`, stamped with
    /// the sender's timestamp from its packet trailer.
    #[wasm_bindgen(js_name = videoFrame)]
    pub fn video_frame(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let VideoFrameInput { track, handle, width, height, timestamp_us, now_us } =
            js::from_value(input)?;
        let timestamp_us = us(timestamp_us);
        if let Some(metrics) = self.inner.metrics_registry().track(&track) {
            metrics.record_received(timestamp_us, us(now_us));
        }
        let frame = WebFrame::Handle(FrameHandle {
            id: handle,
            width,
            height,
            timestamp_us,
            source: FrameSource::Live,
        });
        outputs(self.step(Input::VideoFrame { track: &track, frame }, now_us))
    }

    /// A call to one of Portal's own RPC methods.
    #[wasm_bindgen(js_name = rpcRequest)]
    pub fn rpc_request(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let RpcRequestInput { id, method, from, payload, now_us } = js::from_value(input)?;
        let request = RpcRequest { id: RpcId(id), method: &method, from: &from, payload: &payload };
        outputs(self.step(Input::RpcRequest(request), now_us))
    }

    #[wasm_bindgen(js_name = rpcResponse)]
    pub fn rpc_response(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let RpcResponseInput { id, result, now_us } = js::from_value(input)?;
        let input = Input::RpcResponse { id: OpId(id), result: result.into() };
        outputs(self.step(input, now_us))
    }

    #[wasm_bindgen(js_name = presenceSet)]
    pub fn presence_set(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let PresenceSetInput { id, error, now_us } = js::from_value(input)?;
        let result = error.map_or(Ok(()), Err);
        outputs(self.step(Input::PresenceSet { id: OpId(id), result }, now_us))
    }

    #[wasm_bindgen(js_name = linkRestored)]
    pub fn link_restored(&mut self, now_us: f64) -> Result<JsValue, JsError> {
        outputs(self.step(Input::LinkRestored, now_us))
    }

    pub fn wake(&mut self, now_us: f64) -> Result<JsValue, JsError> {
        outputs(self.step(Input::Wake, now_us))
    }

    #[wasm_bindgen(js_name = sendState)]
    pub fn send_state(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let SendValuesInput { values, timestamp_us, now_us, .. } = js::from_value(input)?;
        let values = js::typed(values, self.inner.state_schema());
        let out = self
            .inner
            .send_state(SendState {
                values: &values,
                timestamp_us: timestamp_us.map(us),
                now_us: us(now_us),
            })
            .map_err(err)?;
        outputs(out)
    }

    #[wasm_bindgen(js_name = sendTelemetry)]
    pub fn send_telemetry(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let SendValuesInput { values, timestamp_us, now_us, .. } = js::from_value(input)?;
        let values = js::typed(values, self.inner.telemetry_schema());
        let out = self
            .inner
            .send_telemetry(SendTelemetry {
                values: &values,
                timestamp_us: timestamp_us.map(us),
                now_us: us(now_us),
            })
            .map_err(err)?;
        outputs(out)
    }

    #[wasm_bindgen(js_name = sendAction)]
    pub fn send_action(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let SendValuesInput { values, timestamp_us, in_reply_to_ts_us, now_us } =
            js::from_value(input)?;
        let values = js::typed(values, self.inner.action_schema());
        let out = self
            .inner
            .send_action(SendAction {
                values: &values,
                timestamp_us: timestamp_us.map(us),
                in_reply_to_ts_us: in_reply_to_ts_us.map(us),
                now_us: us(now_us),
            })
            .map_err(err)?;
        outputs(out)
    }

    #[wasm_bindgen(js_name = sendKeypoint)]
    pub fn send_keypoint(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let SendKeypointInput { kind, payload, timestamp_us, now_us } = js::from_value(input)?;
        let out = self
            .inner
            .send_keypoint(SendKeypoint {
                kind: &kind,
                payload,
                timestamp_us: timestamp_us.map(us),
                now_us: us(now_us),
            })
            .map_err(err)?;
        outputs(out)
    }

    /// Route one robot frame. For a byte-frame track, also encode `rgb`
    /// into the payload to send.
    #[wasm_bindgen(js_name = sendVideoFrame)]
    pub fn send_video_frame(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let SendVideoFrameInput { track, timestamp_us, now_us, rgb, width, height } =
            js::from_value(input)?;
        let route = self
            .inner
            .send_video_frame(SendVideoFrame {
                track: &track,
                timestamp_us: timestamp_us.map(us),
                now_us: us(now_us),
            })
            .map_err(err)?;
        let output = match route.transport {
            VideoTransport::TransportEncoded => VideoRouteOutput {
                timestamp_us: route.timestamp_us,
                transport: "transport",
                payload: None,
            },
            VideoTransport::ByteFrames { codec, quality } => {
                let (Some(rgb), Some(width), Some(height)) = (rgb, width, height) else {
                    return Err(JsError::new(&format!(
                        "track '{track}' sends byte frames: pass rgb, width and height"
                    )));
                };
                let payload = build_frame_payload(RgbFrame {
                    track_name: &track,
                    codec,
                    width,
                    height,
                    timestamp_us: route.timestamp_us,
                    rgb: &rgb,
                    quality,
                })
                .map_err(err)?;
                if let Some(metrics) = self.inner.metrics_registry().track(&track) {
                    metrics.record_sent_bytes(payload.len());
                }
                VideoRouteOutput {
                    timestamp_us: route.timestamp_us,
                    transport: "bytes",
                    payload: Some(ByteBuf::from(payload)),
                }
            }
            VideoTransport::DriverEncoded { codec } => {
                return Err(JsError::new(&format!("no driver-side {codec:?} encoder on LiveKit")));
            }
        };
        js::to_value(&output)
    }

    #[wasm_bindgen(js_name = setActiveOperator)]
    pub fn set_active_operator(&mut self, input: JsValue) -> Result<JsValue, JsError> {
        let SetActiveOperatorInput { identity, now_us } = js::from_value(input)?;
        let (_, out) = self
            .inner
            .set_active_operator(SetActiveOperator { identity, now_us: us(now_us) })
            .map_err(err)?;
        outputs(out)
    }

    /// Now on the robot's clock, given the raw local time.
    #[wasm_bindgen(js_name = nowUs)]
    pub fn now_us(&mut self, raw_us: f64) -> f64 {
        self.inner.now_us(us(raw_us)) as f64
    }

    #[wasm_bindgen(js_name = timeSynced)]
    pub fn time_synced(&self) -> bool {
        self.inner.time_synced()
    }

    #[wasm_bindgen(js_name = receivesFrameBytes)]
    pub fn receives_frame_bytes(&self) -> bool {
        self.inner.receives_frame_bytes()
    }

    #[wasm_bindgen(js_name = activeOperator)]
    pub fn active_operator(&self) -> Option<String> {
        self.inner.active_operator()
    }

    pub fn operators(&self) -> Vec<String> {
        self.inner.operators()
    }

    pub fn observers(&self) -> Vec<String> {
        self.inner.observers()
    }

    #[wasm_bindgen(js_name = robotIdentity)]
    pub fn robot_identity(&self) -> Option<String> {
        self.inner.robot_identity()
    }

    #[wasm_bindgen(js_name = defaultPeer)]
    pub fn default_peer(&self) -> Option<String> {
        self.inner.default_peer()
    }

    #[wasm_bindgen(js_name = localIdentity)]
    pub fn local_identity(&self) -> Option<String> {
        self.inner.local_identity().map(String::from)
    }

    pub fn metrics(&self) -> Result<JsValue, JsError> {
        js::to_value(&Metrics::from(self.inner.metrics()))
    }

    #[wasm_bindgen(js_name = resetMetrics)]
    pub fn reset_metrics(&mut self) {
        self.inner.reset_metrics();
    }
}

impl Core {
    fn step(&mut self, input: Input<'_, WebFrame>, now_us: f64) -> Vec<Output<WebFrame>> {
        self.inner.handle(Step { input, now_us: us(now_us) })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireNames {
    frame_video_topic: &'static str,
    set_active_operator_rpc: &'static str,
}

/// LiveKit names the driver uses directly.
#[wasm_bindgen(js_name = wireNames)]
pub fn wire_names() -> Result<JsValue, JsError> {
    js::to_value(&WireNames {
        frame_video_topic: livekit::FRAME_VIDEO_TOPIC,
        set_active_operator_rpc: livekit_portal_core::SET_ACTIVE_OPERATOR_RPC,
    })
}
