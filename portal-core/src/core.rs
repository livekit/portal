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

//! `PortalCore`: every Portal decision, with no IO. A driver feeds it
//! transport events (peers, presence, stream messages, frames) plus the
//! local time, and carries out the publishes it returns. What the
//! transport can't do natively (`TransportCaps`), the core polyfills over
//! its own streams.
//!
//! Times are raw local microseconds from the driver's clock. The core never
//! reads a clock, spawns a task or touches a socket.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Map, Value};

use crate::clock::{ClockOptions, ClockPacket, ClockSync, EstimatorEvent, Exchange, Sample};
use crate::codec::Codec;
use crate::config::{FieldSpec, PortalConfig};
use crate::control::{ControlMessage, decode_presence, encode_presence};
use crate::data::{ActionOrigin, ActionRecord, DataPublisher, SendMap, build_action, build_state};
use crate::error::{PortalError, PortalResult};
use crate::frame::FramePayload;
use crate::frame_video::FrameVideoTracks;
use crate::keypoint::{self, Keypoint};
use crate::metrics::{DataStream, MetricsRegistry, PortalMetrics};
use crate::presence::{PresenceUpdate, Roster, RosterEvent};
use crate::rpc::RpcError;
use crate::serialization::{
    DecodeError, action_fingerprint, deserialize_action, deserialize_values, schema_fingerprint,
};
use crate::sync_buffer::{SyncBuffer, SyncOutput};
use crate::transport::{Delivery, PeerId, Presence, Stream, TransportCaps, VideoCaps};
use crate::types::{
    Action, ActionSubscription, Observation, Role, State, TypedValue, VideoFrameData,
};

/// RPC method a robot answers so any peer can request a change to the
/// active operator pointer. Payload is the new identity, or the empty
/// string to clear. Result is the empty string on success.
pub const SET_ACTIVE_OPERATOR_RPC: &str = "portal.set_active_operator";

/// App-level RPC error code (outside the transport's 1001-1999 range)
/// returned when the robot is not connected yet.
pub const RPC_NOT_CONNECTED: u32 = 2001;
/// App-level RPC error code returned when the transport fails to update the
/// robot's presence while it processes a `set_active_operator` request.
pub const RPC_PRESENCE_FAILED: u32 = 2002;
/// App-level RPC error code returned when `set_active_operator` comes from
/// a peer that is neither an operator nor an observer.
pub const RPC_NOT_AUTHORIZED: u32 = 2003;
/// Transport-level RPC codes, shared with LiveKit's so callers see the same
/// errors whether RPC is native or polyfilled.
pub const RPC_UNSUPPORTED_METHOD: u32 = 1400;
pub const RPC_RESPONSE_TIMEOUT: u32 = 1502;
pub const RPC_RECIPIENT_DISCONNECTED: u32 = 1503;
/// How long the robot waits for an unrecognised RPC caller to appear in the
/// rosters. The caller's presence and its RPC travel separately, so a peer
/// that calls right after connecting can beat its own presence here.
pub const RPC_CALLER_ROSTER_WAIT_US: u64 = 1_500_000;
/// Polyfilled RPC timeout when the caller sets none (LiveKit's default).
pub const DEFAULT_RPC_TIMEOUT_MS: u32 = 15_000;
/// Polyfilled presence: a peer re-announces itself this often, and is
/// taken to have left after `PRESENCE_TIMEOUT_US` of silence.
pub const PRESENCE_HEARTBEAT_US: u64 = 1_000_000;
pub const PRESENCE_TIMEOUT_US: u64 = 3 * PRESENCE_HEARTBEAT_US;
/// Lossy actions older than this are stale commands, better dropped.
pub const ACTION_MAX_AGE_MS: u32 = 200;
/// A late clock packet only adds a useless round trip.
pub const CLOCK_MAX_AGE_MS: u32 = 500;

/// An RPC to answer. Numbered by the driver for native RPC, by the core
/// for polyfilled RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RpcId(pub u64);

/// An operation the core started and wants the outcome of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

pub struct CoreInit {
    pub config: PortalConfig,
    pub caps: TransportCaps,
}

/// The transport is up.
pub struct Connect {
    /// This peer's authenticated identity.
    pub local_peer: PeerId,
    /// Robot only: a pointer the transport seeded before connect (a LiveKit
    /// token attribute), so the gate holds before anyone sets it.
    pub active_operator: Option<PeerId>,
    /// Native presence only: peers already there, with what they announce.
    pub peers: Vec<(PeerId, Presence)>,
    pub now_us: u64,
}

/// One input plus the local time it happened at.
pub struct Step<'a, F> {
    pub input: Input<'a, F>,
    pub now_us: u64,
}

pub enum Input<'a, F> {
    /// Native presence: a peer joined. Its presence may follow separately.
    PeerJoined { peer: &'a str },
    /// Native presence: what a peer announces changed.
    PresenceChanged { peer: &'a str, presence: Presence },
    /// Native presence: a peer left.
    PeerLeft { peer: &'a str },
    /// A message on a peer's stream. `from` is `None` for messages the
    /// transport itself sent.
    Data { stream: Stream, from: Option<&'a str>, payload: &'a [u8] },
    /// A frame envelope (see `frame_video`), decoded here.
    FrameBytes { from: &'a str, payload: Bytes },
    /// A frame the driver already decoded, timestamped by the sender.
    VideoFrame { track: &'a str, frame: F },
    /// Native RPC: a call to one of Portal's own methods.
    RpcRequest(RpcRequest<'a>),
    /// Native RPC: how an `Output::Rpc` ended.
    RpcResponse { id: OpId, result: Result<String, RpcError> },
    /// Native presence: how an `Output::SetPresence` with an id ended.
    PresenceSet { id: OpId, result: Result<(), String> },
    /// The transport reconnected after an outage.
    LinkRestored,
    /// The time asked for by `ScheduleWake` has come.
    Wake,
}

pub struct RpcRequest<'a> {
    pub id: RpcId,
    pub method: &'a str,
    pub from: &'a str,
    pub payload: &'a str,
}

#[derive(Debug)]
pub enum Output<F> {
    /// Publish on this peer's stream. Transports only broadcast; `to` is a
    /// hint a transport with native addressing may use. Point-to-point
    /// messages carry their addressee, so receivers can filter without it.
    Publish {
        stream: Stream,
        payload: Vec<u8>,
        delivery: Delivery,
        to: Option<PeerId>,
    },
    /// Native presence: announce this. With an id, report the outcome back
    /// as `Input::PresenceSet`.
    SetPresence {
        id: Option<OpId>,
        presence: Presence,
    },
    /// Native RPC: call a method on a peer and report back as
    /// `Input::RpcResponse`.
    Rpc {
        id: OpId,
        to: PeerId,
        method: String,
        payload: String,
        timeout_ms: Option<u32>,
    },
    /// Native RPC: answer an `Input::RpcRequest`.
    RpcReply {
        id: RpcId,
        result: Result<String, RpcError>,
    },
    Emit(Event<F>),
    /// Feed `Input::Wake` at this local time, replacing any earlier
    /// request. `None` cancels.
    ScheduleWake {
        at_us: Option<u64>,
    },
}

#[derive(Debug)]
pub enum Event<F> {
    Action(Action),
    State(State),
    VideoFrame {
        track: String,
        frame: F,
    },
    Observation(Observation<F>),
    /// States that could not be matched to video, typed like
    /// `Observation::state`.
    Drops(Vec<HashMap<String, TypedValue>>),
    Keypoint(Keypoint),
    TimeSynced,
    OperatorJoined(PeerId),
    OperatorLeft(PeerId),
    ActiveOperatorChanged(Option<PeerId>),
    /// How a `perform_rpc` or an operator's `set_active_operator` ended.
    RpcResult {
        id: OpId,
        result: Result<String, RpcError>,
    },
    /// Polyfilled RPC: a call to a method the application registered.
    /// Answer with `PortalCore::reply_rpc`.
    RpcRequest {
        id: RpcId,
        from: PeerId,
        method: String,
        payload: String,
    },
}

pub struct SendState<'a> {
    pub values: &'a HashMap<String, TypedValue>,
    pub timestamp_us: Option<u64>,
    pub now_us: u64,
}

pub struct SendAction<'a> {
    pub values: &'a HashMap<String, TypedValue>,
    pub timestamp_us: Option<u64>,
    pub in_reply_to_ts_us: Option<u64>,
    pub now_us: u64,
}

pub struct SendKeypoint<'a> {
    pub kind: &'a str,
    pub payload: Map<String, Value>,
    pub timestamp_us: Option<u64>,
    pub now_us: u64,
}

pub struct SendVideoFrame<'a> {
    pub track: &'a str,
    pub timestamp_us: Option<u64>,
    pub now_us: u64,
}

