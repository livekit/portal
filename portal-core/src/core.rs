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
//! LiveKit room events plus the local time, and carries out the LiveKit
//! calls it returns.
//!
//! Times are raw local microseconds from the driver's clock. The core never
//! reads a clock, spawns a task or touches a socket.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::clock::{
    CLOCK_TOPIC, ClockOptions, ClockPacket, ClockSync, EstimatorEvent, Exchange, Sample,
};
use crate::codec::Codec;
use crate::config::{FieldSpec, PortalConfig};
use crate::data::{
    ACTION_TOPIC, ActionOrigin, ActionRecord, DataPublisher, STATE_TOPIC, SendMap, build_action,
    build_state,
};
use crate::error::{PortalError, PortalResult};
use crate::frame::FramePayload;
use crate::frame_video::FRAME_VIDEO_TOPIC;
use crate::keypoint::{self, KEYPOINT_TOPIC, Keypoint};
use crate::metrics::{DataStream, MetricsRegistry, PortalMetrics};
use crate::presence::{
    ACTIVE_OPERATOR_ATTR_KEY, ParticipantUpdate, Presence, PresenceEvent, active_operator_attr,
};
use crate::rpc::RpcError;
use crate::serialization::{
    DecodeError, action_fingerprint, deserialize_action, deserialize_values, schema_fingerprint,
};
use crate::sync_buffer::{SyncBuffer, SyncOutput};
use crate::types::{Action, ActionSubscription, Observation, Role, State, TypedValue};

/// RPC method registered by Robot-side Portals so any participant can request
/// a change to the active operator pointer. Payload is the new identity, or
/// the empty string to clear. Result is the empty string on success.
pub const SET_ACTIVE_OPERATOR_RPC: &str = "portal.set_active_operator";

/// App-level RPC error code (outside the SDK's 1001-1999 reserved range)
/// returned when the robot has not yet finished setting up its
/// LocalParticipant.
pub const RPC_NOT_CONNECTED: u32 = 2001;
/// App-level RPC error code returned when the SDK's `set_attributes` fails
/// while the robot is processing a `set_active_operator` request.
pub const RPC_SET_ATTRIBUTES_FAILED: u32 = 2002;
/// App-level RPC error code returned when `set_active_operator` comes from
/// a participant that is neither an operator nor an observer.
pub const RPC_NOT_AUTHORIZED: u32 = 2003;
/// How long the robot waits for an unrecognised RPC caller to appear in the
/// rosters. The caller's role attribute and its RPC travel separately, so a
/// peer that calls right after connecting can beat its own attribute here.
pub const RPC_CALLER_ROSTER_WAIT_US: u64 = 1_500_000;

/// An incoming RPC, numbered by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RpcId(pub u64);

/// An outgoing call the core started and wants the result of, numbered by
/// the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

/// A remote participant as LiveKit reports it.
#[derive(Debug, Clone)]
pub struct ParticipantInfo {
    pub identity: String,
    pub attributes: HashMap<String, String>,
}

/// One input plus the local time it happened at.
pub struct Step<'a, F> {
    pub input: Input<'a, F>,
    pub now_us: u64,
}

pub enum Input<'a, F> {
    /// The room is up and this peer's role attributes are set.
    Connected(Connected),
    /// The connection ended, or a connect attempt is being rolled back.
    Disconnected,
    ParticipantConnected(ParticipantInfo),
    ParticipantDisconnected {
        identity: &'a str,
    },
    AttributesChanged {
        identity: &'a str,
        attributes: HashMap<String, String>,
    },
    DataReceived(DataReceived<'a>),
    /// A decoded frame from a subscribed track, timestamped by the sender.
    VideoFrame {
        track: &'a str,
        frame: F,
    },
    /// A call to one of Portal's own RPC methods.
    RpcRequest(RpcRequest<'a>),
    /// How a `PerformRpc` output ended.
    RpcResult {
        id: OpId,
        result: Result<String, RpcError>,
    },
    /// How a `SetAttributes` output with an id ended.
    AttributesSet {
        id: OpId,
        result: Result<(), String>,
    },
    Reconnected,
    /// The time asked for by `ScheduleWake` has come.
    Wake,
}

pub struct Connected {
    pub local_identity: String,
    /// The local participant's attributes, e.g. seeded by the token.
    pub local_attributes: HashMap<String, String>,
    pub participants: Vec<ParticipantInfo>,
}

pub struct DataReceived<'a> {
    pub topic: &'a str,
    pub payload: &'a [u8],
    /// `None` for packets the server sent.
    pub sender: Option<&'a str>,
}

