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

//! Robot/operator scenarios over the loopback transport: whole sessions of
//! cores on a fake clock, no server. Most run on the MoQ-shaped default
//! (polyfilled RPC and presence, byte-frame video); the ones about paths
//! only a native transport takes run on the room-shaped caps.

use std::collections::HashMap;

use livekit_portal_core::loopback::{
    LinkConfig, Loopback, LoopbackOptions, PeerSpec, VideoSend, pubsub_caps, room_caps,
};
use livekit_portal_core::{
    ActionSubscription, Codec, DType, Event, FrameSource, Input, Output, PRESENCE_TIMEOUT_US,
    PerformRpc, PortalConfig, PortalError, RPC_CALLER_ROSTER_WAIT_US, RPC_NOT_AUTHORIZED, Role,
    RpcError, RpcId, RpcRequest, SET_ACTIVE_OPERATOR_RPC, SendAction, SendKeypoint, SendState,
    SetActiveOperator, StallBehavior, Stream, TransportCaps, TypedValue, VideoFrameData,
    VideoOptions,
};
use livekit_portal_core::{Connect, CoreInit, PortalCore};

const MS: u64 = 1_000;
const S: u64 = 1_000_000;
const SIDE: u32 = 16;

fn session(caps: TransportCaps) -> Loopback {
    session_with(caps, LinkConfig { latency_us: 5 * MS, ..Default::default() })
}

fn session_with(caps: TransportCaps, link: LinkConfig) -> Loopback {
    Loopback::new(LoopbackOptions { caps, link, ..Default::default() })
}

fn join(lb: &mut Loopback, id: &str, config: PortalConfig) {
    lb.join(PeerSpec::new(id, config)).expect("join");
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

/// Send a state stamped on the robot's clock; returns the stamp.
fn send_state(lb: &mut Loopback, id: &str, value: f64) -> u64 {
    let values = HashMap::from([("j".to_string(), TypedValue::F64(value))]);
    lb.with_core(id, |c, now_us| {
        let ts = c.now_us(now_us);
        let out = c.send_state(SendState { values: &values, timestamp_us: Some(ts), now_us })?;
        Ok((ts, out))
    })
    .expect("send_state")
}

fn send_action(lb: &mut Loopback, id: &str, value: f64) -> Result<(), PortalError> {
    let values = HashMap::from([("v".to_string(), TypedValue::F64(value))]);
    lb.with_core(id, |c, now_us| {
        let send =
            SendAction { values: &values, timestamp_us: None, in_reply_to_ts_us: None, now_us };
        Ok(((), c.send_action(send)?))
    })
}

fn send_frame(lb: &mut Loopback, id: &str, track: &str, timestamp_us: u64) {
    let rgb: Vec<u8> = (0..SIDE * SIDE * 3).map(|i| (i % 251) as u8).collect();
    let send = VideoSend {
        from: id,
        track,
        rgb: &rgb,
        width: SIDE,
        height: SIDE,
        timestamp_us: Some(timestamp_us),
    };
    lb.send_video_frame(send).expect("send_video_frame");
}

fn set_active_operator(lb: &mut Loopback, id: &str, identity: Option<&str>) {
    lb.with_core(id, |c, now_us| {
        let input = SetActiveOperator { identity: identity.map(String::from), now_us };
        c.set_active_operator(input)
    })
    .expect("set_active_operator");
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

fn observations(events: &[Event<VideoFrameData>]) -> Vec<&livekit_portal_core::Observation> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Observation(o) => Some(o),
            _ => None,
        })
        .collect()
}

fn has(events: &[Event<VideoFrameData>], f: impl Fn(&Event<VideoFrameData>) -> bool) -> bool {
    events.iter().any(f)
}

// --- Data and video ---

