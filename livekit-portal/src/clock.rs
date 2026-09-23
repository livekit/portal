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

//! Time sync: every non-robot peer estimates the robot's clock from an
//! NTP-style ping/pong on `portal_clock`, and `Portal::now_us()` reads that
//! estimate through a clock that never repeats and never goes backwards.
//!
//! The pieces are split so the math is testable without a room:
//!   * `ClockPacket` — wire codec
//!   * `OffsetEstimator` — samples in, offset out
//!   * `MonotonicClock` — raw time + offset in, timestamps out
//!   * `SyncedClock` — the shared, locked handle `Portal` reads
//!   * `ClockService` — the actor that pings, answers and feeds the estimator

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use livekit::prelude::*;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::metrics::{MetricsRegistry, TimeSyncMetrics};
use crate::portal::ControllerState;
use crate::types::Role;

pub(crate) const CLOCK_TOPIC: &str = "portal_clock";

const PING_KIND: u8 = 0;
const PONG_KIND: u8 = 1;
const PING_LEN: usize = 1 + 4 + 8;
const PONG_LEN: usize = 1 + 4 + 8 + 8 + 8;

const UNSYNCED_PING_INTERVAL: Duration = Duration::from_millis(250);
const SYNCED_PING_INTERVAL: Duration = Duration::from_secs(1);
const WINDOW_US: u64 = 10_000_000;
/// Samples needed before the first estimate is trusted: one second at the
/// unsynced rate, enough for the min-rtt pick to skip an unlucky first sample.
const MIN_SAMPLES_TO_SYNC: usize = 4;
const JUMP_THRESHOLD_US: u64 = 1_000_000;
const RESYNC_RUN: usize = 30;
/// Jump candidates within this distance of each other count as agreeing.
const JUMP_AGREEMENT_US: u64 = 100_000;
/// A backward correction removes at most `elapsed / SLEW_DIVISOR` per step,
/// so the clock keeps running at no less than 95% of real speed.
const SLEW_DIVISOR: u64 = 20;
const CLOCK_QUEUE_CAP: usize = 64;

// --- Wire ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockPacket {
    Ping { seq: u32, t1: u64 },
    Pong { seq: u32, t1: u64, t2: u64, t3: u64 },
}

impl ClockPacket {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            ClockPacket::Ping { seq, t1 } => {
                let mut buf = Vec::with_capacity(PING_LEN);
                buf.push(PING_KIND);
                buf.extend_from_slice(&seq.to_le_bytes());
                buf.extend_from_slice(&t1.to_le_bytes());
                buf
            }
            ClockPacket::Pong { seq, t1, t2, t3 } => {
                let mut buf = Vec::with_capacity(PONG_LEN);
                buf.push(PONG_KIND);
                buf.extend_from_slice(&seq.to_le_bytes());
                buf.extend_from_slice(&t1.to_le_bytes());
                buf.extend_from_slice(&t2.to_le_bytes());
                buf.extend_from_slice(&t3.to_le_bytes());
                buf
            }
        }
    }

    pub fn decode(payload: &[u8]) -> Option<Self> {
        let (&kind, rest) = payload.split_first()?;
        match (kind, payload.len()) {
            (PING_KIND, PING_LEN) => {
                Some(ClockPacket::Ping { seq: read_u32(rest, 0), t1: read_u64(rest, 4) })
            }
            (PONG_KIND, PONG_LEN) => Some(ClockPacket::Pong {
                seq: read_u32(rest, 0),
                t1: read_u64(rest, 4),
                t2: read_u64(rest, 12),
                t3: read_u64(rest, 20),
            }),
            _ => None,
        }
    }
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("length checked by decode"))
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("length checked by decode"))
}

// --- Estimator ---

/// One completed exchange. `offset_us` is robot clock minus local clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    pub offset_us: i64,
    pub rtt_us: u64,
    /// Local receive time (`t4`), used to age samples out of the window.
    pub at_us: u64,
}