/// Where a frame the robot sends goes. The pixels stay with the driver,
/// which encodes them off the core's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoRoute {
    pub timestamp_us: u64,
    pub transport: VideoTransport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTransport {
    /// Hand raw frames to the transport, which encodes them and carries
    /// the timestamp in its own metadata.
    TransportEncoded,
    /// Encode a frame envelope with `frame_video::build_frame_payload` and
    /// publish it as bytes on the track's stream.
    ByteFrames { codec: Codec, quality: u8 },
    /// Encode with the driver's own `codec` encoder, carrying the timestamp
    /// in Portal's envelope.
    DriverEncoded { codec: Codec },
}

pub struct SetActiveOperator {
    /// `None` clears the pointer.
    pub identity: Option<PeerId>,
    pub now_us: u64,
}

pub struct PerformRpc {
    /// `None` picks the obvious counterpart (see `default_peer`).
    pub to: Option<PeerId>,
    pub method: String,
    pub payload: String,
    pub timeout_ms: Option<u32>,
    pub now_us: u64,
}

pub struct ReplyRpc {
    pub id: RpcId,
    pub result: Result<String, RpcError>,
}

/// Who asked for an RPC to be answered, and over what.
#[derive(Debug, Clone)]
enum Caller {
    Native(RpcId),
    Control { id: u64, from: PeerId },
}

enum PendingOp {
    /// Robot side, native presence: a pointer change waiting for the
    /// transport to announce it.
    SetActive { identity: Option<PeerId>, prev: Option<PeerId>, caller: Option<Caller> },
    /// An RPC this peer made. Polyfilled calls time out on their own.
    Call { to: PeerId, deadline_us: Option<u64> },
}

/// A `set_active_operator` RPC from a caller not yet in the rosters.
struct WaitingRpc {
    caller: Caller,
    from: PeerId,
    payload: String,
    deadline_us: u64,
}

pub struct PortalCore<F> {
    config: PortalConfig,
    caps: TransportCaps,
    /// Every video track name (WebRTC and frame video), in declaration order.
    track_names: Vec<String>,
    frame_tracks: FrameVideoTracks,
    action_fp: u32,
    state_fp: u32,
    metrics: Arc<MetricsRegistry>,
    clock: ClockSync,
    roster: Roster,
    local_peer: Option<PeerId>,
    /// Peers the transport reports (native presence) or that announced
    /// themselves (polyfill), excluding self.
    peers: HashSet<PeerId>,
    /// Polyfilled presence: when each peer was last heard from.
    last_heard_us: HashMap<PeerId, u64>,
    next_heartbeat_us: Option<u64>,
    state_publisher: Option<DataPublisher>,
    action_publisher: Option<DataPublisher>,
    sync_buffer: Option<SyncBuffer<F>>,
    /// Peer fingerprints already reported as mismatched, per stream. Logged
    /// once per unique offender to surface schema drift without spamming.
    action_mismatch_warned: HashSet<u32>,
    state_mismatch_warned: HashSet<u32>,
    /// Keypoint senders already warned about for a malformed packet.
    keypoint_warned: HashSet<String>,
    pending: HashMap<OpId, PendingOp>,
    next_op: u64,
    /// Polyfilled RPCs handed to the application, by the id it answers.
    inbound: HashMap<RpcId, Caller>,
    next_inbound: u64,
    waiting_rpcs: Vec<WaitingRpc>,
    scheduled_wake_us: Option<u64>,
    /// Local time of the step being handled.
    now_us: u64,
    /// Outputs of the step being handled.
    out: Vec<Output<F>>,
}

impl<F: FramePayload + From<VideoFrameData>> PortalCore<F> {
    pub fn new(init: CoreInit) -> Self {
        let CoreInit { config, caps } = init;
        let track_names: Vec<String> = config.video_track_names().map(String::from).collect();
        let ignored = config.ignored_sync_options();
        if !ignored.is_empty() {
            log::warn!(
                "[{}] observation sync is off, so these options do nothing: {}",
                config.session(),
                ignored.join(", ")
            );
        }
        let clock = ClockSync::new(ClockOptions {
            is_reference: config.role() == Role::Robot,
            source: config.time_sync_source(),
        });
        Self {
            action_fp: action_fingerprint(config.action_schema()),
            state_fp: schema_fingerprint(config.state_schema()),
            metrics: Arc::new(MetricsRegistry::new(&track_names)),
            frame_tracks: FrameVideoTracks::new(&config),
            track_names,
            clock,
            roster: Roster::default(),
            local_peer: None,
            peers: HashSet::new(),
            last_heard_us: HashMap::new(),
            next_heartbeat_us: None,
            state_publisher: None,
            action_publisher: None,
            sync_buffer: None,
            action_mismatch_warned: HashSet::new(),
            state_mismatch_warned: HashSet::new(),
            keypoint_warned: HashSet::new(),
            pending: HashMap::new(),
            next_op: 0,
            inbound: HashMap::new(),
            next_inbound: 0,
            waiting_rpcs: Vec::new(),
            scheduled_wake_us: None,
            now_us: 0,
            out: Vec::new(),
            caps,
            config,
        }
    }