#[test]
fn state_and_mjpeg_track_become_observations() {
    let mut lb = session(pubsub_caps());
    let mut robot = robot_config();
    robot.add_video("cam", Codec::Mjpeg, VideoOptions::default());
    join(&mut lb, "robot", robot);
    let mut op = operator_config(ActionSubscription::None);
    op.add_video("cam", Codec::Mjpeg, VideoOptions::default());
    op.set_observation_sync(true);
    join(&mut lb, "op", op);
    lb.run_for(100 * MS);

    for i in 0..10 {
        let ts = send_state(&mut lb, "robot", i as f64);
        send_frame(&mut lb, "robot", "cam", ts);
        lb.run_for(33 * MS);
    }

    let events = lb.events("op");
    let obs = observations(&events);
    assert_eq!(obs.len(), 10);
    for (i, o) in obs.iter().enumerate() {
        assert_eq!(o.raw_state["j"], i as f64);
        let frame = &o.frames["cam"];
        assert_eq!((frame.width, frame.height, frame.source), (SIDE, SIDE, FrameSource::Live));
        assert_eq!(frame.timestamp_us, o.timestamp_us);
    }
    let states = events.iter().filter(|e| matches!(e, Event::State(_))).count();
    let frames = events.iter().filter(|e| matches!(e, Event::VideoFrame { .. })).count();
    assert_eq!((states, frames), (10, 10));
    let m = lb.core("op").metrics();
    assert_eq!(m.sync.observations_emitted, 10);
    assert_eq!(m.transport.states_received, 10);
    assert_eq!(m.transport.frames_received["cam"], 10);
    assert!(m.transport.bytes_received["cam"] > 0);
    assert_eq!(lb.core("robot").metrics().transport.states_sent, 10);
}

#[test]
fn lossy_state_never_goes_backwards() {
    let link = LinkConfig { latency_us: 5 * MS, jitter_us: 20 * MS, loss: 0.0, reorder: true };
    let mut lb = session_with(pubsub_caps(), link);
    let mut robot = robot_config();
    robot.set_state_reliable(false);
    join(&mut lb, "robot", robot);
    join(&mut lb, "op", operator_config(ActionSubscription::None));
    lb.run_for(50 * MS);
    for i in 0..200 {
        send_state(&mut lb, "robot", i as f64);
        lb.run_for(2 * MS);
    }
    lb.run_for(100 * MS);
    let received: Vec<f64> = lb
        .events("op")
        .iter()
        .filter_map(|e| match e {
            Event::State(s) => Some(s.raw_values["j"]),
            _ => None,
        })
        .collect();
    assert!(received.len() > 20, "most states arrive, got {}", received.len());
    assert!(received.len() < 200, "jitter reorders some, and the superseded are dropped");
    assert!(received.windows(2).all(|w| w[0] < w[1]), "latest-only never delivers an older value");
}

#[test]
fn stale_lossy_actions_are_dropped() {
    let link = LinkConfig { latency_us: 300 * MS, ..Default::default() };
    let mut lb = session_with(pubsub_caps(), link);
    join(&mut lb, "robot", robot_config());
    let mut op = operator_config(ActionSubscription::None);
    op.set_action_reliable(false);
    join(&mut lb, "alice", op);
    lb.run_for(2 * S);
    set_active_operator(&mut lb, "robot", Some("alice"));
    lb.run_for(2 * S);
    send_action(&mut lb, "alice", 1.0).unwrap();
    lb.run_for(S);
    assert!(actions(&lb.events("robot")).is_empty(), "300 ms late is past the action max age");

    lb.set_link("alice", "robot", LinkConfig { latency_us: 5 * MS, ..Default::default() });
    send_action(&mut lb, "alice", 2.0).unwrap();
    lb.run_for(50 * MS);
    assert_eq!(actions(&lb.events("robot")), vec![("alice".into(), 2.0, true)]);
}

// --- Arbitration ---

fn three_peers(caps: TransportCaps) -> Loopback {
    let mut lb = session(caps);
    join(&mut lb, "robot", robot_config());
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    join(&mut lb, "bob", operator_config(ActionSubscription::All));
    lb.run_for(50 * MS);
    lb
}

