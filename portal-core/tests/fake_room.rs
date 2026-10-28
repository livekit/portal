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

//! Several `PortalCore`s in an in-memory room: each one's outputs become
//! the others' inputs, on a fake clock with optional latency, jitter, loss
//! and reordering. No server, no tasks.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};

use bytes::Bytes;
use portal_core::frame_video::{FrameVideoTracks, RgbFrame, build_frame_payload};
use portal_core::presence::role_attributes;
use portal_core::{
    ACTIVE_OPERATOR_ATTR_KEY, ActionSubscription, Codec, Connected, DType, DataReceived, Event,
    FrameSource, Input, OpId, Output, PROTOCOL_VERSION_ATTR_KEY, ParticipantInfo, PortalConfig,
    PortalCore, PortalError, RPC_CALLER_ROSTER_WAIT_US, RPC_NOT_AUTHORIZED, Role, RpcError, RpcId,
    RpcRequest, SET_ACTIVE_OPERATOR_RPC, SendAction, SendKeypoint, SendState, SendVideoFrame,
    SetActiveOperator, StallBehavior, Step, TypedValue, VideoFrameData, VideoOptions,
    VideoTransport,
};

const MS: u64 = 1_000;
const S: u64 = 1_000_000;
const START: u64 = 1_000 * S;

type Core = PortalCore<VideoFrameData>;

#[derive(Clone, Copy, Default)]
struct Link {
    latency_us: u64,
    /// Uniform extra delay in `0..jitter_us`. Unreliable packets may reorder.
    jitter_us: u64,
    /// Chance in `0..1` that an unreliable packet is lost.
    loss: f64,
}

enum Delivery {
    Data { topic: &'static str, payload: Vec<u8>, sender: String },
    FrameVideo { payload: Vec<u8> },
    Frame { track: String, frame: VideoFrameData },
    Attributes { identity: String, attributes: HashMap<String, String> },
    Joined(ParticipantInfo),
    Left(String),
    RpcRequest { id: RpcId, caller: String, payload: String, reply_to: (String, OpId) },
    RpcResult { id: OpId, result: Result<String, RpcError> },
}

struct Peer {
    core: Core,
    attributes: HashMap<String, String>,
    connected: bool,
    skew_us: i64,
    wake_at: Option<u64>,
    events: Vec<Event<VideoFrameData>>,
    /// Who an incoming RPC came from, to route the reply.
    rpc_callers: HashMap<RpcId, (String, OpId)>,
    frame_video: FrameVideoTracks,
}

struct Room {
    now: u64,
    link: Link,
    peers: BTreeMap<String, Peer>,
    queue: BinaryHeap<Reverse<(u64, u64, String)>>,
    deliveries: HashMap<u64, Delivery>,
    /// Latest delivery time per (from, to), so reliable traffic stays ordered.
    ordered: HashMap<(String, String), u64>,
    seq: u64,
    rng: u64,
    next_rpc: u64,
}

impl Room {
    fn new(link: Link) -> Self {
        Self {
            now: START,
            link,
            peers: BTreeMap::new(),
            queue: BinaryHeap::new(),
            deliveries: HashMap::new(),
            ordered: HashMap::new(),
            seq: 0,
            rng: 0x2545_F491_4F6C_DD1D,
            next_rpc: 0,
        }
    }

    fn random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn local_time(&self, id: &str) -> u64 {
        self.now.saturating_add_signed(self.peers[id].skew_us)
    }

    fn add(&mut self, id: &str, config: PortalConfig) {
        self.add_skewed(id, config, 0);
    }

    fn add_skewed(&mut self, id: &str, config: PortalConfig, skew_us: i64) {
        let frame_video = FrameVideoTracks::new(&config);
        let peer = Peer {
            attributes: role_attributes(config.role()),
            core: Core::new(config),
            connected: false,
            skew_us,
            wake_at: None,
            events: Vec::new(),
            rpc_callers: HashMap::new(),
            frame_video,
        };
        self.peers.insert(id.to_string(), peer);
    }

    /// Connect `id`: role attributes are set, then the core sees the room.
    fn join(&mut self, id: &str) {
        self.join_with(id, HashMap::new());
    }

