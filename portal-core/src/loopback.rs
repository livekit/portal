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

//! An in-process transport on a fake clock, for running whole sessions of
//! `PortalCore`s in plain `cargo test`.
//!
//! By default it is deliberately shaped like Media over QUIC, so nothing
//! LiveKit-specific can leak into the core unnoticed: broadcast pub/sub
//! only, publisher-scoped streams, no native RPC or presence (the core's
//! polyfills run), and video as byte frames only. It honors `Delivery`
//! the way MoQ would: `LatestOnly` is a group per value, dropped only when
//! a newer one supersedes it, and a late subscriber starts from the latest;
//! `BestEffort` is lost at the link's loss rate and dropped once stale.
//! Each link has its own latency, jitter, loss and reordering, and each
//! peer its own clock skew.
//!
//! With native caps it instead emulates a room with built-in presence and
//! RPC, which exercises the paths the LiveKit driver uses.

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;

use crate::config::PortalConfig;
use crate::control::{decode_presence, encode_presence};
use crate::core::{
    Connect, CoreInit, Event, Input, OpId, Output, PortalCore, RPC_RECIPIENT_DISCONNECTED, RpcId,
    RpcRequest, SET_ACTIVE_OPERATOR_RPC, SendVideoFrame, Step, VideoTransport,
};
use crate::error::{PortalError, PortalResult};
use crate::frame_video::{RgbFrame, build_frame_payload};
use crate::rpc::RpcError;
use crate::transport::{Delivery, PeerId, Presence, Stream, TransportCaps, VideoCaps};
use crate::types::{FrameSource, VideoFrameData};

/// Where the fake clock starts, far enough from zero that skewed peer
/// clocks stay positive.
const EPOCH_US: u64 = 1_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkConfig {
    pub latency_us: u64,
    /// Extra delay, uniform in `0..=jitter_us`.
    pub jitter_us: u64,
    /// Chance in `0.0..=1.0` that a `BestEffort` message is lost.
    pub loss: f64,
    /// Whether non-reliable messages may overtake each other. Reliable ones
    /// never do.
    pub reorder: bool,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self { latency_us: 5_000, jitter_us: 0, loss: 0.0, reorder: true }
    }
}

/// The MoQ-shaped default.
pub fn pubsub_caps() -> TransportCaps {
    TransportCaps {
        video: VideoCaps::ByteFramesOnly,
        native_rpc: false,
        native_presence: false,
        e2ee: false,
        max_payload_bytes: 64 * 1024,
    }
}

/// A room with built-in presence and RPC that encodes video itself.
pub fn room_caps() -> TransportCaps {
    TransportCaps {
        video: VideoCaps::TransportEncoded,
        native_rpc: true,
        native_presence: true,
        ..pubsub_caps()
    }
}

pub struct LoopbackOptions {
    pub caps: TransportCaps,
    /// Every link's config unless `set_link` overrides it.
    pub link: LinkConfig,
    pub seed: u64,
}

impl Default for LoopbackOptions {
    fn default() -> Self {
        Self { caps: pubsub_caps(), link: LinkConfig::default(), seed: 0x9E37_79B9_7F4A_7C15 }
    }
}

pub struct PeerSpec {
    pub id: PeerId,
    pub config: PortalConfig,
    /// Added to the peer's local clock.
    pub skew_us: i64,
    /// A pointer seeded before connect, like a LiveKit token attribute.
    pub active_operator: Option<PeerId>,
    /// Announce this protocol version instead of the real one.
    pub protocol_version: Option<String>,
}

impl PeerSpec {
    pub fn new(id: &str, config: PortalConfig) -> Self {
        Self {
            id: id.to_string(),
            config,
            skew_us: 0,
            active_operator: None,
            protocol_version: None,
        }
    }
}

/// Answers application RPC methods: `(method, payload)` to the result.
pub type RpcHandler = Box<dyn FnMut(&str, &str) -> Result<String, RpcError>>;

pub type Core = PortalCore<VideoFrameData>;