    /// The transport is up. Fails, before anything is published, when the
    /// config asks for something the transport cannot do.
    pub fn connect(&mut self, input: Connect) -> PortalResult<Vec<Output<F>>> {
        let Connect { local_peer, active_operator, peers, now_us } = input;
        if self.local_peer.is_some() {
            return Err(PortalError::AlreadyConnected);
        }
        self.caps.validate(&self.config)?;
        self.now_us = now_us;
        let role = self.config.role();
        self.local_peer = Some(local_peer);
        if role == Role::Robot && active_operator.is_some() {
            self.roster.active_operator = active_operator;
        }
        for (peer, presence) in peers {
            self.peers.insert(peer.clone());
            self.update_roster(&peer, &presence);
        }
        let session = self.config.session().to_string();
        match role {
            Role::Robot => {
                for spec in self.config.frame_video_tracks() {
                    log::info!(
                        "[{session}] ready to publish frame-video track '{}' as byte frames (codec={:?}, quality={})",
                        spec.name,
                        spec.codec,
                        spec.quality
                    );
                }
                let schema = self.config.state_schema();
                if !schema.is_empty() {
                    let mode = reliability(self.config.state_reliable());
                    log::info!(
                        "[{session}] ready to publish state via {mode} data ({} fields)",
                        schema.len()
                    );
                    self.state_publisher = Some(DataPublisher::new(schema, DataStream::State));
                }
            }
            Role::Operator | Role::Observer => {
                if self.config.observation_sync() {
                    self.sync_buffer = Some(SyncBuffer::new(
                        &self.track_names,
                        self.config.state_schema().to_vec(),
                        self.config.sync_config(),
                        self.config.stall_configs(&self.track_names),
                        self.metrics.clone(),
                    ));
                }
                let schema = self.config.action_schema();
                if role == Role::Operator && !schema.is_empty() {
                    let mode = reliability(self.config.action_reliable());
                    log::info!(
                        "[{session}] ready to publish action via {mode} data ({} fields)",
                        schema.len()
                    );
                    self.action_publisher = Some(DataPublisher::new(schema, DataStream::Action));
                }
            }
        }
        self.clock.start_pinging(now_us);
        if !self.caps.native_presence {
            self.announce_presence();
            self.next_heartbeat_us = Some(now_us.saturating_add(PRESENCE_HEARTBEAT_US));
        }
        Ok(self.finish_step())
    }

    /// The connection ended, or a connect attempt is being rolled back.
    pub fn disconnect(&mut self) -> Vec<Output<F>> {
        self.local_peer = None;
        self.roster.clear();
        self.peers.clear();
        self.last_heard_us.clear();
        self.next_heartbeat_us = None;
        self.state_publisher = None;
        self.action_publisher = None;
        if let Some(mut sb) = self.sync_buffer.take() {
            sb.clear();
        }
        self.clock.stop_pinging();
        for (id, op) in std::mem::take(&mut self.pending) {
            if let PendingOp::Call { .. } = op {
                let result = Err(RpcError::new(RPC_RECIPIENT_DISCONNECTED, "disconnected", None));
                self.out.push(Output::Emit(Event::RpcResult { id, result }));
            }
        }
        for w in std::mem::take(&mut self.waiting_rpcs) {
            self.reply(
                w.caller,
                Err(RpcError::new(RPC_NOT_CONNECTED, "robot not connected", None)),
            );
        }
        self.inbound.clear();
        self.finish_step()
    }

    pub fn handle(&mut self, step: Step<'_, F>) -> Vec<Output<F>> {
        let Step { input, now_us } = step;
        self.now_us = now_us;
        match input {
            Input::PeerJoined { peer } => {
                if self.local_peer.as_deref() != Some(peer) {
                    self.peers.insert(peer.to_string());
                }
            }
            Input::PresenceChanged { peer, presence } => {
                // A transport may echo this peer's own presence back.
                if self.local_peer.as_deref() != Some(peer) {
                    self.peers.insert(peer.to_string());
                    self.update_roster(peer, &presence);
                }
            }
            Input::PeerLeft { peer } => self.peer_left(peer),
            Input::Data { stream, from, payload } => self.on_data(stream, from, payload),
            Input::FrameBytes { from: _, payload } => self.on_frame_bytes(payload),
            Input::VideoFrame { track, frame } => self.on_video_frame(track, frame),
            Input::RpcRequest(request) => self.on_rpc_request(request),
            Input::RpcResponse { id, result } => {
                if self.pending.remove(&id).is_some() {
                    self.out.push(Output::Emit(Event::RpcResult { id, result }));
                }
            }
            Input::PresenceSet { id, result } => self.on_presence_set(id, result),
            Input::LinkRestored => {
                log::info!(
                    "[{}] reconnected, clearing sync buffers and latest slots",
                    self.config.session()
                );
                if let Some(sb) = &mut self.sync_buffer {
                    sb.clear();
                }
                self.roster.clear_for_reconnect();
                if !self.caps.native_presence {
                    self.last_heard_us.clear();
                    self.peers.clear();
                    self.announce_presence();
                }
            }
            Input::Wake => self.on_wake(),
        }
        self.finish_step()
    }

    // --- Sends ---

    /// Publish a state sample (robot only). `timestamp_us` defaults to
    /// `now_us()`.
    pub fn send_state(&mut self, input: SendState<'_>) -> PortalResult<Vec<Output<F>>> {
        let SendState { values, timestamp_us, now_us } = input;
        let role = self.config.role();
        let publisher = self.state_publisher.as_mut().ok_or(PortalError::WrongRole(role))?;
        let timestamp_us = timestamp_us.unwrap_or_else(|| self.clock.now(now_us));
        let encoded =
            publisher.send_map(SendMap { values, timestamp_us, in_reply_to_ts_us: None })?;
        self.metrics.bump_sent(DataStream::State);
        let delivery = match self.config.state_reliable() {
            true => Delivery::Reliable,
            false => Delivery::LatestOnly,
        };
        Ok(vec![Output::Publish {
            stream: Stream::State,
            payload: encoded.payload,
            delivery,
            to: None,
        }])
    }

    /// Publish an action (operator only), echoing it to this peer's own
    /// subscription: transports don't deliver a publisher's own messages.
    pub fn send_action(&mut self, input: SendAction<'_>) -> PortalResult<Vec<Output<F>>> {
        let SendAction { values, timestamp_us, in_reply_to_ts_us, now_us } = input;
        let role = self.config.role();
        let publisher = self.action_publisher.as_mut().ok_or(PortalError::WrongRole(role))?;
        let send_ts = timestamp_us.unwrap_or_else(|| self.clock.now(now_us));
        let encoded =
            publisher.send_map(SendMap { values, timestamp_us: send_ts, in_reply_to_ts_us })?;
        self.metrics.bump_sent(DataStream::Action);
        let delivery = match self.config.action_reliable() {
            true => Delivery::Reliable,
            false => Delivery::BestEffort { max_age_ms: ACTION_MAX_AGE_MS },
        };
        let mut out = vec![Output::Publish {
            stream: Stream::Action,
            payload: encoded.payload,
            delivery,
            to: None,
        }];
        // Echo what the subscription would have delivered from anyone else:
        // the wire values, post-carry-forward and post-saturation.
        let active = self.is_self_active();
        let echo = match self.config.action_subscription() {
            ActionSubscription::None => false,
            ActionSubscription::Active => active,
            ActionSubscription::All => true,
        };
        if let Some(sender) = self.local_peer.clone().filter(|_| echo) {
            out.push(Output::Emit(Event::Action(build_action(ActionRecord {
                timestamp_us: send_ts,
                in_reply_to_ts_us,
                schema: self.config.action_schema(),
                values: &encoded.wire_values,
                origin: ActionOrigin { sender, active },
            }))));
        }
        Ok(out)
    }