pub struct RpcRequest<'a> {
    pub id: RpcId,
    pub method: &'a str,
    pub caller: &'a str,
    pub payload: &'a str,
}

#[derive(Debug)]
pub enum Output<F> {
    PublishData {
        topic: &'static str,
        payload: Vec<u8>,
        reliable: bool,
        /// Empty means everyone in the room.
        destinations: Vec<String>,
    },
    /// Set the local participant's attributes. With an id, report the
    /// outcome back as `Input::AttributesSet`.
    SetAttributes {
        id: Option<OpId>,
        attributes: HashMap<String, String>,
    },
    /// Call an RPC method on a peer and report back as `Input::RpcResult`.
    PerformRpc {
        id: OpId,
        destination: String,
        method: &'static str,
        payload: String,
        /// `None` for the SDK's default.
        timeout_ms: Option<u32>,
    },
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
    OperatorJoined(String),
    OperatorLeft(String),
    ActiveOperatorChanged(Option<String>),
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
    /// A published WebRTC track, with the timestamp in its frame metadata.
    WebRtc,
    /// One byte stream per frame on `FRAME_VIDEO_TOPIC`.
    ByteStream { codec: Codec, quality: u8 },
}

pub struct SetActiveOperator {
    /// `None` clears the pointer.
    pub identity: Option<String>,
}

enum PendingOp {
    /// Robot side: a local `set_active_operator`, or one requested over RPC.
    SetActive { identity: Option<String>, prev: Option<String>, rpc: Option<RpcId> },
    /// Operator side: the RPC asking the robot to move the pointer.
    AskRobot,
}

/// A `set_active_operator` RPC from a caller not yet in the rosters.
struct WaitingRpc {
    id: RpcId,
    caller: String,
    payload: String,
    deadline_us: u64,
}

pub struct PortalCore<F> {
    config: PortalConfig,
    /// Every video track name (WebRTC and frame video), in declaration order.
    track_names: Vec<String>,
    action_fp: u32,
    state_fp: u32,
    metrics: Arc<MetricsRegistry>,
    clock: ClockSync,
    presence: Presence,
    local_identity: Option<String>,
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
    waiting_rpcs: Vec<WaitingRpc>,
    scheduled_wake_us: Option<u64>,
    /// Local time of the step being handled.
    now_us: u64,
    /// Outputs of the step being handled.
    out: Vec<Output<F>>,
}