struct Peer {
    core: Core,
    skew_us: i64,
    protocol_version: Option<String>,
    /// Local time of the requested wake-up.
    wake_at: Option<u64>,
    events: Vec<Event<VideoFrameData>>,
    rpc_handler: Option<RpcHandler>,
}

/// One publisher's stream (or video track) as one subscriber sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Lane {
    Data(Stream),
    Video(String),
    Room,
}

#[derive(Debug, Clone)]
enum Message {
    Data { stream: Stream, payload: Vec<u8> },
    Frame(Bytes),
    VideoFrame { track: String, frame: VideoFrameData },
    Joined,
    Presence(Presence),
    Left,
    Call { key: u64, method: String, payload: String },
    Result { id: OpId, result: Result<String, RpcError> },
}

struct InFlight {
    at_us: u64,
    sent_us: u64,
    /// Tie-break: first sent, first delivered.
    order: u64,
    from: PeerId,
    to: PeerId,
    lane: Lane,
    /// Per publisher lane, for `LatestOnly`.
    seq: u64,
    delivery: Delivery,
    message: Message,
}

pub struct Loopback {
    now_us: u64,
    caps: TransportCaps,
    default_link: LinkConfig,
    links: HashMap<(PeerId, PeerId), LinkConfig>,
    rng: u64,
    peers: BTreeMap<PeerId, Peer>,
    in_flight: Vec<InFlight>,
    next_order: u64,
    /// Per publisher lane, the last sequence number used.
    lane_seq: HashMap<(PeerId, Lane), u64>,
    /// Per (publisher, subscriber, lane): last delivery time (ordering)
    /// and last sequence delivered (`LatestOnly`).
    lane_tail_us: HashMap<(PeerId, PeerId, Lane), u64>,
    lane_delivered: HashMap<(PeerId, PeerId, Lane), u64>,
    /// `LatestOnly` values, per publisher lane, for late subscribers.
    retained: HashMap<(PeerId, Lane), (u64, Message)>,
    /// Native presence: what each peer announces.
    presence: HashMap<PeerId, Presence>,
    /// Native RPC in flight: key to (caller, its op id).
    calls: HashMap<u64, (PeerId, OpId)>,
    next_call: u64,
}

impl Loopback {
    pub fn new(options: LoopbackOptions) -> Self {
        let LoopbackOptions { caps, link, seed } = options;
        Self {
            now_us: EPOCH_US,
            caps,
            default_link: link,
            links: HashMap::new(),
            rng: seed.max(1),
            peers: BTreeMap::new(),
            in_flight: Vec::new(),
            next_order: 0,
            lane_seq: HashMap::new(),
            lane_tail_us: HashMap::new(),
            lane_delivered: HashMap::new(),
            retained: HashMap::new(),
            presence: HashMap::new(),
            calls: HashMap::new(),
            next_call: 0,
        }
    }

    /// Override the link from `from` to `to`.
    pub fn set_link(&mut self, from: &str, to: &str, link: LinkConfig) {
        self.links.insert((from.to_string(), to.to_string()), link);
    }

    /// Create a peer and connect it.
    pub fn join(&mut self, spec: PeerSpec) -> PortalResult<()> {
        let PeerSpec { id, config, skew_us, active_operator, protocol_version } = spec;
        if self.peers.contains_key(&id) {
            return Err(PortalError::AlreadyConnected);
        }
        let mut core = PortalCore::new(CoreInit { config, caps: self.caps });
        let peers = match self.caps.native_presence {
            true => self.presence.iter().map(|(p, pr)| (p.clone(), pr.clone())).collect(),
            false => Vec::new(),
        };
        let now_us = local_time(self.now_us, skew_us);
        let out =
            core.connect(Connect { local_peer: id.clone(), active_operator, peers, now_us })?;
        let peer = Peer {
            core,
            skew_us,
            protocol_version,
            wake_at: None,
            events: Vec::new(),
            rpc_handler: None,
        };
        self.peers.insert(id.clone(), peer);
        if self.caps.native_presence {
            let presence = self.announced(&id, self.peers[&id].core.local_presence());
            self.presence.insert(id.clone(), presence.clone());
            for other in self.others(&id) {
                self.send(&id, &other, Lane::Room, Delivery::Reliable, Message::Joined);
                let message = Message::Presence(presence.clone());
                self.send(&id, &other, Lane::Room, Delivery::Reliable, message);
            }
        }
        // A late subscriber starts from every publisher's latest values.
        let retained: Vec<_> = self
            .retained
            .iter()
            .filter(|((from, _), _)| *from != id)
            .map(|((from, lane), (seq, m))| (from.clone(), lane.clone(), *seq, m.clone()))
            .collect();
        for (from, lane, seq, message) in retained {
            self.enqueue(Send {
                from,
                to: id.clone(),
                lane,
                seq,
                delivery: Delivery::LatestOnly,
                message,
            });
        }
        self.execute(&id, out);
        Ok(())
    }