    /// Send an annotation to everyone in the session, this peer included.
    pub fn send_keypoint(&mut self, input: SendKeypoint<'_>) -> PortalResult<Vec<Output<F>>> {
        let SendKeypoint { kind, payload, timestamp_us, now_us } = input;
        let sender = self.local_peer.clone().ok_or(PortalError::NotConnected)?;
        let timestamp_us = timestamp_us.unwrap_or_else(|| self.clock.now(now_us));
        Ok(vec![
            Output::Publish {
                stream: Stream::Keypoint,
                payload: keypoint::encode(kind, &payload, timestamp_us),
                delivery: Delivery::Reliable,
                to: None,
            },
            Output::Emit(Event::Keypoint(Keypoint {
                kind: kind.to_string(),
                payload,
                timestamp_us,
                sender,
            })),
        ])
    }

    /// Route a frame the robot is sending. Errors match `send_video_frame`:
    /// `WrongRole` for a declared track on a non-robot, otherwise
    /// `UnknownVideoTrack`.
    pub fn send_video_frame(&mut self, input: SendVideoFrame<'_>) -> PortalResult<VideoRoute> {
        let SendVideoFrame { track, timestamp_us, now_us } = input;
        let timestamp_us = timestamp_us.unwrap_or_else(|| self.clock.now(now_us));
        let spec = self.config.video_tracks().iter().find(|s| s.name == track);
        match (self.config.role(), spec) {
            (Role::Robot, Some(spec)) => {
                let transport = match (spec.is_webrtc(), self.caps.video) {
                    (false, _) => {
                        VideoTransport::ByteFrames { codec: spec.codec, quality: spec.quality }
                    }
                    (true, VideoCaps::DriverEncoded) => {
                        VideoTransport::DriverEncoded { codec: spec.codec }
                    }
                    // `connect` refused WebRTC codecs on byte-frame transports.
                    (true, _) => VideoTransport::TransportEncoded,
                };
                Ok(VideoRoute { timestamp_us, transport })
            }
            (role, Some(_)) => Err(PortalError::WrongRole(role)),
            (_, None) => Err(PortalError::UnknownVideoTrack { name: track.to_string() }),
        }
    }

    /// Set the active operator. The robot moves its own pointer and
    /// announces it in its presence; others ask the robot over RPC, and
    /// learn the outcome as `Event::RpcResult` for the returned id.
    pub fn set_active_operator(
        &mut self,
        input: SetActiveOperator,
    ) -> PortalResult<(Option<OpId>, Vec<Output<F>>)> {
        let SetActiveOperator { identity, now_us } = input;
        self.now_us = now_us;
        if self.local_peer.is_none() {
            return Err(match self.config.role() {
                Role::Robot => PortalError::NotConnected,
                Role::Operator | Role::Observer => PortalError::NoPeer,
            });
        }
        let id = match self.config.role() {
            Role::Robot => {
                self.change_active_operator(identity, None);
                None
            }
            Role::Operator | Role::Observer => {
                let robot = self.roster.robot.clone().ok_or(PortalError::NoPeer)?;
                Some(self.start_call(Call {
                    to: robot,
                    method: SET_ACTIVE_OPERATOR_RPC.to_string(),
                    payload: identity.unwrap_or_default(),
                    timeout_ms: None,
                }))
            }
        };
        Ok((id, self.finish_step()))
    }

    /// Call `method` on a peer. The outcome arrives as `Event::RpcResult`
    /// for the returned id.
    pub fn perform_rpc(&mut self, input: PerformRpc) -> PortalResult<(OpId, Vec<Output<F>>)> {
        let PerformRpc { to, method, payload, timeout_ms, now_us } = input;
        self.now_us = now_us;
        if self.local_peer.is_none() {
            return Err(PortalError::NotConnected);
        }
        let to = match to {
            Some(to) => to,
            None => self.resolve_peer()?,
        };
        let id = self.start_call(Call { to, method, payload, timeout_ms });
        Ok((id, self.finish_step()))
    }

    /// Answer an `Event::RpcRequest`.
    pub fn reply_rpc(&mut self, input: ReplyRpc) -> Vec<Output<F>> {
        let ReplyRpc { id, result } = input;
        if let Some(caller) = self.inbound.remove(&id) {
            self.reply(caller, result);
        }
        std::mem::take(&mut self.out)
    }

    // --- Getters ---

    pub fn config(&self) -> &PortalConfig {
        &self.config
    }

    pub fn caps(&self) -> &TransportCaps {
        &self.caps
    }

    pub fn state_schema(&self) -> &[FieldSpec] {
        self.config.state_schema()
    }

    pub fn action_schema(&self) -> &[FieldSpec] {
        self.config.action_schema()
    }

    /// This peer's identity while connected.
    pub fn local_identity(&self) -> Option<&str> {
        self.local_peer.as_deref()
    }

    /// What this peer announces.
    pub fn local_presence(&self) -> Presence {
        let role = self.config.role();
        let active_operator = match role {
            Role::Robot => self.roster.active_operator.clone(),
            Role::Operator | Role::Observer => None,
        };
        Presence { active_operator, ..Presence::local(role) }
    }

    pub fn active_operator(&self) -> Option<PeerId> {
        self.roster.active_operator.clone()
    }

    /// Connected operators (excluding self), sorted.
    pub fn operators(&self) -> Vec<PeerId> {
        sorted(&self.roster.operators)
    }

    /// Connected observers (excluding self), sorted.
    pub fn observers(&self) -> Vec<PeerId> {
        sorted(&self.roster.observers)
    }

    pub fn robot_identity(&self) -> Option<PeerId> {
        self.roster.robot.clone()
    }

    /// Every connected peer (excluding self), Portal or not, sorted.
    pub fn peers(&self) -> Vec<PeerId> {
        sorted(&self.peers)
    }

    /// The obvious RPC counterpart: the robot for an operator or observer,
    /// the active operator for the robot. `None` if not known yet.
    pub fn default_peer(&self) -> Option<PeerId> {
        match self.config.role() {
            Role::Operator | Role::Observer => self.roster.robot.clone(),
            Role::Robot => self.roster.active_operator.clone(),
        }
    }

    /// Whether this peer takes frame envelopes (`Input::FrameBytes`).
    pub fn receives_frame_bytes(&self) -> bool {
        self.config.role() != Role::Robot && !self.frame_tracks.is_empty()
    }

    /// Now on the robot's clock: never repeats, and never goes backwards
    /// except once, at the first sync.
    pub fn now_us(&mut self, raw_us: u64) -> u64 {
        self.clock.now(raw_us)
    }

    /// Counters shared with the driver, which records the transport events
    /// only it sees (frames captured, received off a stream, bytes sent).
    pub fn metrics_registry(&self) -> &Arc<MetricsRegistry> {
        &self.metrics
    }

    pub fn metrics(&self) -> PortalMetrics {
        let (video_fill, state_fill) = match &self.sync_buffer {
            Some(sb) => (sb.video_fill_snapshot(), sb.state_fill()),
            None => (HashMap::new(), 0),
        };
        let mut snapshot = self.metrics.snapshot(video_fill, state_fill);
        snapshot.time_sync = self.clock.metrics();
        snapshot
    }