fn handoff(caps: TransportCaps) {
    let mut lb = three_peers(caps);
    assert_eq!(lb.core("robot").operators(), vec!["alice", "bob"]);

    send_action(&mut lb, "alice", 1.0).unwrap();
    send_action(&mut lb, "bob", 2.0).unwrap();
    lb.run_for(50 * MS);
    assert!(actions(&lb.events("robot")).is_empty(), "no active operator: everything dropped");

    // Alice takes control by asking the robot over RPC.
    set_active_operator(&mut lb, "alice", Some("alice"));
    lb.run_for(50 * MS);
    assert_eq!(lb.core("robot").active_operator().as_deref(), Some("alice"));
    assert_eq!(lb.core("bob").active_operator().as_deref(), Some("alice"));
    let robot_events = lb.events("robot");
    assert!(has(
        &robot_events,
        |e| matches!(e, Event::ActiveOperatorChanged(Some(id)) if id == "alice")
    ));
    let alice_events = lb.events("alice");
    assert!(has(&alice_events, |e| matches!(e, Event::RpcResult { result: Ok(_), .. })));

    lb.events("bob");
    send_action(&mut lb, "alice", 3.0).unwrap();
    send_action(&mut lb, "bob", 4.0).unwrap();
    lb.run_for(50 * MS);
    assert_eq!(actions(&lb.events("robot")), vec![("alice".into(), 3.0, true)]);
    // Bob subscribes to All: alice's action plus his own shadow echo.
    let mut bob = actions(&lb.events("bob"));
    bob.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(bob, vec![("alice".into(), 3.0, true), ("bob".into(), 4.0, false)]);

    // Handoff to bob, by the robot this time.
    set_active_operator(&mut lb, "robot", Some("bob"));
    lb.run_for(50 * MS);
    lb.events("robot");
    send_action(&mut lb, "alice", 5.0).unwrap();
    send_action(&mut lb, "bob", 6.0).unwrap();
    lb.run_for(50 * MS);
    assert_eq!(actions(&lb.events("robot")), vec![("bob".into(), 6.0, true)]);
    assert_eq!(lb.core("alice").active_operator().as_deref(), Some("bob"));
}

#[test]
fn only_the_active_operators_actions_reach_the_robot() {
    handoff(pubsub_caps());
}

#[test]
fn only_the_active_operators_actions_reach_the_robot_on_native_rpc() {
    handoff(room_caps());
}

fn late_join(caps: TransportCaps) {
    let mut lb = session(caps);
    join(&mut lb, "robot", robot_config());
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    lb.run_for(20 * MS);
    set_active_operator(&mut lb, "robot", Some("alice"));
    lb.run_for(20 * MS);

    join(&mut lb, "late", operator_config(ActionSubscription::None));
    // One link latency, far short of a presence heartbeat: the late peer
    // starts from the robot's latest presence, it doesn't wait for the next.
    lb.run_for(10 * MS);
    assert_eq!(lb.core("late").active_operator().as_deref(), Some("alice"));
    let events = lb.events("late");
    assert!(has(&events, |e| matches!(e, Event::ActiveOperatorChanged(Some(id)) if id == "alice")));
    assert_eq!(lb.core("late").robot_identity().as_deref(), Some("robot"));
}

#[test]
fn late_joining_operator_sees_the_active_operator() {
    late_join(pubsub_caps());
}

#[test]
fn late_joining_operator_sees_the_active_operator_on_native_presence() {
    late_join(room_caps());
}

fn token_seeded(caps: TransportCaps) {
    let mut lb = session(caps);
    let mut robot = PeerSpec::new("robot", robot_config());
    robot.active_operator = Some("alice".into());
    lb.join(robot).unwrap();
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    lb.run_for(20 * MS);
    send_action(&mut lb, "alice", 1.0).unwrap();
    lb.run_for(20 * MS);
    assert_eq!(actions(&lb.events("robot")), vec![("alice".into(), 1.0, true)]);
    assert_eq!(lb.core("alice").active_operator().as_deref(), Some("alice"));
}

#[test]
fn token_seeded_active_operator_gates_from_the_start() {
    token_seeded(room_caps());
}