    /// Disconnect a peer. A graceful leave announces it; otherwise the peer
    /// just goes silent, as on a crash or a cut link.
    pub fn leave(&mut self, id: &str, graceful: bool) {
        let Some(mut peer) = self.peers.remove(id) else {
            return;
        };
        peer.core.disconnect();
        // Calls that can no longer be answered fail, as on a real room.
        let unanswerable: Vec<(PeerId, u64)> = self
            .in_flight
            .iter()
            .filter(|m| m.to == id)
            .filter_map(|m| match m.message {
                Message::Call { key, .. } => Some((m.from.clone(), key)),
                _ => None,
            })
            .collect();
        self.in_flight.retain(|m| m.to != id);
        for (caller, key) in unanswerable {
            let Some((_, op)) = self.calls.remove(&key) else {
                continue;
            };
            let result =
                Err(RpcError::new(RPC_RECIPIENT_DISCONNECTED, "Recipient disconnected", None));
            let message = Message::Result { id: op, result };
            self.send(id, &caller, Lane::Room, Delivery::Reliable, message);
        }
        self.retained.retain(|(from, _), _| from != id);
        self.presence.remove(id);
        if graceful && self.caps.native_presence {
            for other in self.others(id) {
                self.send(id, &other, Lane::Room, Delivery::Reliable, Message::Left);
            }
        }
    }

    /// Advance the fake clock, delivering messages and firing wake-ups in
    /// time order.
    pub fn run_for(&mut self, duration_us: u64) {
        let until = self.now_us + duration_us;
        loop {
            let next_message = self.in_flight.iter().map(|m| (m.at_us, m.order)).min();
            let next_wake = self
                .peers
                .iter()
                .filter_map(|(id, p)| p.wake_at.map(|at| (net_time(at, p.skew_us), id.clone())))
                .min();
            match (next_message, next_wake) {
                (Some((at, _)), wake)
                    if at <= until && wake.as_ref().is_none_or(|(w, _)| at <= *w) =>
                {
                    self.now_us = self.now_us.max(at);
                    self.deliver_next();
                }
                (_, Some((at, id))) if at <= until => {
                    self.now_us = self.now_us.max(at);
                    if let Some(p) = self.peers.get_mut(&id) {
                        p.wake_at = None;
                    }
                    self.step(&id, Input::Wake);
                }
                _ => break,
            }
        }
        self.now_us = until;
    }