impl<F: FramePayload> PortalCore<F> {
    pub fn new(config: PortalConfig) -> Self {
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
            track_names,
            clock,
            presence: Presence::default(),
            local_identity: None,
            state_publisher: None,
            action_publisher: None,
            sync_buffer: None,
            action_mismatch_warned: HashSet::new(),
            state_mismatch_warned: HashSet::new(),
            keypoint_warned: HashSet::new(),
            pending: HashMap::new(),
            next_op: 0,
            waiting_rpcs: Vec::new(),
            scheduled_wake_us: None,
            now_us: 0,
            out: Vec::new(),
            config,
        }
    }

    pub fn handle(&mut self, step: Step<'_, F>) -> Vec<Output<F>> {
        let Step { input, now_us } = step;
        self.now_us = now_us;
        match input {
            Input::Connected(connected) => self.on_connected(connected),
            Input::Disconnected => self.on_disconnected(),
            Input::ParticipantConnected(p) => self.classify(&p.identity, &p.attributes),
            Input::ParticipantDisconnected { identity } => {
                log::info!("[{}] participant '{identity}' disconnected", self.config.session());
                for event in self.presence.disconnect(identity) {
                    self.presence_event(event);
                }
            }
            Input::AttributesChanged { identity, attributes } => {
                // Our own attribute updates echo back through this event.
                if self.local_identity.as_deref() != Some(identity) {
                    self.classify(identity, &attributes);
                }
            }
            Input::DataReceived(packet) => self.on_data(packet),
            Input::VideoFrame { track, frame } => self.on_video_frame(track, frame),
            Input::RpcRequest(request) => self.on_rpc_request(request),
            Input::RpcResult { id, .. } => {
                self.pending.remove(&id);
            }
            Input::AttributesSet { id, result } => self.on_attributes_set(id, result),
            Input::Reconnected => {
                log::info!(
                    "[{}] reconnected, clearing sync buffers and latest slots",
                    self.config.session()
                );
                if let Some(sb) = &mut self.sync_buffer {
                    sb.clear();
                }
                self.presence.clear_for_reconnect();
            }
            Input::Wake => self.on_wake(),
        }
        self.poll_waiting_rpcs();
        self.schedule_wake();
        std::mem::take(&mut self.out)
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
        Ok(vec![Output::PublishData {
            topic: STATE_TOPIC,
            payload: encoded.payload,
            reliable: self.config.state_reliable(),
            destinations: Vec::new(),
        }])
    }

    /// Publish an action (operator only), echoing it to this peer's own
    /// subscription: LiveKit does not fan out a publisher's own packets.
    pub fn send_action(&mut self, input: SendAction<'_>) -> PortalResult<Vec<Output<F>>> {
        let SendAction { values, timestamp_us, in_reply_to_ts_us, now_us } = input;
        let role = self.config.role();
        let publisher = self.action_publisher.as_mut().ok_or(PortalError::WrongRole(role))?;
        let send_ts = timestamp_us.unwrap_or_else(|| self.clock.now(now_us));
        let encoded =
            publisher.send_map(SendMap { values, timestamp_us: send_ts, in_reply_to_ts_us })?;
        self.metrics.bump_sent(DataStream::Action);
        let mut out = vec![Output::PublishData {
            topic: ACTION_TOPIC,
            payload: encoded.payload,
            reliable: self.config.action_reliable(),
            destinations: Vec::new(),
        }];
        // Echo what the subscription would have delivered from anyone else:
        // the wire values, post-carry-forward and post-saturation.
        let active = self.is_self_active();
        let echo = match self.config.action_subscription() {
            ActionSubscription::None => false,
            ActionSubscription::Active => active,
            ActionSubscription::All => true,
        };
        if let Some(sender) = self.local_identity.clone().filter(|_| echo) {
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

    /// Send an annotation to everyone in the room, this peer included.
    pub fn send_keypoint(&mut self, input: SendKeypoint<'_>) -> PortalResult<Vec<Output<F>>> {
        let SendKeypoint { kind, payload, timestamp_us, now_us } = input;
        let sender = self.local_identity.clone().ok_or(PortalError::NotConnected)?;
        let timestamp_us = timestamp_us.unwrap_or_else(|| self.clock.now(now_us));
        Ok(vec![
            Output::PublishData {
                topic: KEYPOINT_TOPIC,
                payload: keypoint::encode(kind, &payload, timestamp_us),
                reliable: true,
                destinations: Vec::new(),
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
                let transport = match spec.is_webrtc() {
                    true => VideoTransport::WebRtc,
                    false => {
                        VideoTransport::ByteStream { codec: spec.codec, quality: spec.quality }
                    }
                };
                Ok(VideoRoute { timestamp_us, transport })
            }
            (role, Some(_)) => Err(PortalError::WrongRole(role)),
            (_, None) => Err(PortalError::UnknownVideoTrack { name: track.to_string() }),
        }
    }

    /// Set the active operator. The robot moves its own pointer and
    /// broadcasts it as an attribute; others ask the robot over RPC.
    pub fn set_active_operator(
        &mut self,
        input: SetActiveOperator,
    ) -> PortalResult<Vec<Output<F>>> {
        let SetActiveOperator { identity } = input;
        match self.config.role() {
            Role::Robot => {
                if self.local_identity.is_none() {
                    return Err(PortalError::NotConnected);
                }
                let prev = self.presence.active_operator.clone();
                let id =
                    self.op(PendingOp::SetActive { identity: identity.clone(), prev, rpc: None });
                Ok(vec![active_operator_attributes(id, identity)])
            }
            Role::Operator | Role::Observer => {
                let robot = self.presence.robot_identity.clone().ok_or(PortalError::NoPeer)?;
                let id = self.op(PendingOp::AskRobot);
                Ok(vec![Output::PerformRpc {
                    id,
                    destination: robot,
                    method: SET_ACTIVE_OPERATOR_RPC,
                    payload: identity.unwrap_or_default(),
                    timeout_ms: None,
                }])
            }
        }
    }

    // --- Getters ---

    pub fn config(&self) -> &PortalConfig {
        &self.config
    }

    pub fn state_schema(&self) -> &[FieldSpec] {
        self.config.state_schema()
    }

    pub fn action_schema(&self) -> &[FieldSpec] {
        self.config.action_schema()
    }

    /// This peer's identity while connected.
    pub fn local_identity(&self) -> Option<&str> {
        self.local_identity.as_deref()
    }

    pub fn active_operator(&self) -> Option<String> {
        self.presence.active_operator.clone()
    }

    /// Connected operators (excluding self), sorted.
    pub fn operators(&self) -> Vec<String> {
        sorted(&self.presence.operators)
    }

    /// Connected observers (excluding self), sorted.
    pub fn observers(&self) -> Vec<String> {
        sorted(&self.presence.observers)
    }

    pub fn robot_identity(&self) -> Option<String> {
        self.presence.robot_identity.clone()
    }

    /// The obvious RPC counterpart: the robot for an operator or observer,
    /// the active operator for the robot. `None` if not known yet.
    pub fn default_peer(&self) -> Option<String> {
        match self.config.role() {
            Role::Operator | Role::Observer => self.presence.robot_identity.clone(),
            Role::Robot => self.presence.active_operator.clone(),
        }
    }

    /// Whether a byte stream on `topic` is Portal's to read.
    pub fn accepts_byte_stream(&self, topic: &str) -> bool {
        self.config.role() != Role::Robot
            && topic == FRAME_VIDEO_TOPIC
            && self.config.frame_video_tracks().next().is_some()
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
        let rpc = self.waiting_rpcs.iter().map(|w| w.deadline_us).min();
        match (self.clock.next_ping_us(), rpc) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    // --- Internal ---

    fn on_connected(&mut self, connected: Connected) {
        let Connected { local_identity, local_attributes, participants } = connected;
        let role = self.config.role();
        self.local_identity = Some(local_identity);
        // A token can seed the pointer, so the gate sees it before anyone
        // calls `set_active_operator`.
        if role == Role::Robot && local_attributes.contains_key(ACTIVE_OPERATOR_ATTR_KEY) {
            self.presence.active_operator = active_operator_attr(&local_attributes);
        }
        for p in &participants {
            self.classify(&p.identity, &p.attributes);
        }
        let session = self.config.session().to_string();
        match role {
            Role::Robot => {
                for spec in self.config.frame_video_tracks() {
                    log::info!(
                        "[{session}] ready to publish frame-video track '{}' via byte stream (codec={:?}, quality={})",
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
        self.clock.start_pinging(self.now_us);
    }

    fn on_disconnected(&mut self) {
        self.local_identity = None;
        self.presence.clear();
        self.state_publisher = None;
        self.action_publisher = None;
        if let Some(mut sb) = self.sync_buffer.take() {
            sb.clear();
        }
        self.clock.stop_pinging();
        self.pending.clear();
        for w in self.waiting_rpcs.drain(..) {
            self.out.push(Output::RpcReply {
                id: w.id,
                result: Err(RpcError::new(RPC_NOT_CONNECTED, "robot not connected", None)),
            });
        }
    }

    fn classify(&mut self, identity: &str, attributes: &HashMap<String, String>) {
        let update = ParticipantUpdate { self_role: self.config.role(), identity, attributes };
        if let Some(event) = self.presence.classify_and_update(update) {
            self.presence_event(event);
        }
    }

    fn presence_event(&mut self, event: PresenceEvent) {
        let event = match event {
            PresenceEvent::OperatorJoined(id) => Event::OperatorJoined(id),
            PresenceEvent::OperatorLeft(id) => Event::OperatorLeft(id),
            PresenceEvent::ActiveOperatorChanged(id) => Event::ActiveOperatorChanged(id),
            PresenceEvent::RobotLeft => {
                // Whoever answers next may be a restarted robot with a new
                // clock, so the estimate starts over.
                self.clock.restart();
                return;
            }
        };
        self.out.push(Output::Emit(event));
    }

    fn is_self_active(&self) -> bool {
        match (&self.local_identity, &self.presence.active_operator) {
            (Some(local), Some(active)) => local == active,
            _ => false,
        }
    }

    fn on_data(&mut self, packet: DataReceived<'_>) {
        let DataReceived { topic, payload, sender } = packet;
        match topic {
            CLOCK_TOPIC => {
                if let Some(sender) = sender {
                    self.on_clock_packet(payload, sender);
                }
            }
            KEYPOINT_TOPIC => {
                let Some(sender) = sender.filter(|s| self.presence.is_known_peer(s)) else {
                    return;
                };
                match keypoint::decode(payload, sender.to_string()) {
                    Ok(kp) => self.out.push(Output::Emit(Event::Keypoint(kp))),
                    Err(e) => {
                        if self.keypoint_warned.insert(sender.to_string()) {
                            log::warn!("[bad-payload] keypoint from '{sender}' is not valid: {e}");
                        }
                    }
                }
            }
            ACTION_TOPIC => {
                let Some(origin) = self.gate_action(sender) else {
                    return;
                };
                self.on_action(payload, origin);
            }
            STATE_TOPIC if self.config.role() != Role::Robot => self.on_state(payload),
            _ => {}
        }
    }

    /// The active-operator gate for an incoming action packet. Returns who
    /// sent it and whether they were active, or `None` to drop it before
    /// decoding.
    ///
    /// The robot takes only the active operator's actions. An operator takes
    /// what its subscription asks for, so a controller with `None` pays
    /// nothing on the receive path. In every case the sender must be a
    /// recognised operator: a pointer seeded from a token or set over RPC
    /// can name any identity.
    fn gate_action(&self, sender: Option<&str>) -> Option<ActionOrigin> {
        let subscription = match self.config.role() {
            Role::Robot => ActionSubscription::Active,
            Role::Operator | Role::Observer => self.config.action_subscription(),
        };
        if subscription == ActionSubscription::None {
            return None;
        }
        let sender = sender?;
        if !self.presence.operators.contains(sender) {
            return None;
        }
        let active = self.presence.active_operator.as_deref() == Some(sender);
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
                warn_mismatch(&mut self.action_mismatch_warned, ACTION_TOPIC, expected, got)
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
                warn_mismatch(&mut self.state_mismatch_warned, STATE_TOPIC, expected, got)
            }
            Err(DecodeError::Malformed(e)) => {
                log::warn!("[bad-payload] state deserialize failed: {e}");
            }
        }
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

    fn on_clock_packet(&mut self, payload: &[u8], sender: &str) {
        let Some(packet) = ClockPacket::decode(payload) else {
            return;
        };
        match (self.config.role(), packet) {
            (Role::Robot, ClockPacket::Ping { seq, t1 }) => {
                // Received and answered within the same step, so t2 == t3.
                let pong = ClockPacket::Pong { seq, t1, t2: self.now_us, t3: self.now_us };
                self.out.push(clock_packet(pong, sender.to_string()));
            }
            (Role::Robot, ClockPacket::Pong { .. }) => {}
            (_, ClockPacket::Pong { t1, t2, t3, .. }) => {
                if self.local_identity.is_none()
                    || self.presence.robot_identity.as_deref() != Some(sender)
                {
                    return;
                }
                let Ok(sample) = Sample::try_from(Exchange { t1, t2, t3, t4: self.now_us }) else {
                    return;
                };
                self.metrics.record_rtt(sample.rtt_us);
                let event = self.clock.record(sender, sample);
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
        let robot = self.presence.robot_identity.clone();
        if let Some((ping, robot)) = self.clock.poll_ping(self.now_us, robot.as_deref()) {
            self.metrics.record_ping_sent();
            self.out.push(clock_packet(ping, robot));
        }
    }

    fn on_rpc_request(&mut self, request: RpcRequest<'_>) {
        let RpcRequest { id, method, caller, payload } = request;
        if method != SET_ACTIVE_OPERATOR_RPC || self.config.role() != Role::Robot {
            self.out.push(Output::RpcReply {
                id,
                result: Err(RpcError::new(1400, "Method not supported at destination", None)),
            });
            return;
        }
        // Checked on every step until the caller shows up or the wait ends.
        self.waiting_rpcs.push(WaitingRpc {
            id,
            caller: caller.to_string(),
            payload: payload.to_string(),
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
            if self.presence.may_steer(&w.caller) {
                self.start_rpc_set_active(w);
            } else if self.now_us >= w.deadline_us {
                self.out.push(Output::RpcReply {
                    id: w.id,
                    result: Err(RpcError::new(
                        RPC_NOT_AUTHORIZED,
                        "only operators and observers may set the active operator",
                        None,
                    )),
                });
            } else {
                self.waiting_rpcs.push(w);
            }
        }
    }

    fn start_rpc_set_active(&mut self, w: WaitingRpc) {
        if self.local_identity.is_none() {
            self.out.push(Output::RpcReply {
                id: w.id,
                result: Err(RpcError::new(RPC_NOT_CONNECTED, "robot not connected", None)),
            });
            return;
        }
        let identity = (!w.payload.is_empty()).then_some(w.payload);
        let prev = self.presence.active_operator.clone();
        let op =
            self.op(PendingOp::SetActive { identity: identity.clone(), prev, rpc: Some(w.id) });
        self.out.push(active_operator_attributes(op, identity));
    }

    fn on_attributes_set(&mut self, id: OpId, result: Result<(), String>) {
        let Some(PendingOp::SetActive { identity, prev, rpc }) = self.pending.remove(&id) else {
            return;
        };
        if let Err(e) = result {
            if let Some(rpc) = rpc {
                self.out.push(Output::RpcReply {
                    id: rpc,
                    result: Err(RpcError::new(
                        RPC_SET_ATTRIBUTES_FAILED,
                        format!("set_attributes failed: {e}"),
                        None,
                    )),
                });
            }
            return;
        }
        self.presence.active_operator = identity.clone();
        if prev != identity {
            self.out.push(Output::Emit(Event::ActiveOperatorChanged(identity)));
        }
        if let Some(rpc) = rpc {
            self.out.push(Output::RpcReply { id: rpc, result: Ok(String::new()) });
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

/// Observations go out one by one so a slow consumer sees each; drops go
/// out as one batch, after them.
fn emit_sync<F>(out: &mut Vec<Output<F>>, synced: SyncOutput<F>) {
    let SyncOutput { observations, drops } = synced;
    out.extend(observations.into_iter().map(|o| Output::Emit(Event::Observation(o))));
    if !drops.is_empty() {
        out.push(Output::Emit(Event::Drops(drops)));
    }
}

fn clock_packet<F>(packet: ClockPacket, destination: String) -> Output<F> {
    Output::PublishData {
        topic: CLOCK_TOPIC,
        payload: packet.encode(),
        // Retransmits would inflate the round trip and skew the offset.
        reliable: false,
        destinations: vec![destination],
    }
}

fn active_operator_attributes<F>(id: OpId, identity: Option<String>) -> Output<F> {
    Output::SetAttributes {
        id: Some(id),
        attributes: HashMap::from([(
            ACTIVE_OPERATOR_ATTR_KEY.to_string(),
            identity.unwrap_or_default(),
        )]),
    }
}

fn warn_mismatch(warned: &mut HashSet<u32>, topic: &str, expected: u32, got: u32) {
    if warned.insert(got) {
        log::warn!(
            "[schema-mismatch] topic '{topic}': peer schema 0x{got:08x} != ours 0x{expected:08x}, dropping packet"
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
    use crate::presence::role_attributes;
    use crate::types::VideoFrameData;

    use super::*;

    fn connected_robot() -> PortalCore<VideoFrameData> {
        let mut core = PortalCore::new(PortalConfig::new("s", Role::Robot));
        let teleop = ParticipantInfo {
            identity: "teleop".into(),
            attributes: role_attributes(Role::Operator),
        };
        core.handle(Step {
            input: Input::Connected(Connected {
                local_identity: "robot".into(),
                local_attributes: HashMap::new(),
                participants: vec![teleop],
            }),
            now_us: 0,
        });
        core
    }

    fn keypoint_from(core: &mut PortalCore<VideoFrameData>, payload: &[u8]) -> Vec<String> {
        let packet = DataReceived { topic: KEYPOINT_TOPIC, payload, sender: Some("teleop") };
        let input = Input::DataReceived(packet);
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
        let mut core = connected_robot();
        assert!(keypoint_from(&mut core, b"garbage").is_empty());
        assert!(keypoint_from(&mut core, b"garbage").is_empty());
        let good = keypoint::encode("idle", &Map::new(), 1);
        assert_eq!(keypoint_from(&mut core, &good), vec!["idle".to_string()]);
        assert_eq!(core.keypoint_warned.len(), 1);
    }

    #[test]
    fn wake_is_cancelled_once_nothing_is_pending() {
        let mut core = connected_robot();
        let out = core.handle(Step {
            input: Input::RpcRequest(RpcRequest {
                id: RpcId(0),
                method: SET_ACTIVE_OPERATOR_RPC,
                caller: "stranger",
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
}