#[test]
fn seeded_active_operator_gates_from_the_start_on_pubsub() {
    token_seeded(pubsub_caps());
}

// --- Stalls ---

/// cam1 keeps streaming, cam2 goes silent after its first frames; each stall
/// behavior resolves the moments cam2 cannot match.
fn stalled(behavior: StallBehavior) -> Loopback {
    let mut lb = session(pubsub_caps());
    let mut robot = robot_config();
    robot.add_video("cam1", Codec::Raw, VideoOptions::default());
    robot.add_video("cam2", Codec::Raw, VideoOptions::default());
    join(&mut lb, "robot", robot);
    let mut op = operator_config(ActionSubscription::None);
    op.add_video("cam1", Codec::Raw, VideoOptions::default());
    op.add_video("cam2", Codec::Raw, VideoOptions::default());
    op.set_observation_sync(true);
    op.set_fps(30);
    op.set_stall_behavior(behavior);
    op.set_max_lag_ms(100);
    join(&mut lb, "op", op);
    lb.run_for(20 * MS);
    for i in 0..30 {
        let ts = send_state(&mut lb, "robot", i as f64);
        send_frame(&mut lb, "robot", "cam1", ts);
        if i < 3 {
            send_frame(&mut lb, "robot", "cam2", ts);
        }
        lb.run_for(33 * MS);
    }
    lb
}

#[test]
fn stall_drop_discards_moments_without_the_silent_track() {
    let mut lb = stalled(StallBehavior::Drop);
    let events = lb.events("op");
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
    assert_eq!(lb.core("op").metrics().sync.states_dropped as usize, dropped);
}

#[test]
fn stall_freeze_reuses_the_last_frame() {
    let mut lb = stalled(StallBehavior::Freeze);
    let events = lb.events("op");
    let obs = observations(&events);
    assert!(obs.len() >= 23, "moments keep flowing, got {}", obs.len());
    let last = obs.last().expect("observation");
    assert_eq!(last.frames["cam1"].source, FrameSource::Live);
    assert_eq!(last.frames["cam2"].source, FrameSource::Stale);
    assert!(last.frames["cam2"].timestamp_us < last.timestamp_us);
    assert!(lb.core("op").metrics().sync.stale_observations_emitted > 0);
}

#[test]
fn stall_omit_substitutes_a_placeholder() {
    let mut lb = stalled(StallBehavior::Omit);
    let events = lb.events("op");
    let obs = observations(&events);
    assert!(obs.len() >= 23, "moments keep flowing, got {}", obs.len());
    let last = obs.last().expect("observation");
    let cam2 = &last.frames["cam2"];
    assert_eq!(cam2.source, FrameSource::Omitted);
    assert_eq!((cam2.width, cam2.height), (SIDE, SIDE));
    assert_eq!(cam2.data, livekit_portal_core::placeholder::render(SIDE, SIDE, "cam2"));
    assert!(lb.core("op").metrics().sync.frames_omitted["cam2"] > 0);
}

// --- Clock ---

#[test]
fn clock_sync_converges_under_skew_and_jitter() {
    let link = LinkConfig { latency_us: 4 * MS, jitter_us: 6 * MS, loss: 0.1, reorder: true };
    let mut lb = session_with(pubsub_caps(), link);
    join(&mut lb, "robot", robot_config());
    // Two operators on a broadcast transport both hear every pong; each
    // must use only the answers to its own pings.
    let skews = [("op", -7 * S as i64), ("op2", 3 * S as i64)];
    for (id, skew) in skews {
        let mut spec = PeerSpec::new(id, operator_config(ActionSubscription::None));
        spec.skew_us = skew;
        lb.join(spec).unwrap();
    }
    lb.run_for(30 * S);

    for (id, skew) in skews {
        let synced = lb.events(id).iter().filter(|e| matches!(e, Event::TimeSynced)).count();
        assert_eq!(synced, 1, "{id}: one sync, no resyncs");
        let m = lb.core(id).metrics();
        assert!(m.time_sync.synced, "{id}");
        let uncertainty = m.time_sync.uncertainty_us.expect("synced");
        let offset = -skew;
        assert!(m.time_sync.offset_us.abs_diff(offset) <= uncertainty, "{id}: {m:?}");
        assert!(uncertainty <= 7 * MS, "{id}: min-rtt sample is tight, got {uncertainty}");
        assert_eq!(m.time_sync.samples_rejected, 0, "{id}: no foreign pong got in");
        assert!(m.rtt.pongs_received > 25 && m.rtt.pings_sent >= m.rtt.pongs_received, "{id}");

        let robot_now = lb.with_core("robot", |c, now| Ok((c.now_us(now), vec![]))).unwrap();
        let op_now = lb.with_core(id, |c, now| Ok((c.now_us(now), vec![]))).unwrap();
        assert!(robot_now.abs_diff(op_now) <= uncertainty + 1, "{id}: clocks agree");
    }
}