    fn join_with(&mut self, id: &str, token_attributes: HashMap<String, String>) {
        let participants: Vec<ParticipantInfo> = self
            .peers
            .iter()
            .filter(|(other, p)| p.connected && other.as_str() != id)
            .map(|(other, p)| ParticipantInfo {
                identity: other.clone(),
                attributes: p.attributes.clone(),
            })
            .collect();
        let peer = self.peers.get_mut(id).expect("peer");
        peer.attributes.extend(token_attributes);
        peer.connected = true;
        let info =
            ParticipantInfo { identity: id.to_string(), attributes: peer.attributes.clone() };
        let input = Input::Connected(Connected {
            local_identity: id.to_string(),
            local_attributes: peer.attributes.clone(),
            participants,
        });
        self.feed(id, input);
        self.broadcast(id, |_| Delivery::Joined(info.clone()), true);
    }

    fn leave(&mut self, id: &str) {
        self.feed(id, Input::Disconnected);
        self.peers.get_mut(id).expect("peer").connected = false;
        self.broadcast(id, |_| Delivery::Left(id.to_string()), true);
    }

    fn feed(&mut self, id: &str, input: Input<'_, VideoFrameData>) {
        let now_us = self.local_time(id);
        let out = self.peers.get_mut(id).expect("peer").core.handle(Step { input, now_us });
        self.execute(id, out);
    }

    fn with_core<T>(&mut self, id: &str, f: impl FnOnce(&mut Core, u64) -> T) -> T {
        let now_us = self.local_time(id);
        f(&mut self.peers.get_mut(id).expect("peer").core, now_us)
    }

    fn execute(&mut self, from: &str, out: Vec<Output<VideoFrameData>>) {
        for o in out {
            match o {
                Output::PublishData { topic, payload, reliable, destinations } => {
                    let targets: Vec<String> = match destinations.is_empty() {
                        true => self.others(from),
                        false => destinations,
                    };
                    for to in targets {
                        let delivery = Delivery::Data {
                            topic,
                            payload: payload.clone(),
                            sender: from.to_string(),
                        };
                        self.send(from, &to, delivery, reliable);
                    }
                }
                Output::SetAttributes { id, attributes } => {
                    let peer = self.peers.get_mut(from).expect("peer");
                    peer.attributes.extend(attributes);
                    let all = peer.attributes.clone();
                    let identity = from.to_string();
                    self.broadcast(
                        from,
                        |_| Delivery::Attributes {
                            identity: identity.clone(),
                            attributes: all.clone(),
                        },
                        true,
                    );
                    // LiveKit echoes a participant's own attribute change back.
                    self.feed(
                        from,
                        Input::AttributesChanged { identity: from, attributes: all.clone() },
                    );
                    if let Some(id) = id {
                        self.feed(from, Input::AttributesSet { id, result: Ok(()) });
                    }
                }
                Output::PerformRpc { id, destination, method, payload, .. } => {
                    assert_eq!(method, SET_ACTIVE_OPERATOR_RPC);
                    let rpc = RpcId(self.next_rpc);
                    self.next_rpc += 1;
                    let delivery = Delivery::RpcRequest {
                        id: rpc,
                        caller: from.to_string(),
                        payload,
                        reply_to: (from.to_string(), id),
                    };
                    self.send(from, &destination, delivery, true);
                }
                Output::RpcReply { id, result } => {
                    let peer = self.peers.get_mut(from).expect("peer");
                    let (caller, op) = peer.rpc_callers.remove(&id).expect("known rpc");
                    self.send(from, &caller, Delivery::RpcResult { id: op, result }, true);
                }
                Output::Emit(event) => self.peers.get_mut(from).expect("peer").events.push(event),
                Output::ScheduleWake { at_us } => {
                    let skew = self.peers[from].skew_us;
                    self.peers.get_mut(from).expect("peer").wake_at =
                        at_us.map(|at| at.saturating_add_signed(-skew));
                }
            }
        }
    }