    /// Resets counters. Time sync state is kept: see `TimeSyncMetrics`.
    pub fn reset_metrics(&mut self) {
        self.metrics.reset();
        self.clock.reset_counters();
    }

    /// The local time `Input::Wake` is next needed at, if any.
    pub fn next_wake_us(&self) -> Option<u64> {
        let rpc = self.waiting_rpcs.iter().map(|w| w.deadline_us);
        let calls = self.pending.values().filter_map(|op| match op {
            PendingOp::Call { deadline_us, .. } => *deadline_us,
            PendingOp::SetActive { .. } => None,
        });
        let timeouts = self.last_heard_us.values().map(|at| at + PRESENCE_TIMEOUT_US);
        [self.clock.next_ping_us(), self.next_heartbeat_us]
            .into_iter()
            .flatten()
            .chain(rpc)
            .chain(calls)
            .chain(timeouts)
            .min()
    }

    // --- Internal ---

    fn finish_step(&mut self) -> Vec<Output<F>> {
        self.poll_waiting_rpcs();
        self.schedule_wake();
        std::mem::take(&mut self.out)
    }

    fn update_roster(&mut self, peer: &str, presence: &Presence) {
        let update = PresenceUpdate { self_role: self.config.role(), peer, presence };
        if let Some(event) = self.roster.update(update) {
            self.roster_event(event);
        }
    }

    fn peer_left(&mut self, peer: &str) {
        log::info!("[{}] participant '{peer}' disconnected", self.config.session());
        self.peers.remove(peer);
        self.last_heard_us.remove(peer);
        for event in self.roster.remove(peer) {
            self.roster_event(event);
        }
    }

    fn roster_event(&mut self, event: RosterEvent) {
        let event = match event {
            RosterEvent::OperatorJoined(id) => Event::OperatorJoined(id),
            RosterEvent::OperatorLeft(id) => Event::OperatorLeft(id),
            RosterEvent::ActiveOperatorChanged(id) => Event::ActiveOperatorChanged(id),
            RosterEvent::RobotLeft => {
                // Whoever answers next may be a restarted robot with a new
                // clock, so the estimate starts over.
                self.clock.restart();
                return;
            }
        };
        self.out.push(Output::Emit(event));
    }

    fn is_self_active(&self) -> bool {
        match (&self.local_peer, &self.roster.active_operator) {
            (Some(local), Some(active)) => local == active,
            _ => false,
        }
    }

    fn announce_presence(&mut self) {
        if self.caps.native_presence {
            let presence = self.local_presence();
            self.out.push(Output::SetPresence { id: None, presence });
            return;
        }
        self.out.push(Output::Publish {
            stream: Stream::Presence,
            payload: encode_presence(&self.local_presence()),
            delivery: Delivery::LatestOnly,
            to: None,
        });
    }

    fn on_data(&mut self, stream: Stream, from: Option<&str>, payload: &[u8]) {
        // Messages that echo back to their publisher are not news.
        if from.is_some() && from == self.local_peer.as_deref() {
            return;
        }
        match stream {
            Stream::Clock => {
                if let Some(from) = from {
                    self.on_clock_packet(payload, from);
                }
            }
            Stream::Keypoint => {
                let Some(from) = from.filter(|s| self.roster.is_known_peer(s)) else {
                    return;
                };
                match keypoint::decode(payload, from.to_string()) {
                    Ok(kp) => self.out.push(Output::Emit(Event::Keypoint(kp))),
                    Err(e) => {
                        if self.keypoint_warned.insert(from.to_string()) {
                            log::warn!("[bad-payload] keypoint from '{from}' is not valid: {e}");
                        }
                    }
                }
            }
            Stream::Action => {
                let Some(origin) = self.gate_action(from) else {
                    return;
                };
                self.on_action(payload, origin);
            }
            Stream::State if self.config.role() != Role::Robot => self.on_state(payload),
            Stream::State => {}
            Stream::Presence if !self.caps.native_presence => {
                if let (Some(from), Some(presence)) = (from, decode_presence(payload)) {
                    self.on_polyfill_presence(from, presence);
                }
            }
            Stream::Control if !self.caps.native_rpc => {
                if let (Some(from), Some(message)) = (from, ControlMessage::decode(payload)) {
                    self.on_control(from, message);
                }
            }
            Stream::Presence | Stream::Control => {}
        }
    }

    /// The active-operator gate for an incoming action. Returns who sent it
    /// and whether they were active, or `None` to drop it before decoding.
    ///
    /// The robot takes only the active operator's actions. An operator takes
    /// what its subscription asks for, so a controller with `None` pays
    /// nothing on the receive path. In every case the sender must be a
    /// recognised operator: a pointer seeded before connect or set over RPC
    /// can name any identity.
    fn gate_action(&self, from: Option<&str>) -> Option<ActionOrigin> {
        let subscription = match self.config.role() {
            Role::Robot => ActionSubscription::Active,
            Role::Operator | Role::Observer => self.config.action_subscription(),
        };
        if subscription == ActionSubscription::None {
            return None;
        }
        let sender = from?;
        if !self.roster.operators.contains(sender) {
            return None;
        }
        let active = self.roster.active_operator.as_deref() == Some(sender);
        if !active && subscription == ActionSubscription::Active {
            return None;
        }
        Some(ActionOrigin { sender: sender.to_string(), active })
    }

    /// `received_at_us` is on the robot's clock, so the robot's end-to-end
    /// latency compares its own frame timestamps against its own clock.
    /// Jitter uses the local arrival time instead, which never steps with a
    /// sync correction.
    fn on_action(&mut self, payload: &[u8], origin: ActionOrigin) {
        let received_at_us = self.clock.estimate(self.now_us);
        let schema = self.config.action_schema();
        match deserialize_action(payload, self.action_fp, schema) {
            Ok((send_ts, in_reply_to_ts_us, values)) => {
                self.metrics.record_received(DataStream::Action, send_ts, self.now_us);
                self.metrics.record_e2e(in_reply_to_ts_us, received_at_us);
                self.out.push(Output::Emit(Event::Action(build_action(ActionRecord {
                    timestamp_us: send_ts,
                    in_reply_to_ts_us,
                    schema,
                    values: &values,
                    origin,
                }))));
            }
            Err(DecodeError::SchemaMismatch { expected, got }) => {
                warn_mismatch(&mut self.action_mismatch_warned, Stream::Action, expected, got)
            }
            Err(DecodeError::Malformed(e)) => {
                log::warn!("[bad-payload] action deserialize failed: {e}");
            }
        }
    }

    fn on_state(&mut self, payload: &[u8]) {
        let schema = self.config.state_schema();
        match deserialize_values(payload, self.state_fp, schema) {
            Ok((timestamp_us, values)) => {
                self.metrics.record_received(DataStream::State, timestamp_us, self.now_us);
                self.out.push(Output::Emit(Event::State(build_state(
                    timestamp_us,
                    schema,
                    &values,
                ))));
                if let Some(sb) = &mut self.sync_buffer {
                    sb.set_now(self.now_us);
                    let synced = sb.push_state(timestamp_us, values);
                    emit_sync(&mut self.out, synced);
                }
            }
            Err(DecodeError::SchemaMismatch { expected, got }) => {
                warn_mismatch(&mut self.state_mismatch_warned, Stream::State, expected, got)
            }
            Err(DecodeError::Malformed(e)) => {
                log::warn!("[bad-payload] state deserialize failed: {e}");
            }
        }
    }