// --- Presence ---

#[test]
fn operator_leaving_is_reported_and_the_pointer_stays_pinned() {
    let mut lb = session(room_caps());
    join(&mut lb, "robot", robot_config());
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    lb.run_for(20 * MS);
    assert!(has(&lb.events("robot"), |e| matches!(e, Event::OperatorJoined(id) if id == "alice")));
    set_active_operator(&mut lb, "robot", Some("alice"));
    lb.run_for(20 * MS);

    lb.leave("alice", true);
    lb.run_for(20 * MS);
    assert!(has(&lb.events("robot"), |e| matches!(e, Event::OperatorLeft(id) if id == "alice")));
    assert!(lb.core("robot").operators().is_empty());
    assert_eq!(lb.core("robot").active_operator().as_deref(), Some("alice"));

    // The same identity rejoining resumes control.
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    lb.run_for(20 * MS);
    send_action(&mut lb, "alice", 9.0).unwrap();
    lb.run_for(20 * MS);
    assert_eq!(actions(&lb.events("robot")), vec![("alice".into(), 9.0, true)]);
}

#[test]
fn silent_peer_is_dropped_after_the_heartbeat_timeout() {
    let mut lb = session(pubsub_caps());
    join(&mut lb, "robot", robot_config());
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    lb.run_for(20 * MS);
    set_active_operator(&mut lb, "robot", Some("alice"));
    lb.run_for(5 * S);
    assert_eq!(lb.core("robot").operators(), vec!["alice"], "heartbeats keep alice alive");
    assert!(!has(&lb.events("robot"), |e| matches!(e, Event::OperatorLeft(_))));

    // Alice vanishes without a word.
    lb.leave("alice", false);
    lb.run_for(PRESENCE_TIMEOUT_US - 100 * MS);
    assert_eq!(lb.core("robot").operators(), vec!["alice"], "not yet");
    lb.run_for(S);
    assert!(has(&lb.events("robot"), |e| matches!(e, Event::OperatorLeft(id) if id == "alice")));
    assert!(lb.core("robot").operators().is_empty());
    assert_eq!(
        lb.core("robot").active_operator().as_deref(),
        Some("alice"),
        "pointer stays pinned"
    );

    // So does the operator's view of a vanished robot.
    join(&mut lb, "alice", operator_config(ActionSubscription::None));
    lb.run_for(20 * MS);
    assert_eq!(lb.core("alice").robot_identity().as_deref(), Some("robot"));
    lb.leave("robot", false);
    lb.run_for(PRESENCE_TIMEOUT_US + S);
    assert_eq!(lb.core("alice").robot_identity(), None);
}

fn version_mismatch(caps: TransportCaps) {
    let mut lb = session(caps);
    let mut robot = PeerSpec::new("robot", robot_config());
    robot.active_operator = Some("old".into());
    lb.join(robot).unwrap();
    let mut old = PeerSpec::new("old", operator_config(ActionSubscription::None));
    old.protocol_version = Some("2".into());
    lb.join(old).unwrap();
    lb.run_for(20 * MS);

    assert!(lb.core("robot").operators().is_empty());
    assert!(lb.events("robot").is_empty());
    assert_eq!(lb.core("old").robot_identity().as_deref(), Some("robot"));
    send_action(&mut lb, "old", 1.0).unwrap();
    lb.run_for(20 * MS);
    assert!(actions(&lb.events("robot")).is_empty(), "even an active pointer can't admit it");
}