    fn others(&self, from: &str) -> Vec<String> {
        self.peers
            .iter()
            .filter(|(id, p)| p.connected && id.as_str() != from)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn broadcast(&mut self, from: &str, make: impl Fn(&str) -> Delivery, reliable: bool) {
        for to in self.others(from) {
            let delivery = make(&to);
            self.send(from, &to, delivery, reliable);
        }
    }

    fn send(&mut self, from: &str, to: &str, delivery: Delivery, reliable: bool) {
        if !reliable && self.link.loss > 0.0 {
            let roll = (self.random() % 1_000_000) as f64 / 1_000_000.0;
            if roll < self.link.loss {
                return;
            }
        }
        let jitter = match self.link.jitter_us {
            0 => 0,
            j => self.random() % j,
        };
        let mut at = self.now + self.link.latency_us + jitter;
        if reliable {
            let key = (from.to_string(), to.to_string());
            let last = self.ordered.entry(key).or_default();
            at = at.max(*last);
            *last = at;
        }
        self.seq += 1;
        self.deliveries.insert(self.seq, delivery);
        self.queue.push(Reverse((at, self.seq, to.to_string())));
    }

    /// Run the room until `now + duration`.
    fn run(&mut self, duration: u64) {
        let end = self.now + duration;
        loop {
            let next_delivery = self.queue.peek().map(|Reverse((at, _, _))| *at);
            let next_wake =
                self.peers.iter().filter_map(|(id, p)| p.wake_at.map(|at| (at, id.clone()))).min();
            let next =
                [next_delivery, next_wake.as_ref().map(|(at, _)| *at)].into_iter().flatten().min();
            let Some(at) = next.filter(|at| *at <= end) else {
                break;
            };
            self.now = self.now.max(at);
            if next_delivery == Some(at) {
                let Reverse((_, seq, to)) = self.queue.pop().expect("peeked");
                let delivery = self.deliveries.remove(&seq).expect("queued");
                self.deliver(&to, delivery);
            } else if let Some((_, id)) = next_wake {
                let peer = self.peers.get_mut(&id).expect("peer");
                peer.wake_at = None;
                if peer.connected {
                    self.feed(&id, Input::Wake);
                }
            }
        }
        self.now = end;
    }

    fn deliver(&mut self, to: &str, delivery: Delivery) {
        if !self.peers[to].connected {
            return;
        }
        match delivery {
            Delivery::Data { topic, payload, sender } => self.feed(
                to,
                Input::DataReceived(DataReceived {
                    topic,
                    payload: &payload,
                    sender: Some(&sender),
                }),
            ),
            Delivery::FrameVideo { payload } => {
                let peer = &self.peers[to];
                if !peer.core.accepts_byte_stream(portal_core::FRAME_VIDEO_TOPIC) {
                    return;
                }
                let Some(received) = peer.frame_video.decode(Bytes::from(payload)) else {
                    return;
                };
                self.feed(
                    to,
                    Input::VideoFrame { track: &received.track_name, frame: received.frame },
                );
            }
            Delivery::Frame { track, frame } => {
                self.feed(to, Input::VideoFrame { track: &track, frame })
            }
            Delivery::Attributes { identity, attributes } => {
                self.feed(to, Input::AttributesChanged { identity: &identity, attributes })
            }
            Delivery::Joined(info) => self.feed(to, Input::ParticipantConnected(info)),
            Delivery::Left(identity) => {
                self.feed(to, Input::ParticipantDisconnected { identity: &identity })
            }
            Delivery::RpcRequest { id, caller, payload, reply_to } => {
                self.peers.get_mut(to).expect("peer").rpc_callers.insert(id, reply_to);
                self.feed(
                    to,
                    Input::RpcRequest(RpcRequest {
                        id,
                        method: SET_ACTIVE_OPERATOR_RPC,
                        caller: &caller,
                        payload: &payload,
                    }),
                );
            }
            Delivery::RpcResult { id, result } => self.feed(to, Input::RpcResult { id, result }),
        }
    }

    // --- Sends as a driver would make them ---

    fn send_state(&mut self, id: &str, values: &[(&str, f64)]) -> u64 {
        let values: HashMap<String, TypedValue> =
            values.iter().map(|(k, v)| (k.to_string(), TypedValue::F64(*v))).collect();
        let ts = self.with_core(id, |c, now_us| c.now_us(now_us));
        let out = self
            .with_core(id, |c, now_us| {
                c.send_state(SendState { values: &values, timestamp_us: Some(ts), now_us })
            })
            .expect("send_state");
        self.execute(id, out);
        ts
    }

    fn send_action(&mut self, id: &str, value: f64) -> Result<(), PortalError> {
        let values = HashMap::from([("v".to_string(), TypedValue::F64(value))]);
        let out = self.with_core(id, |c, now_us| {
            c.send_action(SendAction {
                values: &values,
                timestamp_us: None,
                in_reply_to_ts_us: None,
                now_us,
            })
        })?;
        self.execute(id, out);
        Ok(())
    }

    /// One 16x16 frame on `track`, through whichever transport it uses.
    fn send_frame(&mut self, id: &str, track: &str, timestamp_us: u64) {
        let route = self
            .with_core(id, |c, now_us| {
                c.send_video_frame(SendVideoFrame {
                    track,
                    timestamp_us: Some(timestamp_us),
                    now_us,
                })
            })
            .expect("route");
        let rgb: Vec<u8> = (0..16 * 16 * 3).map(|i| (i % 251) as u8).collect();
        match route.transport {
            VideoTransport::ByteStream { codec, quality } => {
                let payload = build_frame_payload(RgbFrame {
                    track_name: track,
                    codec,
                    width: 16,
                    height: 16,
                    timestamp_us: route.timestamp_us,
                    rgb: &rgb,
                    quality,
                })
                .expect("encodes");
                self.broadcast(id, |_| Delivery::FrameVideo { payload: payload.clone() }, true);
            }
            VideoTransport::WebRtc => {
                let frame = VideoFrameData {
                    width: 16,
                    height: 16,
                    data: Bytes::from(rgb),
                    timestamp_us: route.timestamp_us,
                    source: FrameSource::Live,
                };
                let track = track.to_string();
                self.broadcast(
                    id,
                    |_| Delivery::Frame { track: track.clone(), frame: frame.clone() },
                    false,
                );
            }
        }
    }

    fn set_active_operator(&mut self, id: &str, identity: Option<&str>) {
        let out = self
            .with_core(id, |c, _| {
                c.set_active_operator(SetActiveOperator { identity: identity.map(String::from) })
            })
            .expect("set_active_operator");
        self.execute(id, out);
    }

    fn events(&mut self, id: &str) -> Vec<Event<VideoFrameData>> {
        std::mem::take(&mut self.peers.get_mut(id).expect("peer").events)
    }

    fn core(&self, id: &str) -> &Core {
        &self.peers[id].core
    }
}

fn robot_config() -> PortalConfig {
    let mut config = PortalConfig::new("s", Role::Robot);
    config.add_state_typed([("j", DType::F64)]);
    config.add_action_typed([("v", DType::F64)]);
    config
}

fn operator_config(subscription: ActionSubscription) -> PortalConfig {
    let mut config = PortalConfig::new("s", Role::Operator);
    config.add_state_typed([("j", DType::F64)]);
    config.add_action_typed([("v", DType::F64)]);
    config.set_action_subscription(subscription);
    config
}

fn actions(events: &[Event<VideoFrameData>]) -> Vec<(String, f64, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Action(a) => Some((a.sender.clone(), a.raw_values["v"], a.active)),
            _ => None,
        })
        .collect()
}