    fn on_frame_bytes(&mut self, payload: Bytes) {
        if !self.receives_frame_bytes() {
            return;
        }
        let Some(received) = self.frame_tracks.decode(payload) else {
            return;
        };
        let ts = received.frame.timestamp_us;
        if let Some(tm) = self.metrics.track(&received.track_name) {
            tm.record_received_bytes(ts, self.now_us, received.wire_len);
        }
        self.on_video_frame(&received.track_name, F::from(received.frame));
    }

    fn on_video_frame(&mut self, track: &str, frame: F) {
        if !self.track_names.iter().any(|n| n == track) {
            return;
        }
        let Some(sb) = &mut self.sync_buffer else {
            self.out.push(Output::Emit(Event::VideoFrame { track: track.to_string(), frame }));
            return;
        };
        self.out.push(Output::Emit(Event::VideoFrame {
            track: track.to_string(),
            frame: frame.clone(),
        }));
        sb.set_now(self.now_us);
        let synced = sb.push_frame(track, frame);
        emit_sync(&mut self.out, synced);
    }

    fn on_clock_packet(&mut self, payload: &[u8], from: &str) {
        let Some(packet) = ClockPacket::decode(payload) else {
            return;
        };
        match (self.config.role(), packet) {
            (Role::Robot, ClockPacket::Ping { seq, t1 }) => {
                // Received and answered within the same step, so t2 == t3.
                let pong = ClockPacket::Pong { seq, t1, t2: self.now_us, t3: self.now_us };
                self.out.push(clock_packet(pong, from.to_string()));
            }
            (Role::Robot, ClockPacket::Pong { .. }) => {}
            (_, ClockPacket::Pong { seq, t1, t2, t3 }) => {
                if self.local_peer.is_none()
                    || self.roster.robot.as_deref() != Some(from)
                    || !self.clock.claim_pong(seq, t1)
                {
                    return;
                }
                let Ok(sample) = Sample::try_from(Exchange { t1, t2, t3, t4: self.now_us }) else {
                    return;
                };
                self.metrics.record_rtt(sample.rtt_us);
                let event = self.clock.record(from, sample);
                if matches!(event, EstimatorEvent::Synced | EstimatorEvent::Resynced)
                    && self.clock.announces_sync()
                {
                    self.out.push(Output::Emit(Event::TimeSynced));
                }
            }
            (_, ClockPacket::Ping { .. }) => {}
        }
    }

    fn on_wake(&mut self) {
        let robot = self.roster.robot.clone();
        if let Some((ping, robot)) = self.clock.poll_ping(self.now_us, robot.as_deref()) {
            self.metrics.record_ping_sent();
            self.out.push(clock_packet(ping, robot));
        }
        if let Some(at) = self.next_heartbeat_us.filter(|at| self.now_us >= *at) {
            self.announce_presence();
            self.next_heartbeat_us = Some(at + PRESENCE_HEARTBEAT_US);
        }
        let silent: Vec<PeerId> = self
            .last_heard_us
            .iter()
            .filter(|(_, at)| self.now_us >= **at + PRESENCE_TIMEOUT_US)
            .map(|(peer, _)| peer.clone())
            .collect();
        for peer in silent {
            self.peer_left(&peer);
        }
        let expired: Vec<OpId> = self
            .pending
            .iter()
            .filter(|(_, op)| {
                matches!(op, PendingOp::Call { deadline_us: Some(at), .. } if self.now_us >= *at)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.pending.remove(&id);
            let result = Err(RpcError::new(RPC_RESPONSE_TIMEOUT, "Response timeout", None));
            self.out.push(Output::Emit(Event::RpcResult { id, result }));
        }
    }

    fn on_polyfill_presence(&mut self, from: &str, presence: Presence) {
        self.peers.insert(from.to_string());
        self.last_heard_us.insert(from.to_string(), self.now_us);
        self.update_roster(from, &presence);
    }

    fn on_control(&mut self, from: &str, message: ControlMessage) {
        let local = self.local_peer.as_deref();
        match message {
            ControlMessage::RpcRequest { id, to, method, payload, .. }
                if Some(to.as_str()) == local =>
            {
                let caller = Caller::Control { id, from: from.to_string() };
                self.on_call(caller, from, &method, payload);
            }
            ControlMessage::RpcResponse { id, to, ok, error } if Some(to.as_str()) == local => {
                let id = OpId(id);
                // Only the peer that was called may answer.
                let expected =
                    matches!(self.pending.get(&id), Some(PendingOp::Call { to, .. }) if to == from);
                if !expected {
                    return;
                }
                self.pending.remove(&id);
                let result = match (ok, error) {
                    (_, Some(e)) => Err(e.into()),
                    (ok, None) => Ok(ok.unwrap_or_default()),
                };
                self.out.push(Output::Emit(Event::RpcResult { id, result }));
            }
            _ => {}
        }
    }

    fn on_rpc_request(&mut self, request: RpcRequest<'_>) {
        let RpcRequest { id, method, from, payload } = request;
        self.on_call(Caller::Native(id), from, method, payload.to_string());
    }

    fn on_call(&mut self, caller: Caller, from: &str, method: &str, payload: String) {
        if method != SET_ACTIVE_OPERATOR_RPC {
            match caller {
                // Native RPC delivers application methods to the driver's
                // own handlers, never here.
                Caller::Native(_) => self.reply(caller, Err(unsupported_method())),
                Caller::Control { .. } => {
                    let id = RpcId(self.next_inbound);
                    self.next_inbound += 1;
                    self.inbound.insert(id, caller);
                    self.out.push(Output::Emit(Event::RpcRequest {
                        id,
                        from: from.to_string(),
                        method: method.to_string(),
                        payload,
                    }));
                }
            }
            return;
        }
        if self.config.role() != Role::Robot {
            self.reply(caller, Err(unsupported_method()));
            return;
        }
        // Checked on every step until the caller shows up or the wait ends.
        self.waiting_rpcs.push(WaitingRpc {
            caller,
            from: from.to_string(),
            payload,
            deadline_us: self.now_us.saturating_add(RPC_CALLER_ROSTER_WAIT_US),
        });
    }

    /// Admit waiting `set_active_operator` callers that are now operators or
    /// observers, and refuse the ones whose wait is over.
    fn poll_waiting_rpcs(&mut self) {
        if self.waiting_rpcs.is_empty() {
            return;
        }
        for w in std::mem::take(&mut self.waiting_rpcs) {
            if self.roster.may_steer(&w.from) {
                if self.local_peer.is_none() {
                    let e = RpcError::new(RPC_NOT_CONNECTED, "robot not connected", None);
                    self.reply(w.caller, Err(e));
                    continue;
                }
                let identity = (!w.payload.is_empty()).then_some(w.payload);
                self.change_active_operator(identity, Some(w.caller));
            } else if self.now_us >= w.deadline_us {
                self.reply(
                    w.caller,
                    Err(RpcError::new(
                        RPC_NOT_AUTHORIZED,
                        "only operators and observers may set the active operator",
                        None,
                    )),
                );
            } else {
                self.waiting_rpcs.push(w);
            }
        }
    }

    /// Robot side. With native presence the pointer moves only once the
    /// transport has announced it, so a failed announcement changes
    /// nothing; a polyfilled announcement can't fail, so it moves at once.
    fn change_active_operator(&mut self, identity: Option<PeerId>, caller: Option<Caller>) {
        let prev = self.roster.active_operator.clone();
        if !self.caps.native_presence {
            self.commit_active_operator(identity, prev, caller);
            self.announce_presence();
            return;
        }
        let presence = Presence { active_operator: identity.clone(), ..self.local_presence() };
        let id = self.op(PendingOp::SetActive { identity, prev, caller });
        self.out.push(Output::SetPresence { id: Some(id), presence });
    }

    fn commit_active_operator(
        &mut self,
        identity: Option<PeerId>,
        prev: Option<PeerId>,
        caller: Option<Caller>,
    ) {
        self.roster.active_operator = identity.clone();
        if prev != identity {
            self.out.push(Output::Emit(Event::ActiveOperatorChanged(identity)));
        }
        if let Some(caller) = caller {
            self.reply(caller, Ok(String::new()));
        }
    }

    fn on_presence_set(&mut self, id: OpId, result: Result<(), String>) {
        let Some(PendingOp::SetActive { identity, prev, caller }) = self.pending.remove(&id) else {
            return;
        };
        match result {
            Ok(()) => self.commit_active_operator(identity, prev, caller),
            Err(e) => {
                if let Some(caller) = caller {
                    let message = format!("presence update failed: {e}");
                    self.reply(caller, Err(RpcError::new(RPC_PRESENCE_FAILED, message, None)));
                }
            }
        }
    }

    fn start_call(&mut self, call: Call) -> OpId {
        let Call { to, method, payload, timeout_ms } = call;
        if self.caps.native_rpc {
            let id = self.op(PendingOp::Call { to: to.clone(), deadline_us: None });
            self.out.push(Output::Rpc { id, to, method, payload, timeout_ms });
            return id;
        }
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_RPC_TIMEOUT_MS);
        let deadline_us = self.now_us.saturating_add(timeout_ms as u64 * 1_000);
        let id = self.op(PendingOp::Call { to: to.clone(), deadline_us: Some(deadline_us) });
        let message =
            ControlMessage::RpcRequest { id: id.0, to: to.clone(), method, payload, timeout_ms };
        self.out.push(Output::Publish {
            stream: Stream::Control,
            payload: message.encode(),
            delivery: Delivery::Reliable,
            to: Some(to),
        });
        id
    }