#[test]
fn protocol_version_mismatch_keeps_peers_apart() {
    version_mismatch(room_caps());
}

#[test]
fn protocol_version_mismatch_keeps_peers_apart_on_pubsub() {
    version_mismatch(pubsub_caps());
}

#[test]
fn reconnect_rebuilds_rosters_but_keeps_the_pointer() {
    for caps in [room_caps(), pubsub_caps()] {
        let mut lb = session(caps);
        join(&mut lb, "robot", robot_config());
        join(&mut lb, "alice", operator_config(ActionSubscription::None));
        lb.run_for(20 * MS);
        set_active_operator(&mut lb, "robot", Some("alice"));
        lb.run_for(20 * MS);
        lb.feed("robot", Input::LinkRestored);
        assert!(lb.core("robot").operators().is_empty());
        assert_eq!(lb.core("robot").active_operator().as_deref(), Some("alice"));
        if !caps.native_presence {
            // The next heartbeat rebuilds the roster.
            lb.run_for(2 * S);
            assert_eq!(lb.core("robot").operators(), vec!["alice"]);
        }
    }
}

// --- RPC ---

type Replies = Vec<(RpcId, Result<String, RpcError>)>;

fn rpc_session() -> Loopback {
    let mut lb = session(room_caps());
    join(&mut lb, "robot", robot_config());
    join(&mut lb, "teleop", operator_config(ActionSubscription::None));
    let mut observer = PortalConfig::new("s", Role::Observer);
    observer.add_state_typed([("j", DType::F64)]);
    join(&mut lb, "recorder", observer);
    lb.run_for(20 * MS);
    lb.events("robot");
    lb
}

/// Feed the robot a native `set_active_operator` RPC and collect replies.
fn call(lb: &mut Loopback, from: &str, rpc: u64) -> Replies {
    let request =
        RpcRequest { id: RpcId(rpc), method: SET_ACTIVE_OPERATOR_RPC, from, payload: from };
    let out = lb.handle("robot", Input::RpcRequest(request));
    replies(lb, out)
}

/// The robot's replies, with presence updates confirmed along the way.
fn replies(lb: &mut Loopback, out: Vec<Output<VideoFrameData>>) -> Replies {
    let mut replies = Vec::new();
    let mut rest = Vec::new();
    for o in out {
        match o {
            Output::RpcReply { id, result } => replies.push((id, result)),
            Output::SetPresence { id: Some(id), .. } => {
                let more = lb.handle("robot", Input::PresenceSet { id, result: Ok(()) });
                replies.extend(self::replies(lb, more));
            }
            other => rest.push(other),
        }
    }
    lb.execute("robot", rest);
    replies
}

fn wake_robot(lb: &mut Loopback, after: u64) -> Replies {
    lb.advance_clock(after);
    let out = lb.handle("robot", Input::Wake);
    replies(lb, out)
}

#[test]
fn only_operators_and_observers_may_steer() {
    let mut lb = rpc_session();
    assert!(matches!(&call(&mut lb, "teleop", 0)[..], [(RpcId(0), Ok(_))]));
    assert!(matches!(&call(&mut lb, "recorder", 1)[..], [(RpcId(1), Ok(_))]));
    assert!(call(&mut lb, "stranger", 2).is_empty(), "a stranger waits for its role");
    let refused = wake_robot(&mut lb, RPC_CALLER_ROSTER_WAIT_US);
    assert!(matches!(&refused[..], [(RpcId(2), Err(e))] if e.code == RPC_NOT_AUTHORIZED));
    assert_eq!(lb.core("robot").active_operator().as_deref(), Some("recorder"));
}