fn observations(events: &[Event<VideoFrameData>]) -> Vec<&portal_core::Observation> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Observation(o) => Some(o),
            _ => None,
        })
        .collect()
}

fn link() -> Link {
    Link { latency_us: 5 * MS, ..Default::default() }
}

#[test]
fn state_and_mjpeg_track_become_observations() {
    let mut room = Room::new(link());
    let mut robot = robot_config();
    robot.add_video("cam", Codec::Mjpeg, VideoOptions::default());
    room.add("robot", robot);
    let mut op = operator_config(ActionSubscription::None);
    op.add_video("cam", Codec::Mjpeg, VideoOptions::default());
    op.set_observation_sync(true);
    room.add("op", op);
    room.join("robot");
    room.join("op");
    room.run(100 * MS);

    for i in 0..10 {
        let ts = room.send_state("robot", &[("j", i as f64)]);
        room.send_frame("robot", "cam", ts);
        room.run(33 * MS);
    }

    let events = room.events("op");
    let obs = observations(&events);
    assert_eq!(obs.len(), 10);
    for (i, o) in obs.iter().enumerate() {
        assert_eq!(o.raw_state["j"], i as f64);
        let frame = &o.frames["cam"];
        assert_eq!((frame.width, frame.height, frame.source), (16, 16, FrameSource::Live));
        assert_eq!(frame.timestamp_us, o.timestamp_us);
    }
    let states = events.iter().filter(|e| matches!(e, Event::State(_))).count();
    let frames = events.iter().filter(|e| matches!(e, Event::VideoFrame { .. })).count();
    assert_eq!((states, frames), (10, 10));
    let m = room.core("op").metrics();
    assert_eq!(m.sync.observations_emitted, 10);
    assert_eq!(m.transport.states_received, 10);
    assert_eq!(room.core("robot").metrics().transport.states_sent, 10);
}