    fn reply(&mut self, caller: Caller, result: Result<String, RpcError>) {
        match caller {
            Caller::Native(id) => self.out.push(Output::RpcReply { id, result }),
            Caller::Control { id, from } => {
                let message = ControlMessage::response(id, from.clone(), result);
                self.out.push(Output::Publish {
                    stream: Stream::Control,
                    payload: message.encode(),
                    delivery: Delivery::Reliable,
                    to: Some(from),
                });
            }
        }
    }

    /// Default RPC destination: the obvious counterpart, else the only
    /// other peer.
    fn resolve_peer(&self) -> PortalResult<PeerId> {
        if let Some(peer) = self.default_peer() {
            return Ok(peer);
        }
        let mut peers = self.peers.iter();
        match (peers.next(), peers.next()) {
            (None, _) => Err(PortalError::NoPeer),
            (Some(peer), None) => Ok(peer.clone()),
            (Some(_), Some(_)) => Err(PortalError::AmbiguousPeer),
        }
    }

    fn op(&mut self, op: PendingOp) -> OpId {
        let id = OpId(self.next_op);
        self.next_op += 1;
        self.pending.insert(id, op);
        id
    }

    fn schedule_wake(&mut self) {
        let next = self.next_wake_us();
        if next == self.scheduled_wake_us {
            return;
        }
        self.scheduled_wake_us = next;
        self.out.push(Output::ScheduleWake { at_us: next });
    }
}

struct Call {
    to: PeerId,
    method: String,
    payload: String,
    timeout_ms: Option<u32>,
}

fn unsupported_method() -> RpcError {
    RpcError::new(RPC_UNSUPPORTED_METHOD, "Method not supported at destination", None)
}

/// Observations go out one by one so a slow consumer sees each; drops go
/// out as one batch, after them.
fn emit_sync<F>(out: &mut Vec<Output<F>>, synced: SyncOutput<F>) {
    let SyncOutput { observations, drops } = synced;
    out.extend(observations.into_iter().map(|o| Output::Emit(Event::Observation(o))));
    if !drops.is_empty() {
        out.push(Output::Emit(Event::Drops(drops)));
    }
}

fn clock_packet<F>(packet: ClockPacket, to: PeerId) -> Output<F> {
    Output::Publish {
        stream: Stream::Clock,
        payload: packet.encode(),
        // Retransmits would inflate the round trip and skew the offset.
        delivery: Delivery::BestEffort { max_age_ms: CLOCK_MAX_AGE_MS },
        to: Some(to),
    }
}

fn warn_mismatch(warned: &mut HashSet<u32>, stream: Stream, expected: u32, got: u32) {
    if warned.insert(got) {
        log::warn!(
            "[schema-mismatch] stream '{}': peer schema 0x{got:08x} != ours 0x{expected:08x}, dropping packet",
            stream.name()
        );
    }
}

fn reliability(reliable: bool) -> &'static str {
    match reliable {
        true => "reliable",
        false => "unreliable",
    }
}

fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(native: bool) -> TransportCaps {
        TransportCaps {
            video: VideoCaps::ByteFramesOnly,
            native_rpc: native,
            native_presence: native,
            e2ee: false,
            max_payload_bytes: 64 * 1024,
        }
    }

    fn connected_robot(native: bool) -> PortalCore<VideoFrameData> {
        let config = PortalConfig::new("s", Role::Robot);
        let mut core = PortalCore::new(CoreInit { config, caps: caps(native) });
        let peers = vec![("teleop".to_string(), Presence::local(Role::Operator))];
        core.connect(Connect {
            local_peer: "robot".into(),
            active_operator: None,
            peers,
            now_us: 0,
        })
        .expect("connect");
        core
    }

    fn keypoint_from(core: &mut PortalCore<VideoFrameData>, payload: &[u8]) -> Vec<String> {
        let input = Input::Data { stream: Stream::Keypoint, from: Some("teleop"), payload };
        core.handle(Step { input, now_us: 1 })
            .into_iter()
            .filter_map(|o| match o {
                Output::Emit(Event::Keypoint(kp)) => Some(kp.kind),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn slot_warns_once_per_sender_and_keeps_delivering() {
        let mut core = connected_robot(true);
        assert!(keypoint_from(&mut core, b"garbage").is_empty());
        assert!(keypoint_from(&mut core, b"garbage").is_empty());
        let good = keypoint::encode("idle", &Map::new(), 1);
        assert_eq!(keypoint_from(&mut core, &good), vec!["idle".to_string()]);
        assert_eq!(core.keypoint_warned.len(), 1);
    }

    #[test]
    fn wake_is_cancelled_once_nothing_is_pending() {
        let mut core = connected_robot(true);
        let out = core.handle(Step {
            input: Input::RpcRequest(RpcRequest {
                id: RpcId(0),
                method: SET_ACTIVE_OPERATOR_RPC,
                from: "stranger",
                payload: "",
            }),
            now_us: 10,
        });
        assert!(out.iter().any(|o| matches!(
            o,
            Output::ScheduleWake { at_us: Some(at) } if *at == 10 + RPC_CALLER_ROSTER_WAIT_US
        )));
        let out = core.handle(Step { input: Input::Wake, now_us: 10 + RPC_CALLER_ROSTER_WAIT_US });
        assert!(out.iter().any(|o| matches!(o, Output::ScheduleWake { at_us: None })));
        assert_eq!(core.next_wake_us(), None);
    }

    #[test]
    fn native_transports_get_no_polyfill_traffic() {
        let mut core = connected_robot(true);
        let (_, out) = core
            .set_active_operator(SetActiveOperator { identity: Some("teleop".into()), now_us: 5 })
            .expect("connected");
        assert!(matches!(&out[..], [Output::SetPresence { id: Some(_), presence }]
            if presence.active_operator.as_deref() == Some("teleop")));
        for now_us in [1_000_000, 10_000_000] {
            let out = core.handle(Step { input: Input::Wake, now_us });
            assert!(!out.iter().any(|o| matches!(o, Output::Publish { .. })), "{out:?}");
        }
    }

    #[test]
    fn a_failed_presence_update_leaves_the_pointer_alone() {
        let mut core = connected_robot(true);
        let (_, out) = core
            .set_active_operator(SetActiveOperator { identity: Some("teleop".into()), now_us: 5 })
            .expect("connected");
        let Some(Output::SetPresence { id: Some(id), .. }) = out.into_iter().next() else {
            panic!("expected a presence update");
        };
        let input = Input::PresenceSet { id, result: Err("denied".into()) };
        let out = core.handle(Step { input, now_us: 6 });
        assert!(!out.iter().any(|o| matches!(o, Output::Emit(_))));
        assert_eq!(core.active_operator(), None);
    }

    #[test]
    fn polyfilled_presence_heartbeats_and_times_out_silent_peers() {
        let mut core = connected_robot(false);
        let op = encode_presence(&Presence::local(Role::Operator));
        let input = Input::Data { stream: Stream::Presence, from: Some("alice"), payload: &op };
        let out = core.handle(Step { input, now_us: 100 });
        assert!(
            out.iter().any(|o| matches!(o, Output::Emit(Event::OperatorJoined(p)) if p == "alice"))
        );

        let out = core.handle(Step { input: Input::Wake, now_us: PRESENCE_HEARTBEAT_US });
        let heartbeat =
            out.iter().any(|o| matches!(o, Output::Publish { stream: Stream::Presence, .. }));
        assert!(heartbeat, "{out:?}");
        assert!(core.operators().contains(&"alice".to_string()));

        let out = core.handle(Step { input: Input::Wake, now_us: 100 + PRESENCE_TIMEOUT_US });
        assert!(
            out.iter().any(|o| matches!(o, Output::Emit(Event::OperatorLeft(p)) if p == "alice"))
        );
        assert_eq!(core.operators(), vec!["teleop".to_string()]);
    }

    #[test]
    fn polyfilled_rpc_times_out() {
        let config = PortalConfig::new("s", Role::Operator);
        let mut core: PortalCore<VideoFrameData> =
            PortalCore::new(CoreInit { config, caps: caps(false) });
        core.connect(Connect {
            local_peer: "op".into(),
            active_operator: None,
            peers: vec![],
            now_us: 0,
        })
        .expect("connect");
        let (id, out) = core
            .perform_rpc(PerformRpc {
                to: Some("robot".into()),
                method: "ping".into(),
                payload: String::new(),
                timeout_ms: Some(100),
                now_us: 0,
            })
            .expect("connected");
        assert!(out.iter().any(|o| matches!(o, Output::Publish { stream: Stream::Control, .. })));
        // A response from someone who wasn't called is ignored.
        let forged = ControlMessage::response(id.0, "op".into(), Ok("hi".into())).encode();
        let input =
            Input::Data { stream: Stream::Control, from: Some("mallory"), payload: &forged };
        assert!(
            !core.handle(Step { input, now_us: 50 }).iter().any(|o| matches!(o, Output::Emit(_)))
        );
        let out = core.handle(Step { input: Input::Wake, now_us: 100_000 });
        assert!(out.iter().any(|o| matches!(o,
            Output::Emit(Event::RpcResult { id: got, result: Err(e) })
                if *got == id && e.code == RPC_RESPONSE_TIMEOUT)));
    }

    #[test]
    fn connect_validates_caps() {
        let mut config = PortalConfig::new("s", Role::Robot);
        config.set_e2ee_key(vec![1, 2, 3]);
        let mut core: PortalCore<VideoFrameData> =
            PortalCore::new(CoreInit { config, caps: caps(false) });
        let err = core
            .connect(Connect {
                local_peer: "r".into(),
                active_operator: None,
                peers: vec![],
                now_us: 0,
            })
            .unwrap_err();
        assert!(matches!(err, PortalError::Unsupported(_)));
        assert_eq!(core.local_identity(), None, "a refused connect leaves nothing behind");
    }

    #[test]
    fn a_transport_echoing_our_own_messages_changes_nothing() {
        let mut core = connected_robot(false);
        let kp = keypoint::encode("idle", &Map::new(), 1);
        let presence = encode_presence(&Presence::local(Role::Operator));
        for (stream, payload) in [(Stream::Keypoint, &kp), (Stream::Presence, &presence)] {
            let input = Input::Data { stream, from: Some("robot"), payload };
            let out = core.handle(Step { input, now_us: 1 });
            assert!(!out.iter().any(|o| matches!(o, Output::Emit(_))), "{out:?}");
        }
        assert!(!core.peers().contains(&"robot".to_string()));
    }

    #[test]
    fn disconnecting_fails_calls_in_flight() {
        for native in [false, true] {
            let config = PortalConfig::new("s", Role::Operator);
            let mut core: PortalCore<VideoFrameData> =
                PortalCore::new(CoreInit { config, caps: caps(native) });
            let connect = Connect {
                local_peer: "op".into(),
                active_operator: None,
                peers: vec![],
                now_us: 0,
            };
            core.connect(connect).expect("connect");
            let call = PerformRpc {
                to: Some("robot".into()),
                method: "m".into(),
                payload: String::new(),
                timeout_ms: None,
                now_us: 0,
            };
            let (id, _) = core.perform_rpc(call).expect("connected");
            let out = core.disconnect();
            assert!(out.iter().any(|o| matches!(o,
                Output::Emit(Event::RpcResult { id: got, result: Err(e) })
                    if *got == id && e.code == RPC_RECIPIENT_DISCONNECTED)));
            assert_eq!(core.next_wake_us(), None, "no timer outlives the connection");
        }
    }
}