/// Timestamps of one ping/pong exchange: `t1` ping sent (local), `t2` ping
/// received (robot), `t3` pong sent (robot), `t4` pong received (local).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Exchange {
    pub t1: u64,
    pub t2: u64,
    pub t3: u64,
    pub t4: u64,
}

impl TryFrom<Exchange> for Sample {
    type Error = ();

    /// Fails when the timestamps are inconsistent (the robot claims to have
    /// spent longer answering than the whole round trip took).
    fn try_from(e: Exchange) -> Result<Self, ()> {
        let round_trip = e.t4.checked_sub(e.t1).ok_or(())?;
        let robot_hold = e.t3.checked_sub(e.t2).ok_or(())?;
        let rtt_us = round_trip.checked_sub(robot_hold).ok_or(())?;
        let offset_us =
            ((e.t2 as i128 - e.t1 as i128) + (e.t3 as i128 - e.t4 as i128)).div_euclid(2) as i64;
        Ok(Sample { offset_us, rtt_us, at_us: e.t4 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EstimatorEvent {
    /// Not enough samples yet.
    Pending,
    /// First trusted estimate.
    Synced,
    /// Estimate refined within the current timeline.
    Updated,
    /// Sample implied an implausible jump and was ignored.
    Rejected,
    /// Enough consecutive samples agreed on a jump that the timeline moved.
    Resynced,
}

#[derive(Debug, Default)]
pub(crate) struct OffsetEstimator {
    window: VecDeque<Sample>,
    synced: bool,
    /// Consecutive samples that each implied a jump and agree with each other.
    jump_run: Vec<Sample>,
    resyncs: u64,
    samples_rejected: u64,
}

impl OffsetEstimator {
    pub fn push(&mut self, sample: Sample) -> EstimatorEvent {
        let Some(best) = self.best() else {
            self.admit(sample);
            return self.pending_or_synced();
        };
        if best.offset_us.abs_diff(sample.offset_us) <= JUMP_THRESHOLD_US {
            self.jump_run.clear();
            self.admit(sample);
            return self.pending_or_synced();
        }

        let agrees = self
            .jump_run
            .first()
            .is_none_or(|first| first.offset_us.abs_diff(sample.offset_us) <= JUMP_AGREEMENT_US);
        if !agrees {
            self.jump_run.clear();
        }
        self.jump_run.push(sample);
        if self.jump_run.len() < RESYNC_RUN {
            self.samples_rejected += 1;
            return EstimatorEvent::Rejected;
        }

        self.window = self.jump_run.drain(..).collect();
        self.resyncs += 1;
        self.synced = true;
        EstimatorEvent::Resynced
    }

    /// Current estimate, once synced.
    pub fn offset_us(&self) -> Option<i64> {
        self.synced.then(|| self.best()).flatten().map(|s| s.offset_us)
    }

    /// Worst-case error of `offset_us`: the true offset lies within half the
    /// round trip of the sample it came from.
    pub fn uncertainty_us(&self) -> Option<u64> {
        self.synced.then(|| self.best()).flatten().map(|s| s.rtt_us.div_ceil(2))
    }

    pub fn is_synced(&self) -> bool {
        self.synced
    }

    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    pub fn samples_rejected(&self) -> u64 {
        self.samples_rejected
    }

    pub fn reset_counters(&mut self) {
        self.resyncs = 0;
        self.samples_rejected = 0;
    }

    fn admit(&mut self, sample: Sample) {
        self.window.push_back(sample);
        let horizon = sample.at_us.saturating_sub(WINDOW_US);
        while self.window.front().is_some_and(|s| s.at_us < horizon) {
            self.window.pop_front();
        }
    }

    fn pending_or_synced(&mut self) -> EstimatorEvent {
        match (self.synced, self.window.len() >= MIN_SAMPLES_TO_SYNC) {
            (true, _) => EstimatorEvent::Updated,
            (false, true) => {
                self.synced = true;
                EstimatorEvent::Synced
            }
            (false, false) => EstimatorEvent::Pending,
        }
    }

    fn best(&self) -> Option<Sample> {
        self.window.iter().min_by_key(|s| s.rtt_us).copied()
    }
}

// --- Monotonic clock ---

/// Turns a raw local time plus the current offset estimate into timestamps
/// that strictly increase. Forward corrections apply at once; backward ones
/// are absorbed gradually so time never runs backwards or stands still.
#[derive(Debug, Default)]
pub(crate) struct MonotonicClock {
    applied_offset_us: i64,
    target_offset_us: i64,
    last_raw_us: Option<u64>,
    last_out_us: u64,
}

impl MonotonicClock {
    pub fn set_target(&mut self, offset_us: i64) {
        self.target_offset_us = offset_us;
        if offset_us > self.applied_offset_us {
            self.applied_offset_us = offset_us;
        }
    }

    pub fn now(&mut self, raw_us: u64) -> u64 {
        let elapsed = self.last_raw_us.map_or(0, |last| raw_us.saturating_sub(last));
        self.last_raw_us = Some(raw_us);

        let behind = self.applied_offset_us - self.target_offset_us;
        if behind > 0 {
            let step = (elapsed / SLEW_DIVISOR).min(behind as u64) as i64;
            self.applied_offset_us -= step;
        }

        let candidate = raw_us.saturating_add_signed(self.applied_offset_us);
        self.last_out_us = candidate.max(self.last_out_us + 1);
        self.last_out_us
    }

    /// Offset currently in effect, which lags the target while slewing back.
    pub fn applied_offset_us(&self) -> i64 {
        self.applied_offset_us
    }
}

/// Local time for the clock: wall time at first use, advanced by a monotonic
/// `Instant` so host clock steps (NTP) never reach Portal timestamps.
fn raw_now_us() -> u64 {
    static ANCHOR: OnceLock<(Instant, u64)> = OnceLock::new();
    let (instant, wall_us) = ANCHOR.get_or_init(|| {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before 1970");
        (Instant::now(), wall.as_micros() as u64)
    });
    wall_us + instant.elapsed().as_micros() as u64
}

// --- Shared handle ---

type TimeSyncedCb = Box<dyn Fn() + Send + Sync>;

struct ClockState {
    clock: MonotonicClock,
    estimator: OffsetEstimator,
    /// Robot the estimator's samples came from. A different robot means a
    /// different timeline, so the estimator starts over.
    robot_identity: Option<String>,
}

/// The clock a `Portal` reads. Outlives connections so `now_us()` stays
/// continuous across a reconnect.
pub(crate) struct SyncedClock {
    is_reference: bool,
    skew_us: i64,
    state: Mutex<ClockState>,
    on_synced: Mutex<Option<TimeSyncedCb>>,
}

impl SyncedClock {
    /// `is_reference` is true on the robot: its own clock is the timeline,
    /// so it is synced from the start. `skew_us` shifts the raw clock and
    /// exists for tests that simulate hosts with different clocks.
    pub fn new(is_reference: bool, skew_us: i64) -> Self {
        let estimator = OffsetEstimator { synced: is_reference, ..Default::default() };
        Self {
            is_reference,
            skew_us,
            state: Mutex::new(ClockState {
                clock: MonotonicClock::default(),
                estimator,
                robot_identity: None,
            }),
            on_synced: Mutex::new(None),
        }
    }

    pub fn now_us(&self) -> u64 {
        let raw = raw_now_us().saturating_add_signed(self.skew_us);
        self.state.lock().clock.now(raw)
    }

    /// Raw local time, the clock the ping/pong timestamps are taken on.
    fn raw_us(&self) -> u64 {
        raw_now_us().saturating_add_signed(self.skew_us)
    }

    pub fn is_synced(&self) -> bool {
        self.state.lock().estimator.is_synced()
    }

    pub fn set_on_synced(&self, cb: TimeSyncedCb) {
        *self.on_synced.lock() = Some(cb);
    }

    pub fn metrics(&self) -> TimeSyncMetrics {
        let state = self.state.lock();
        TimeSyncMetrics {
            synced: state.estimator.is_synced(),
            offset_us: state.clock.applied_offset_us(),
            uncertainty_us: match self.is_reference {
                true => Some(0),
                false => state.estimator.uncertainty_us(),
            },
            resyncs: state.estimator.resyncs(),
            samples_rejected: state.estimator.samples_rejected(),
        }
    }

    pub fn reset_counters(&self) {
        self.state.lock().estimator.reset_counters();
    }

    /// Drops the estimate but keeps the applied offset, so `now_us()` stays
    /// continuous until a new estimate arrives.
    fn restart(&self, robot_identity: Option<String>) {
        let mut state = self.state.lock();
        state.estimator = OffsetEstimator::default();
        state.robot_identity = robot_identity;
    }

    fn record(&self, robot_identity: &str, sample: Sample) -> EstimatorEvent {
        let mut state = self.state.lock();
        if state.robot_identity.as_deref() != Some(robot_identity) {
            state.estimator = OffsetEstimator::default();
            state.robot_identity = Some(robot_identity.to_string());
        }
        let event = state.estimator.push(sample);
        if let Some(offset) = state.estimator.offset_us() {
            state.clock.set_target(offset);
        }
        event
    }

    fn fire_synced(&self) {
        if let Some(cb) = self.on_synced.lock().as_ref()
            && catch_unwind(AssertUnwindSafe(cb)).is_err()
        {
            log::error!("[callback-panic] on_time_synced callback panicked");
        }
    }
}

// --- Service ---

struct Inbound {
    packet: ClockPacket,
    sender: String,
    /// Local raw time the packet arrived, taken in the event handler so
    /// queueing delay doesn't count as network delay.
    received_raw_us: u64,
}

/// Handle to the running clock actor. Dropping it stops the actor.
pub(crate) struct ClockService {
    tx: mpsc::Sender<Inbound>,
    clock: Arc<SyncedClock>,
    task: JoinHandle<()>,
}

impl ClockService {
    pub fn spawn(actor: ClockActor) -> Self {
        let (tx, rx) = mpsc::channel(CLOCK_QUEUE_CAP);
        let clock = actor.clock.clone();
        let task = tokio::spawn(actor.run(rx));
        Self { tx, clock, task }
    }

    /// Called from the room event handler for packets on `CLOCK_TOPIC`.
    pub fn handle_packet(&self, payload: &[u8], sender: &str) {
        let received_raw_us = self.clock.raw_us();
        let Some(packet) = ClockPacket::decode(payload) else {
            return;
        };
        // Drop on full: a lost sample only delays the estimate by one ping.
        let _ = self.tx.try_send(Inbound { packet, sender: sender.to_string(), received_raw_us });
    }

    /// The robot left. Whoever answers next may be a restarted robot with a
    /// new clock, so the estimate starts over instead of rejecting the jump.
    pub fn robot_left(&self) {
        self.clock.restart(None);
    }
}

impl Drop for ClockService {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) struct ClockActor {
    pub role: Role,
    pub local_participant: LocalParticipant,
    pub controller: Arc<ControllerState>,
    pub clock: Arc<SyncedClock>,
    pub metrics: Arc<MetricsRegistry>,
}

impl ClockActor {
    async fn run(self, mut rx: mpsc::Receiver<Inbound>) {
        let pings = self.role != Role::Robot;
        if pings {
            self.clock.restart(None);
        }
        let mut seq: u32 = 0;
        let mut next_ping = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_ping), if pings => {
                    self.send_ping(seq).await;
                    seq = seq.wrapping_add(1);
                    let interval = match self.clock.is_synced() {
                        true => SYNCED_PING_INTERVAL,
                        false => UNSYNCED_PING_INTERVAL,
                    };
                    next_ping += interval;
                }
                inbound = rx.recv() => {
                    let Some(inbound) = inbound else { break };
                    self.handle(inbound).await;
                }
            }
        }
    }

    async fn send_ping(&self, seq: u32) {
        let Some(robot) = self.controller.robot_identity.lock().clone() else {
            return;
        };
        let t1 = self.clock.raw_us();
        self.metrics.record_ping_sent();
        self.publish(ClockPacket::Ping { seq, t1 }, robot).await;
    }

    async fn handle(&self, inbound: Inbound) {
        match (self.role, inbound.packet) {
            (Role::Robot, ClockPacket::Ping { seq, t1 }) => {
                let t2 = inbound.received_raw_us;
                let t3 = self.clock.raw_us();
                self.publish(ClockPacket::Pong { seq, t1, t2, t3 }, inbound.sender).await;
            }
            (Role::Robot, ClockPacket::Pong { .. }) => {}
            (_, ClockPacket::Pong { t1, t2, t3, .. }) => {
                let robot = self.controller.robot_identity.lock().clone();
                if robot.as_deref() != Some(inbound.sender.as_str()) {
                    return;
                }
                let exchange = Exchange { t1, t2, t3, t4: inbound.received_raw_us };
                let Ok(sample) = Sample::try_from(exchange) else {
                    return;
                };
                self.metrics.record_rtt(sample.rtt_us);
                let event = self.clock.record(&inbound.sender, sample);
                if matches!(event, EstimatorEvent::Synced | EstimatorEvent::Resynced) {
                    self.clock.fire_synced();
                }
            }
            (_, ClockPacket::Ping { .. }) => {}
        }
    }

    async fn publish(&self, packet: ClockPacket, destination: String) {
        let data = DataPacket {
            payload: packet.encode(),
            topic: Some(CLOCK_TOPIC.to_string()),
            // Retransmits would inflate the round trip and skew the offset.
            reliable: false,
            destination_identities: vec![destination.into()],
        };
        if let Err(e) = self.local_participant.publish_data(data).await {
            log::warn!("[publish-failed] clock publish failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;
    const S: u64 = 1_000_000;

    /// An exchange with a robot `offset` ahead, `up`/`down` one-way delays,
    /// and `hold` spent on the robot, starting at local time `t1`.
    fn exchange(t1: u64, offset: i64, up: u64, down: u64, hold: u64) -> Sample {
        let t2 = (t1 + up).saturating_add_signed(offset);
        let t3 = t2 + hold;
        let t4 = t1 + up + hold + down;
        Sample::try_from(Exchange { t1, t2, t3, t4 }).expect("consistent exchange")
    }

    fn synced_estimator(offset: i64) -> (OffsetEstimator, u64) {
        let mut est = OffsetEstimator::default();
        let mut t = 100 * S;
        for _ in 0..MIN_SAMPLES_TO_SYNC {
            est.push(exchange(t, offset, 5 * MS, 5 * MS, 0));
            t += 250 * MS;
        }
        assert!(est.is_synced());
        (est, t)
    }

    #[test]
    fn packet_roundtrip() {
        let ping = ClockPacket::Ping { seq: 7, t1: 123_456 };
        let pong = ClockPacket::Pong { seq: u32::MAX, t1: 1, t2: u64::MAX, t3: 3 };
        assert_eq!(ClockPacket::decode(&ping.encode()), Some(ping));
        assert_eq!(ClockPacket::decode(&pong.encode()), Some(pong));
    }

    #[test]
    fn malformed_packets_are_ignored() {
        assert_eq!(ClockPacket::decode(&[]), None);
        assert_eq!(ClockPacket::decode(&[PING_KIND; PONG_LEN]), None);
        assert_eq!(ClockPacket::decode(&[PONG_KIND; PING_LEN]), None);
        assert_eq!(ClockPacket::decode(&[9; PING_LEN]), None);

        // Fixed-seed xorshift over random lengths and bytes: decode must never panic.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8).collect();
            let _ = ClockPacket::decode(&bytes);
        }
    }

    #[test]
    fn symmetric_delay_gives_exact_offset() {
        let s = exchange(10 * S, 5 * S as i64, 20 * MS, 20 * MS, MS);
        assert_eq!(s.offset_us, 5 * S as i64);
        assert_eq!(s.rtt_us, 40 * MS);

        let s = exchange(10 * S, -3 * S as i64, 7 * MS, 7 * MS, 0);
        assert_eq!(s.offset_us, -3 * S as i64);
    }

    #[test]
    fn asymmetric_delay_stays_within_uncertainty() {
        let mut est = OffsetEstimator::default();
        let offset = 2 * S as i64;
        for i in 0..MIN_SAMPLES_TO_SYNC as u64 {
            est.push(exchange(10 * S + i * 250 * MS, offset, 30 * MS, 2 * MS, 0));
        }
        let error = est.offset_us().expect("synced").abs_diff(offset);
        assert!(error <= est.uncertainty_us().expect("synced"));
    }

    #[test]
    fn inconsistent_exchange_is_rejected() {
        // Robot claims it held the ping longer than the round trip lasted.
        let e = Exchange { t1: 100, t2: 1_000, t3: 5_000, t4: 200 };
        assert!(Sample::try_from(e).is_err());
        let e = Exchange { t1: 200, t2: 0, t3: 0, t4: 100 };
        assert!(Sample::try_from(e).is_err());
    }

    #[test]
    fn syncs_after_min_samples() {
        let mut est = OffsetEstimator::default();
        for i in 0..MIN_SAMPLES_TO_SYNC as u64 - 1 {
            assert_eq!(est.push(exchange(i * 250 * MS, 0, MS, MS, 0)), EstimatorEvent::Pending);
            assert_eq!(est.offset_us(), None);
        }
        assert_eq!(est.push(exchange(S, 0, MS, MS, 0)), EstimatorEvent::Synced);
        assert_eq!(est.push(exchange(2 * S, 0, MS, MS, 0)), EstimatorEvent::Updated);
    }

    #[test]
    fn uses_smallest_rtt_sample_in_window() {
        let mut est = OffsetEstimator::default();
        est.push(exchange(0, 1_000, 40 * MS, 10 * MS, 0));
        est.push(exchange(S, 2_000, 2 * MS, 2 * MS, 0));
        est.push(exchange(2 * S, 3_000, 30 * MS, 30 * MS, 0));
        est.push(exchange(3 * S, 4_000, 20 * MS, 20 * MS, 0));
        assert_eq!(est.offset_us(), Some(2_000));
        assert_eq!(est.uncertainty_us(), Some(2 * MS));
    }

    #[test]
    fn old_samples_leave_the_window() {
        let mut est = OffsetEstimator::default();
        est.push(exchange(0, 1_000, MS, MS, 0));
        for i in 1..=4 {
            est.push(exchange(i * 3 * S, 9_000, 10 * MS, 10 * MS, 0));
        }
        // The best sample (t=0) is now more than 10 s older than the newest.
        assert_eq!(est.offset_us(), Some(9_000));
    }

    #[test]
    fn single_jump_is_rejected() {
        let (mut est, t) = synced_estimator(0);
        assert_eq!(est.push(exchange(t, 5 * S as i64, MS, MS, 0)), EstimatorEvent::Rejected);
        assert_eq!(est.samples_rejected(), 1);
        assert_eq!(est.offset_us(), Some(0));
    }

    #[test]
    fn agreeing_jumps_resync() {
        let (mut est, mut t) = synced_estimator(0);
        let jumped = 5 * S as i64;
        for i in 0..RESYNC_RUN - 1 {
            let jitter = (i as i64 % 3) * 10 * MS as i64;
            assert_eq!(est.push(exchange(t, jumped + jitter, MS, MS, 0)), EstimatorEvent::Rejected);
            t += S;
        }
        assert_eq!(est.push(exchange(t, jumped, MS, MS, 0)), EstimatorEvent::Resynced);
        assert_eq!(est.resyncs(), 1);
        assert_eq!(est.offset_us().map(|o| o.abs_diff(jumped) <= 20 * MS), Some(true));
    }

    #[test]
    fn a_normal_sample_breaks_the_jump_run() {
        let (mut est, mut t) = synced_estimator(0);
        for _ in 0..RESYNC_RUN - 1 {
            est.push(exchange(t, 5 * S as i64, MS, MS, 0));
            t += S;
        }
        assert_eq!(est.push(exchange(t, 0, MS, MS, 0)), EstimatorEvent::Updated);
        t += S;
        assert_eq!(est.push(exchange(t, 5 * S as i64, MS, MS, 0)), EstimatorEvent::Rejected);
        assert_eq!(est.resyncs(), 0);
    }

    #[test]
    fn disagreeing_jumps_do_not_resync() {
        let (mut est, mut t) = synced_estimator(0);
        for i in 0..RESYNC_RUN * 2 {
            let offset = if i % 2 == 0 { 5 * S as i64 } else { -5 * S as i64 };
            assert_eq!(est.push(exchange(t, offset, MS, MS, 0)), EstimatorEvent::Rejected);
            t += S;
        }
        assert_eq!(est.resyncs(), 0);
    }

    #[test]
    fn clock_never_repeats() {
        let mut clock = MonotonicClock::default();
        let a = clock.now(1_000);
        let b = clock.now(1_000);
        let c = clock.now(999);
        assert!(a < b && b < c);
    }

    #[test]
    fn forward_correction_applies_immediately() {
        let mut clock = MonotonicClock::default();
        clock.now(1_000);
        clock.set_target(5 * S as i64);
        assert_eq!(clock.now(1_001), 1_001 + 5 * S);
    }

    #[test]
    fn backward_correction_is_absorbed_gradually() {
        let mut clock = MonotonicClock::default();
        clock.set_target(S as i64);
        let mut raw = 10 * S;
        let mut last = clock.now(raw);
        clock.set_target(0);

        let step = 10 * MS;
        let mut elapsed = 0;
        while clock.applied_offset_us() > 0 {
            raw += step;
            elapsed += step;
            let out = clock.now(raw);
            assert!(out - last >= step * 19 / 20, "clock ran slower than 95%");
            last = out;
            assert!(elapsed <= 21 * S, "took too long to absorb");
        }
        assert!(elapsed >= 19 * S, "absorbed faster than the slew rate allows");
        raw += step;
        assert_eq!(clock.now(raw), raw);
    }

    #[test]
    fn reference_clock_starts_synced() {
        let robot = SyncedClock::new(true, 0);
        assert!(robot.metrics().synced);
        assert_eq!(robot.metrics().uncertainty_us, Some(0));
        let operator = SyncedClock::new(false, 0);
        assert!(!operator.metrics().synced);
        assert_eq!(operator.metrics().uncertainty_us, None);
    }

    #[test]
    fn new_robot_restarts_estimate() {
        let clock = SyncedClock::new(false, 0);
        for i in 0..MIN_SAMPLES_TO_SYNC as u64 {
            clock.record("robot-a", exchange(i * 250 * MS, 2 * S as i64, MS, MS, 0));
        }
        assert!(clock.is_synced());
        let event = clock.record("robot-b", exchange(2 * S, 7 * S as i64, MS, MS, 0));
        assert_eq!(event, EstimatorEvent::Pending);
        assert!(!clock.is_synced());
    }
}