#[test]
fn only_the_active_operators_actions_reach_the_robot() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("alice", operator_config(ActionSubscription::None));
    room.add("bob", operator_config(ActionSubscription::All));
    for id in ["robot", "alice", "bob"] {
        room.join(id);
    }
    room.run(50 * MS);
    assert_eq!(room.core("robot").operators(), vec!["alice", "bob"]);

    room.send_action("alice", 1.0).unwrap();
    room.send_action("bob", 2.0).unwrap();
    room.run(50 * MS);
    assert!(actions(&room.events("robot")).is_empty(), "no active operator: everything dropped");

    // Alice takes control by asking the robot over RPC.
    room.set_active_operator("alice", Some("alice"));
    room.run(50 * MS);
    assert_eq!(room.core("robot").active_operator().as_deref(), Some("alice"));
    assert_eq!(room.core("bob").active_operator().as_deref(), Some("alice"));
    let robot_events = room.events("robot");
    assert!(
        robot_events
            .iter()
            .any(|e| matches!(e, Event::ActiveOperatorChanged(Some(id)) if id == "alice"))
    );

    room.events("bob");
    room.send_action("alice", 3.0).unwrap();
    room.send_action("bob", 4.0).unwrap();
    room.run(50 * MS);
    assert_eq!(actions(&room.events("robot")), vec![("alice".into(), 3.0, true)]);
    // Bob subscribes to All: alice's action plus his own shadow echo.
    let mut bob = actions(&room.events("bob"));
    bob.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(bob, vec![("alice".into(), 3.0, true), ("bob".into(), 4.0, false)]);

    // Handoff to bob, by the robot this time.
    room.set_active_operator("robot", Some("bob"));
    room.run(50 * MS);
    room.events("robot");
    room.send_action("alice", 5.0).unwrap();
    room.send_action("bob", 6.0).unwrap();
    room.run(50 * MS);
    assert_eq!(actions(&room.events("robot")), vec![("bob".into(), 6.0, true)]);
    assert_eq!(room.core("alice").active_operator().as_deref(), Some("bob"));
}

#[test]
fn late_joining_operator_sees_the_active_operator() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("alice", operator_config(ActionSubscription::None));
    room.join("robot");
    room.join("alice");
    room.run(20 * MS);
    room.set_active_operator("robot", Some("alice"));
    room.run(20 * MS);

    room.add("late", operator_config(ActionSubscription::None));
    room.join("late");
    assert_eq!(room.core("late").active_operator().as_deref(), Some("alice"));
    assert!(
        room.events("late")
            .iter()
            .any(|e| matches!(e, Event::ActiveOperatorChanged(Some(id)) if id == "alice"))
    );
    assert_eq!(room.core("late").robot_identity().as_deref(), Some("robot"));
}

#[test]
fn token_seeded_active_operator_gates_from_the_start() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("alice", operator_config(ActionSubscription::None));
    let seed = HashMap::from([(ACTIVE_OPERATOR_ATTR_KEY.to_string(), "alice".to_string())]);
    room.join_with("robot", seed);
    room.join("alice");
    room.run(20 * MS);
    room.send_action("alice", 1.0).unwrap();
    room.run(20 * MS);
    assert_eq!(actions(&room.events("robot")), vec![("alice".into(), 1.0, true)]);
}

/// cam1 keeps streaming, cam2 goes silent after its first frames; each stall
/// behavior resolves the moments cam2 cannot match.
fn stalled_room(behavior: StallBehavior) -> Room {
    let mut room = Room::new(link());
    let mut robot = robot_config();
    robot.add_video("cam1", Codec::H264, VideoOptions::default());
    robot.add_video("cam2", Codec::H264, VideoOptions::default());
    room.add("robot", robot);
    let mut op = operator_config(ActionSubscription::None);
    op.add_video("cam1", Codec::H264, VideoOptions::default());
    op.add_video("cam2", Codec::H264, VideoOptions::default());
    op.set_observation_sync(true);
    op.set_fps(30);
    op.set_stall_behavior(behavior);
    op.set_max_lag_ms(100);
    room.add("op", op);
    room.join("robot");
    room.join("op");
    room.run(20 * MS);
    for i in 0..30 {
        let ts = room.send_state("robot", &[("j", i as f64)]);
        room.send_frame("robot", "cam1", ts);
        if i < 3 {
            room.send_frame("robot", "cam2", ts);
        }
        room.run(33 * MS);
    }
    room
}