#[test]
fn waiting_strangers_do_not_hold_up_other_callers() {
    let mut lb = rpc_session();
    for i in 0..50 {
        assert!(call(&mut lb, &format!("s{i}"), i).is_empty());
    }
    assert!(call(&mut lb, "late", 100).is_empty());
    let deadline = lb.local_now("robot") + RPC_CALLER_ROSTER_WAIT_US;
    assert!(lb.core("robot").next_wake_us().is_some_and(|at| at <= deadline));

    // A known caller is answered at once, without waiting on the strangers.
    assert!(matches!(&call(&mut lb, "teleop", 200)[..], [(RpcId(200), Ok(_))]));

    // A caller whose role shows up during the wait is let in.
    lb.advance_clock(500 * MS);
    let presence = livekit_portal_core::Presence::local(Role::Operator);
    let out = lb.handle("robot", Input::PresenceChanged { peer: "late", presence });
    let admitted = replies(&mut lb, out);
    assert!(matches!(&admitted[..], [(RpcId(100), Ok(_))]));

    // Refused together after one wait, not one wait each.
    let refused = wake_robot(&mut lb, RPC_CALLER_ROSTER_WAIT_US - 500 * MS);
    assert_eq!(refused.len(), 50);
    assert!(refused.iter().all(|(_, r)| matches!(r, Err(e) if e.code == RPC_NOT_AUTHORIZED)));
}

#[test]
fn a_stranger_cannot_steer_over_the_polyfill() {
    let mut lb = session(pubsub_caps());
    join(&mut lb, "robot", robot_config());
    // Speaks another version, so it never joins a roster.
    let mut stranger = PeerSpec::new("mallory", operator_config(ActionSubscription::None));
    stranger.protocol_version = Some("2".into());
    lb.join(stranger).unwrap();
    lb.run_for(50 * MS);
    let id = lb
        .with_core("mallory", |c, now_us| {
            let call = PerformRpc {
                to: Some("robot".into()),
                method: SET_ACTIVE_OPERATOR_RPC.into(),
                payload: "mallory".into(),
                timeout_ms: None,
                now_us,
            };
            c.perform_rpc(call)
        })
        .unwrap();
    lb.run_for(RPC_CALLER_ROSTER_WAIT_US + 100 * MS);
    let events = lb.events("mallory");
    assert!(has(&events, |e| matches!(e,
        Event::RpcResult { id: got, result: Err(err) } if *got == id && err.code == RPC_NOT_AUTHORIZED)));
    assert_eq!(lb.core("robot").active_operator(), None);
}

#[test]
fn application_rpc_runs_over_the_polyfill() {
    for caps in [pubsub_caps(), room_caps()] {
        let mut lb = session(caps);
        join(&mut lb, "robot", robot_config());
        join(&mut lb, "alice", operator_config(ActionSubscription::None));
        lb.run_for(20 * MS);
        lb.on_rpc("robot", Box::new(|method, payload| Ok(format!("{method}:{payload}"))));
        let call = |method: &str| {
            let method = method.to_string();
            move |c: &mut PortalCore<VideoFrameData>, now_us| {
                // No destination: the robot is the operator's counterpart.
                let call =
                    PerformRpc { to: None, method, payload: "x".into(), timeout_ms: None, now_us };
                c.perform_rpc(call)
            }
        };
        let id = lb.with_core("alice", call("echo")).unwrap();
        lb.run_for(20 * MS);
        let events = lb.events("alice");
        assert!(has(&events, |e| matches!(e,
            Event::RpcResult { id: got, result: Ok(r) } if *got == id && r == "echo:x")));

        // A peer that leaves mid-call times the call out (polyfill) or fails
        // it at once (native).
        let id = lb.with_core("alice", call("echo")).unwrap();
        lb.leave("robot", false);
        lb.run_for(16 * S);
        let events = lb.events("alice");
        assert!(has(
            &events,
            |e| matches!(e, Event::RpcResult { id: got, result: Err(_) } if *got == id)
        ));
    }
}