    /// Run a closure against a peer's core with its local time, and carry
    /// out the outputs it returns.
    pub fn with_core<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Core, u64) -> PortalResult<(T, Vec<Output<VideoFrameData>>)>,
    ) -> PortalResult<T> {
        let now_us = self.local_now(id);
        let peer = self.peers.get_mut(id).ok_or(PortalError::NotConnected)?;
        let (value, out) = f(&mut peer.core, now_us)?;
        self.execute(id, out);
        Ok(value)
    }

    /// Feed a peer an input directly and carry out what it returns.
    pub fn feed(&mut self, id: &str, input: Input<'_, VideoFrameData>) {
        self.step(id, input);
    }

    /// Feed a peer an input and hand back its outputs instead of carrying
    /// them out, to inspect them; `execute` carries out the rest.
    pub fn handle(
        &mut self,
        id: &str,
        input: Input<'_, VideoFrameData>,
    ) -> Vec<Output<VideoFrameData>> {
        let now_us = self.local_now(id);
        match self.peers.get_mut(id) {
            Some(peer) => peer.core.handle(Step { input, now_us }),
            None => Vec::new(),
        }
    }

    /// Move the fake clock without delivering anything or waking anyone.
    pub fn advance_clock(&mut self, us: u64) {
        self.now_us += us;
    }

    /// Send a packed RGB24 frame from a robot, the way a driver would:
    /// route it through the core, then encode it as the route says.
    pub fn send_video_frame(&mut self, input: VideoSend<'_>) -> PortalResult<u64> {
        let VideoSend { from, track, rgb, width, height, timestamp_us } = input;
        let now_us = self.local_now(from);
        let peer = self.peers.get_mut(from).ok_or(PortalError::NotConnected)?;
        let route = peer.core.send_video_frame(SendVideoFrame { track, timestamp_us, now_us })?;
        let ts = route.timestamp_us;
        let message = match route.transport {
            VideoTransport::ByteFrames { codec, quality } => {
                let frame = RgbFrame {
                    track_name: track,
                    codec,
                    width,
                    height,
                    timestamp_us: ts,
                    rgb,
                    quality,
                };
                Message::Frame(Bytes::from(build_frame_payload(frame)?))
            }
            VideoTransport::TransportEncoded => {
                let frame = VideoFrameData {
                    width,
                    height,
                    data: Bytes::copy_from_slice(rgb),
                    timestamp_us: ts,
                    source: FrameSource::Live,
                };
                Message::VideoFrame { track: track.to_string(), frame }
            }
            VideoTransport::DriverEncoded { codec } => {
                return Err(PortalError::Unsupported(format!(
                    "the loopback has no {codec:?} encoder"
                )));
            }
        };
        let lane = Lane::Video(track.to_string());
        for other in self.others(from) {
            self.send(from, &other, lane.clone(), Delivery::Reliable, message.clone());
        }
        Ok(ts)
    }

    /// Answer application RPCs to `id` with `handler`.
    pub fn on_rpc(&mut self, id: &str, handler: RpcHandler) {
        if let Some(peer) = self.peers.get_mut(id) {
            peer.rpc_handler = Some(handler);
        }
    }

    /// Take the events a peer has emitted so far.
    pub fn events(&mut self, id: &str) -> Vec<Event<VideoFrameData>> {
        self.peers.get_mut(id).map(|p| std::mem::take(&mut p.events)).unwrap_or_default()
    }

    pub fn core(&self, id: &str) -> &Core {
        &self.peers[id].core
    }

    /// The fake clock as `id` reads it.
    pub fn local_now(&self, id: &str) -> u64 {
        let skew = self.peers.get(id).map_or(0, |p| p.skew_us);
        local_time(self.now_us, skew)
    }

    /// The fake clock, unskewed.
    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    /// Messages still in flight.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    // --- Internal ---

    fn others(&self, id: &str) -> Vec<PeerId> {
        self.peers.keys().filter(|k| *k != id).cloned().collect()
    }

    fn step(&mut self, id: &str, input: Input<'_, VideoFrameData>) {
        let now_us = self.local_now(id);
        let Some(peer) = self.peers.get_mut(id) else {
            return;
        };
        let out = peer.core.handle(Step { input, now_us });
        self.execute(id, out);
    }

    /// Carry out a peer's outputs.
    pub fn execute(&mut self, from: &str, out: Vec<Output<VideoFrameData>>) {
        for output in out {
            match output {
                Output::Publish { stream, payload, delivery, to: _ } => {
                    // Broadcast only: `to` is a hint this transport ignores.
                    let payload = match stream {
                        Stream::Presence => self.rewrite_presence(from, payload),
                        _ => payload,
                    };
                    let lane = Lane::Data(stream);
                    let message = Message::Data { stream, payload };
                    let seq = self.next_seq(from, &lane);
                    if delivery == Delivery::LatestOnly {
                        self.retained
                            .insert((from.to_string(), lane.clone()), (seq, message.clone()));
                    }
                    for other in self.others(from) {
                        let send = Send {
                            from: from.to_string(),
                            to: other,
                            lane: lane.clone(),
                            seq,
                            delivery,
                            message: message.clone(),
                        };
                        self.enqueue(send);
                    }
                }
                Output::SetPresence { id, presence } => {
                    let presence = self.announced(from, presence);
                    self.presence.insert(from.to_string(), presence.clone());
                    for other in self.others(from) {
                        let message = Message::Presence(presence.clone());
                        self.send(from, &other, Lane::Room, Delivery::Reliable, message);
                    }
                    if let Some(id) = id {
                        self.step(from, Input::PresenceSet { id, result: Ok(()) });
                    }
                }
                Output::Rpc { id, to, method, payload, timeout_ms: _ } => {
                    if !self.peers.contains_key(&to) {
                        let result = Err(RpcError::new(
                            RPC_RECIPIENT_DISCONNECTED,
                            "Recipient disconnected",
                            None,
                        ));
                        self.step(from, Input::RpcResponse { id, result });
                        continue;
                    }
                    let key = self.next_call;
                    self.next_call += 1;
                    self.calls.insert(key, (from.to_string(), id));
                    let message = Message::Call { key, method, payload };
                    self.send(from, &to, Lane::Room, Delivery::Reliable, message);
                }
                Output::RpcReply { id, result } => {
                    let Some((caller, op)) = self.calls.remove(&id.0) else {
                        continue;
                    };
                    let message = Message::Result { id: op, result };
                    self.send(from, &caller, Lane::Room, Delivery::Reliable, message);
                }
                Output::Emit(event) => self.emit(from, event),
                Output::ScheduleWake { at_us } => {
                    if let Some(p) = self.peers.get_mut(from) {
                        p.wake_at = at_us;
                    }
                }
            }
        }
    }

    fn emit(&mut self, id: &str, event: Event<VideoFrameData>) {
        let Event::RpcRequest { id: rpc, method, payload, .. } = &event else {
            if let Some(p) = self.peers.get_mut(id) {
                p.events.push(event);
            }
            return;
        };
        let Some(peer) = self.peers.get_mut(id) else {
            return;
        };
        let result = match &mut peer.rpc_handler {
            Some(handler) => handler(method, payload),
            None => Err(RpcError::new(1400, "Method not supported at destination", None)),
        };
        let out = peer.core.reply_rpc(crate::core::ReplyRpc { id: *rpc, result });
        peer.events.push(event);
        self.execute(id, out);
    }

    /// The presence `id` announces, with any version override applied.
    fn announced(&self, id: &str, mut presence: Presence) -> Presence {
        if let Some(version) = self.peers.get(id).and_then(|p| p.protocol_version.clone()) {
            presence.protocol_version = Some(version);
        }
        presence
    }

    fn rewrite_presence(&self, id: &str, payload: Vec<u8>) -> Vec<u8> {
        match decode_presence(&payload) {
            Some(p) if self.peers.get(id).is_some_and(|p| p.protocol_version.is_some()) => {
                encode_presence(&self.announced(id, p))
            }
            _ => payload,
        }
    }

    fn next_seq(&mut self, from: &str, lane: &Lane) -> u64 {
        let seq = self.lane_seq.entry((from.to_string(), lane.clone())).or_default();
        *seq += 1;
        *seq
    }

    fn send(&mut self, from: &str, to: &str, lane: Lane, delivery: Delivery, message: Message) {
        let seq = self.next_seq(from, &lane);
        let (from, to) = (from.to_string(), to.to_string());
        self.enqueue(Send { from, to, lane, seq, delivery, message });
    }

    fn enqueue(&mut self, send: Send) {
        let Send { from, to, lane, seq, delivery, message } = send;
        let link =
            self.links.get(&(from.clone(), to.clone())).copied().unwrap_or(self.default_link);
        let reliable = delivery == Delivery::Reliable;
        let lossy = matches!(delivery, Delivery::BestEffort { .. });
        if lossy && link.loss > 0.0 && self.random_unit() < link.loss {
            return;
        }
        let jitter = match link.jitter_us {
            0 => 0,
            j => self.random() % (j + 1),
        };
        let mut at_us = self.now_us + link.latency_us + jitter;
        let key = (from.clone(), to.clone(), lane.clone());
        if reliable || !link.reorder {
            let tail = self.lane_tail_us.get(&key).copied().unwrap_or(0);
            at_us = at_us.max(tail);
            self.lane_tail_us.insert(key, at_us);
        }
        let order = self.next_order;
        self.next_order += 1;
        let sent_us = self.now_us;
        self.in_flight.push(InFlight {
            at_us,
            sent_us,
            order,
            from,
            to,
            lane,
            seq,
            delivery,
            message,
        });
    }

    fn deliver_next(&mut self) {
        let Some(i) = self
            .in_flight
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| (m.at_us, m.order))
            .map(|(i, _)| i)
        else {
            return;
        };
        let m = self.in_flight.swap_remove(i);
        if !self.peers.contains_key(&m.to) {
            return;
        }
        let key = (m.from.clone(), m.to.clone(), m.lane.clone());
        match m.delivery {
            Delivery::Reliable => {}
            Delivery::LatestOnly => {
                let last = self.lane_delivered.get(&key).copied().unwrap_or(0);
                if m.seq <= last {
                    return;
                }
                self.lane_delivered.insert(key, m.seq);
            }
            Delivery::BestEffort { max_age_ms } => {
                if m.at_us - m.sent_us > max_age_ms as u64 * 1_000 {
                    return;
                }
            }
        }
        let (from, to) = (m.from.as_str(), m.to.as_str());
        match m.message {
            Message::Data { stream, payload } => {
                self.step(to, Input::Data { stream, from: Some(from), payload: &payload });
            }
            Message::Frame(payload) => self.step(to, Input::FrameBytes { from, payload }),
            Message::VideoFrame { track, frame } => {
                self.step(to, Input::VideoFrame { track: &track, frame })
            }
            Message::Joined => self.step(to, Input::PeerJoined { peer: from }),
            Message::Presence(presence) => {
                self.step(to, Input::PresenceChanged { peer: from, presence })
            }
            Message::Left => self.step(to, Input::PeerLeft { peer: from }),
            Message::Call { key, method, payload } => {
                self.on_native_call(NativeCall { key, from, to, method, payload })
            }
            Message::Result { id, result } => self.step(to, Input::RpcResponse { id, result }),
        }
    }

    /// Native RPC reaching its callee: Portal's own method goes to the core,
    /// application methods to the peer's handler, as on LiveKit.
    fn on_native_call(&mut self, call: NativeCall<'_>) {
        let NativeCall { key, from, to, method, payload } = call;
        if method == SET_ACTIVE_OPERATOR_RPC {
            let request = RpcRequest { id: RpcId(key), method: &method, from, payload: &payload };
            self.step(to, Input::RpcRequest(request));
            return;
        }
        let Some(peer) = self.peers.get_mut(to) else {
            return;
        };
        let result = match &mut peer.rpc_handler {
            Some(handler) => handler(&method, &payload),
            None => Err(RpcError::new(1400, "Method not supported at destination", None)),
        };
        self.execute(to, vec![Output::RpcReply { id: RpcId(key), result }]);
    }

    fn random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn random_unit(&mut self) -> f64 {
        (self.random() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub struct VideoSend<'a> {
    pub from: &'a str,
    pub track: &'a str,
    pub rgb: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub timestamp_us: Option<u64>,
}

struct Send {
    from: PeerId,
    to: PeerId,
    lane: Lane,
    seq: u64,
    delivery: Delivery,
    message: Message,
}

struct NativeCall<'a> {
    key: u64,
    from: &'a str,
    to: &'a str,
    method: String,
    payload: String,
}

fn local_time(net_us: u64, skew_us: i64) -> u64 {
    net_us.saturating_add_signed(skew_us)
}

fn net_time(local_us: u64, skew_us: i64) -> u64 {
    local_us.saturating_add_signed(-skew_us)
}