#[test]
fn stall_drop_discards_moments_without_the_silent_track() {
    let mut room = stalled_room(StallBehavior::Drop);
    let events = room.events("op");
    let obs = observations(&events);
    assert_eq!(obs.len(), 3, "only the moments cam2 covered");
    let dropped: usize = events
        .iter()
        .map(|e| match e {
            Event::Drops(d) => d.len(),
            _ => 0,
        })
        .sum();
    assert!(dropped >= 20, "silent-track moments are dropped, got {dropped}");
    assert_eq!(room.core("op").metrics().sync.states_dropped as usize, dropped);
}

#[test]
fn stall_freeze_reuses_the_last_frame() {
    let mut room = stalled_room(StallBehavior::Freeze);
    let events = room.events("op");
    let obs = observations(&events);
    assert!(obs.len() >= 23, "moments keep flowing, got {}", obs.len());
    let last = obs.last().expect("observation");
    assert_eq!(last.frames["cam1"].source, FrameSource::Live);
    assert_eq!(last.frames["cam2"].source, FrameSource::Stale);
    assert!(last.frames["cam2"].timestamp_us < last.timestamp_us);
    assert!(room.core("op").metrics().sync.stale_observations_emitted > 0);
}

#[test]
fn stall_omit_substitutes_a_placeholder() {
    let mut room = stalled_room(StallBehavior::Omit);
    let events = room.events("op");
    let obs = observations(&events);
    assert!(obs.len() >= 23, "moments keep flowing, got {}", obs.len());
    let last = obs.last().expect("observation");
    let cam2 = &last.frames["cam2"];
    assert_eq!(cam2.source, FrameSource::Omitted);
    assert_eq!((cam2.width, cam2.height), (16, 16));
    assert_eq!(cam2.data, portal_core::placeholder::render(16, 16, "cam2"));
    assert!(room.core("op").metrics().sync.frames_omitted["cam2"] > 0);
}

#[test]
fn clock_sync_converges_under_skew_and_jitter() {
    let skew = 7 * S as i64;
    let mut room = Room::new(Link { latency_us: 4 * MS, jitter_us: 6 * MS, loss: 0.1 });
    room.add("robot", robot_config());
    room.add_skewed("op", operator_config(ActionSubscription::None), -skew);
    room.join("robot");
    room.join("op");
    room.run(30 * S);

    let synced = room.events("op").iter().filter(|e| matches!(e, Event::TimeSynced)).count();
    assert_eq!(synced, 1, "one sync, no resyncs");
    let m = room.core("op").metrics();
    assert!(m.time_sync.synced);
    let uncertainty = m.time_sync.uncertainty_us.expect("synced");
    assert!(m.time_sync.offset_us.abs_diff(skew) <= uncertainty, "{m:?}");
    assert!(uncertainty <= 7 * MS, "min-rtt sample is tight, got {uncertainty}");
    assert!(m.rtt.pongs_received > 25 && m.rtt.pings_sent >= m.rtt.pongs_received);

    let robot_now = room.with_core("robot", |c, now| c.now_us(now));
    let op_now = room.with_core("op", |c, now| c.now_us(now));
    assert!(robot_now.abs_diff(op_now) <= uncertainty + 1, "clocks agree within uncertainty");
}

#[test]
fn operator_leaving_is_reported_and_the_pointer_stays_pinned() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("alice", operator_config(ActionSubscription::None));
    room.join("robot");
    room.join("alice");
    room.run(20 * MS);
    assert!(
        room.events("robot")
            .iter()
            .any(|e| matches!(e, Event::OperatorJoined(id) if id == "alice"))
    );
    room.set_active_operator("robot", Some("alice"));
    room.run(20 * MS);

    room.leave("alice");
    room.run(20 * MS);
    assert!(
        room.events("robot").iter().any(|e| matches!(e, Event::OperatorLeft(id) if id == "alice"))
    );
    assert!(room.core("robot").operators().is_empty());
    assert_eq!(room.core("robot").active_operator().as_deref(), Some("alice"));

    // The same identity rejoining resumes control.
    room.join("alice");
    room.run(20 * MS);
    room.send_action("alice", 9.0).unwrap();
    room.run(20 * MS);
    assert_eq!(actions(&room.events("robot")), vec![("alice".into(), 9.0, true)]);
}