#[test]
fn keypoints_reach_everyone_and_bad_ones_warn_once() {
    let mut lb = rpc_session();
    lb.with_core("teleop", |c, now_us| {
        let send = SendKeypoint {
            kind: "recording",
            payload: serde_json::Map::new(),
            timestamp_us: Some(42),
            now_us,
        };
        Ok(((), c.send_keypoint(send)?))
    })
    .unwrap();
    lb.run_for(20 * MS);
    for id in ["robot", "recorder", "teleop"] {
        let kps: Vec<_> = lb
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
        let input =
            Input::Data { stream: Stream::Keypoint, from: Some("teleop"), payload: b"garbage" };
        lb.feed("robot", input);
    }
    let input = Input::Data { stream: Stream::Keypoint, from: Some("stranger"), payload: b"{}" };
    lb.feed("robot", input);
    assert!(lb.events("robot").is_empty());
}

#[test]
fn sends_check_role_and_connection() {
    let new = |config| -> PortalCore<VideoFrameData> {
        PortalCore::new(CoreInit { config, caps: pubsub_caps() })
    };
    let mut op = new(operator_config(ActionSubscription::All));
    let values = HashMap::new();
    let err =
        op.send_state(SendState { values: &values, timestamp_us: None, now_us: 1 }).unwrap_err();
    assert!(matches!(err, PortalError::WrongRole(Role::Operator)));
    let one = HashMap::from([("v".to_string(), TypedValue::F64(1.0))]);
    let send =
        |values| SendAction { values, timestamp_us: None, in_reply_to_ts_us: None, now_us: 1 };
    assert!(
        matches!(op.send_action(send(&one)), Err(PortalError::WrongRole(_))),
        "not connected yet"
    );
    let mut robot = new(robot_config());
    let clear = || SetActiveOperator { identity: None, now_us: 1 };
    assert!(matches!(robot.set_active_operator(clear()), Err(PortalError::NotConnected)));
    assert!(matches!(op.set_active_operator(clear()), Err(PortalError::NoPeer)));

    let connect = |id: &str| Connect {
        local_peer: id.into(),
        active_operator: None,
        peers: vec![],
        now_us: 1,
    };
    op.connect(connect("op")).unwrap();
    assert!(matches!(op.connect(connect("op")), Err(PortalError::AlreadyConnected)));
    assert!(matches!(op.set_active_operator(clear()), Err(PortalError::NoPeer)), "no robot yet");
    op.send_action(send(&one)).unwrap();
    let bad = HashMap::from([("v".to_string(), TypedValue::Bool(true))]);
    assert!(matches!(op.send_action(send(&bad)), Err(PortalError::DtypeMismatch { .. })));
}

// --- Caps ---

#[test]
fn webrtc_codec_on_the_loopback_is_refused() {
    let mut lb = session(pubsub_caps());
    let mut robot = robot_config();
    robot.add_video("cam", Codec::H264, VideoOptions::default());
    let err = lb.join(PeerSpec::new("robot", robot)).unwrap_err();
    assert!(
        matches!(&err, PortalError::Unsupported(m) if m.contains("RAW, PNG or MJPEG")),
        "{err}"
    );
    assert_eq!(lb.in_flight(), 0, "nothing was published");

    // The same config is fine on a transport that encodes video.
    let mut lb = session(room_caps());
    let mut robot = robot_config();
    robot.add_video("cam", Codec::H264, VideoOptions::default());
    lb.join(PeerSpec::new("robot", robot)).unwrap();
}

#[test]
fn e2ee_key_on_the_loopback_is_refused() {
    let mut lb = session(pubsub_caps());
    let mut op = operator_config(ActionSubscription::None);
    op.set_e2ee_key(b"shared secret".to_vec());
    let err = lb.join(PeerSpec::new("op", op)).unwrap_err();
    assert!(matches!(&err, PortalError::Unsupported(m) if m.contains("plaintext")), "{err}");
    assert_eq!(lb.in_flight(), 0, "nothing was published in the clear");
}