#[test]
fn protocol_version_mismatch_keeps_peers_apart() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("old", operator_config(ActionSubscription::None));
    room.peers
        .get_mut("old")
        .unwrap()
        .attributes
        .insert(PROTOCOL_VERSION_ATTR_KEY.into(), "2".into());
    let seed = HashMap::from([(ACTIVE_OPERATOR_ATTR_KEY.to_string(), "old".to_string())]);
    room.join_with("robot", seed);
    room.join("old");
    room.run(20 * MS);

    assert!(room.core("robot").operators().is_empty());
    assert!(room.events("robot").is_empty());
    assert_eq!(room.core("old").robot_identity().as_deref(), Some("robot"));
    room.send_action("old", 1.0).unwrap();
    room.run(20 * MS);
    assert!(actions(&room.events("robot")).is_empty(), "even an active pointer can't admit it");
}

#[test]
fn reconnect_rebuilds_rosters_but_keeps_the_pointer() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("alice", operator_config(ActionSubscription::None));
    room.join("robot");
    room.join("alice");
    room.run(20 * MS);
    room.set_active_operator("robot", Some("alice"));
    room.run(20 * MS);
    room.feed("robot", Input::Reconnected);
    assert!(room.core("robot").operators().is_empty());
    assert_eq!(room.core("robot").active_operator().as_deref(), Some("alice"));
}

fn rpc_room() -> Room {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("teleop", operator_config(ActionSubscription::None));
    let mut observer = PortalConfig::new("s", Role::Observer);
    observer.add_state_typed([("j", DType::F64)]);
    room.add("recorder", observer);
    for id in ["robot", "teleop", "recorder"] {
        room.join(id);
    }
    room.run(20 * MS);
    room.events("robot");
    room
}

/// Feed the robot a `set_active_operator` RPC and collect the replies.
fn call(room: &mut Room, caller: &str, rpc: u64) -> Vec<(RpcId, Result<String, RpcError>)> {
    let now_us = room.local_time("robot");
    let out = room.peers.get_mut("robot").unwrap().core.handle(Step {
        input: Input::RpcRequest(RpcRequest {
            id: RpcId(rpc),
            method: SET_ACTIVE_OPERATOR_RPC,
            caller,
            payload: caller,
        }),
        now_us,
    });
    replies(room, out)
}

fn replies(
    room: &mut Room,
    out: Vec<Output<VideoFrameData>>,
) -> Vec<(RpcId, Result<String, RpcError>)> {
    let mut replies = Vec::new();
    let mut rest = Vec::new();
    for o in out {
        match o {
            Output::RpcReply { id, result } => replies.push((id, result)),
            Output::SetAttributes { id: Some(id), .. } => {
                let now_us = room.local_time("robot");
                let more =
                    room.peers.get_mut("robot").unwrap().core.handle(Step {
                        input: Input::AttributesSet { id, result: Ok(()) },
                        now_us,
                    });
                replies.extend(self::replies(room, more));
            }
            other => rest.push(other),
        }
    }
    room.execute("robot", rest);
    replies
}

fn wake_robot(room: &mut Room, after: u64) -> Vec<(RpcId, Result<String, RpcError>)> {
    room.now += after;
    let now_us = room.local_time("robot");
    let out = room.peers.get_mut("robot").unwrap().core.handle(Step { input: Input::Wake, now_us });
    replies(room, out)
}

#[test]
fn only_operators_and_observers_may_steer() {
    let mut room = rpc_room();
    assert!(matches!(&call(&mut room, "teleop", 0)[..], [(RpcId(0), Ok(_))]));
    assert!(matches!(&call(&mut room, "recorder", 1)[..], [(RpcId(1), Ok(_))]));
    assert!(call(&mut room, "stranger", 2).is_empty(), "a stranger waits for its role");
    let refused = wake_robot(&mut room, RPC_CALLER_ROSTER_WAIT_US);
    assert!(matches!(&refused[..], [(RpcId(2), Err(e))] if e.code == RPC_NOT_AUTHORIZED));
    assert_eq!(room.core("robot").active_operator().as_deref(), Some("recorder"));
}

#[test]
fn waiting_strangers_do_not_hold_up_other_callers() {
    let mut room = rpc_room();
    for i in 0..50 {
        assert!(call(&mut room, &format!("s{i}"), i).is_empty());
    }
    assert!(call(&mut room, "late", 100).is_empty());
    let deadline = room.local_time("robot") + RPC_CALLER_ROSTER_WAIT_US;
    assert!(room.core("robot").next_wake_us().is_some_and(|at| at <= deadline));

    // A known caller is answered at once, without waiting on the strangers.
    assert!(matches!(&call(&mut room, "teleop", 200)[..], [(RpcId(200), Ok(_))]));

    // A caller whose role shows up during the wait is let in.
    room.now += 500 * MS;
    let mut late = operator_config(ActionSubscription::None);
    late.add_state_typed([("j", DType::F64)]);
    room.add("late", late);
    let attributes = role_attributes(Role::Operator);
    let now_us = room.local_time("robot");
    let out =
        room.peers.get_mut("robot").unwrap().core.handle(Step {
            input: Input::AttributesChanged { identity: "late", attributes },
            now_us,
        });
    let admitted = replies(&mut room, out);
    assert!(matches!(&admitted[..], [(RpcId(100), Ok(_))]));

    // Refused together after one wait, not one wait each.
    let refused = wake_robot(&mut room, RPC_CALLER_ROSTER_WAIT_US - 500 * MS);
    assert_eq!(refused.len(), 50);
    assert!(refused.iter().all(|(_, r)| matches!(r, Err(e) if e.code == RPC_NOT_AUTHORIZED)));
}

#[test]
fn keypoints_reach_everyone_and_bad_ones_warn_once() {
    let mut room = rpc_room();
    let out = room
        .with_core("teleop", |c, now_us| {
            c.send_keypoint(SendKeypoint {
                kind: "recording",
                payload: serde_json::Map::new(),
                timestamp_us: Some(42),
                now_us,
            })
        })
        .unwrap();
    room.execute("teleop", out);
    room.run(20 * MS);
    for id in ["robot", "recorder", "teleop"] {
        let kps: Vec<_> = room
            .events(id)
            .into_iter()
            .filter_map(|e| match e {
                Event::Keypoint(k) => Some((k.kind, k.sender, k.timestamp_us)),
                _ => None,
            })
            .collect();
        assert_eq!(kps, vec![("recording".into(), "teleop".into(), 42)], "{id}");
    }

    for _ in 0..2 {
        room.feed(
            "robot",
            Input::DataReceived(DataReceived {
                topic: "portal_keypoint",
                payload: b"garbage",
                sender: Some("teleop"),
            }),
        );
    }
    room.feed(
        "robot",
        Input::DataReceived(DataReceived {
            topic: "portal_keypoint",
            payload: b"{}",
            sender: Some("stranger"),
        }),
    );
    assert!(room.events("robot").is_empty());
}

#[test]
fn sends_check_role_and_connection() {
    let mut room = Room::new(link());
    room.add("robot", robot_config());
    room.add("op", operator_config(ActionSubscription::All));
    let values = HashMap::new();
    let err = room
        .with_core("op", |c, now_us| {
            c.send_state(SendState { values: &values, timestamp_us: None, now_us })
        })
        .unwrap_err();
    assert!(matches!(err, PortalError::WrongRole(Role::Operator)));
    assert!(
        matches!(room.send_action("op", 1.0), Err(PortalError::WrongRole(_))),
        "not connected yet"
    );
    let err = room
        .with_core("robot", |c, _| c.set_active_operator(SetActiveOperator { identity: None }))
        .unwrap_err();
    assert!(matches!(err, PortalError::NotConnected));
    let err = room
        .with_core("op", |c, _| c.set_active_operator(SetActiveOperator { identity: None }))
        .unwrap_err();
    assert!(matches!(err, PortalError::NoPeer));
    room.join("robot");
    room.join("op");
    room.send_action("op", 1.0).unwrap();
    let bad = HashMap::from([("v".to_string(), TypedValue::Bool(true))]);
    let err = room
        .with_core("op", |c, now_us| {
            c.send_action(SendAction {
                values: &bad,
                timestamp_us: None,
                in_reply_to_ts_us: None,
                now_us,
            })
        })
        .unwrap_err();
    assert!(matches!(err, PortalError::DtypeMismatch { .. }));
}
