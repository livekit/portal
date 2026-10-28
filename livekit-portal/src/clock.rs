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

//! Time sync IO. The math lives in `portal_core::clock`; this module adds
//! the local time sources and the actor that pings and answers:
//!   * `SyncedClock` — the shared, locked handle `Portal` reads
//!   * `ClockService` — the actor that pings, answers and feeds the estimator

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use livekit::prelude::*;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub(crate) use portal_core::clock::{
    CLOCK_TOPIC, ClockPacket, EstimatorEvent, Exchange, MonotonicClock, OffsetEstimator, Sample,
};
use portal_core::metrics::{MetricsRegistry, TimeSyncMetrics};
use portal_core::types::{Role, TimeSyncSource};

use crate::portal::ControllerState;

const UNSYNCED_PING_INTERVAL: Duration = Duration::from_millis(250);
const SYNCED_PING_INTERVAL: Duration = Duration::from_secs(1);
const CLOCK_QUEUE_CAP: usize = 64;

/// Local time for the clock in `portal` mode: wall time at first use,
/// advanced by a monotonic `Instant` so host clock steps (NTP) never reach
/// Portal timestamps. Also the arrival clock for jitter, which must not
/// step when a sync correction does.
pub(crate) fn anchored_now_us() -> u64 {
    static ANCHOR: OnceLock<(Instant, u64)> = OnceLock::new();
    let (instant, wall_us) = ANCHOR.get_or_init(|| (Instant::now(), system_now_us()));
    wall_us + instant.elapsed().as_micros() as u64
}

/// Local time in `system` mode: the host clock as is, which a lab trusts
/// because PTP or GPS already keeps it in step with the other hosts.
fn system_now_us() -> u64 {
    let wall = SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before 1970");
    wall.as_micros() as u64
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

pub(crate) struct ClockOptions {
    /// True on the robot: its own clock is the timeline, so it is synced
    /// from the start.
    pub is_reference: bool,
    pub source: TimeSyncSource,
    /// Shifts the raw clock, for tests that simulate hosts whose clocks
    /// disagree.
    pub skew_us: i64,
}

/// The clock a `Portal` reads. Outlives connections so `now_us()` stays
/// continuous across a reconnect.
pub(crate) struct SyncedClock {
    options: ClockOptions,
    state: Mutex<ClockState>,
    on_synced: Mutex<Option<TimeSyncedCb>>,
}

impl SyncedClock {
    pub fn new(options: ClockOptions) -> Self {
        let estimator = match options.is_reference {
            true => OffsetEstimator::reference(),
            false => OffsetEstimator::default(),
        };
        Self {
            options,
            state: Mutex::new(ClockState {
                clock: MonotonicClock::default(),
                estimator,
                robot_identity: None,
            }),
            on_synced: Mutex::new(None),
        }
    }

    pub fn now_us(&self) -> u64 {
        let raw = self.raw_us();
        self.state.lock().clock.now(raw)
    }

    /// See `MonotonicClock::estimate`.
    pub fn estimate_us(&self) -> u64 {
        let raw = self.raw_us();
        self.state.lock().clock.estimate(raw)
    }

    /// Raw local time, the clock the ping/pong timestamps are taken on.
    fn raw_us(&self) -> u64 {
        let raw = match self.options.source {
            TimeSyncSource::Portal => anchored_now_us(),
            TimeSyncSource::System => system_now_us(),
        };
        raw.saturating_add_signed(self.options.skew_us)
    }

    /// Whether the exchange has produced an estimate yet, applied or not.
    /// Drives the ping rate.
    fn has_estimate(&self) -> bool {
        self.state.lock().estimator.is_synced()
    }

    pub fn set_on_synced(&self, cb: TimeSyncedCb) {
        *self.on_synced.lock() = Some(cb);
    }

    pub fn metrics(&self) -> TimeSyncMetrics {
        let state = self.state.lock();
        let trusted = self.options.is_reference || self.options.source == TimeSyncSource::System;
        TimeSyncMetrics {
            source: self.options.source,
            synced: trusted || state.estimator.is_synced(),
            offset_us: state.clock.applied_offset_us(),
            uncertainty_us: match (self.options.is_reference, self.options.source) {
                (true, _) => Some(0),
                // Nothing is applied, so Portal can't bound the host clock's error.
                (false, TimeSyncSource::System) => None,
                (false, TimeSyncSource::Portal) => state.estimator.uncertainty_us(),
            },
            measured_offset_us: match self.options.is_reference {
                true => None,
                false => state.estimator.offset_us(),
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
        // In system mode the estimate is only measured, never applied.
        if self.options.source == TimeSyncSource::Portal
            && let Some(offset) = state.estimator.offset_us()
        {
            state.clock.set_target(offset);
        }
        event
    }

    fn fire_synced(&self) {
        if self.options.source == TimeSyncSource::System {
            return;
        }
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
                    let interval = match self.clock.has_estimate() {
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
    use portal_core::clock::MIN_SAMPLES_TO_SYNC;

    const MS: u64 = 1_000;
    const S: u64 = 1_000_000;

    fn exchange(t1: u64, offset: i64, up: u64, down: u64, hold: u64) -> Sample {
        let t2 = (t1 + up).saturating_add_signed(offset);
        let t3 = t2 + hold;
        let t4 = t1 + up + hold + down;
        Sample::try_from(Exchange { t1, t2, t3, t4 }).expect("consistent exchange")
    }

    #[test]
    fn reference_clock_starts_synced() {
        let robot = SyncedClock::new(options(true, TimeSyncSource::Portal));
        assert!(robot.metrics().synced);
        assert_eq!(robot.metrics().uncertainty_us, Some(0));
        let operator = SyncedClock::new(options(false, TimeSyncSource::Portal));
        assert!(!operator.metrics().synced);
        assert_eq!(operator.metrics().uncertainty_us, None);
    }

    #[test]
    fn new_robot_restarts_estimate() {
        let clock = SyncedClock::new(options(false, TimeSyncSource::Portal));
        for i in 0..MIN_SAMPLES_TO_SYNC as u64 {
            clock.record("robot-a", exchange(i * 250 * MS, 2 * S as i64, MS, MS, 0));
        }
        assert!(clock.has_estimate());
        let event = clock.record("robot-b", exchange(2 * S, 7 * S as i64, MS, MS, 0));
        assert_eq!(event, EstimatorEvent::Pending);
        assert!(!clock.has_estimate());
    }

    #[test]
    fn system_mode_measures_but_does_not_apply() {
        let clock = SyncedClock::new(options(false, TimeSyncSource::System));
        let m = clock.metrics();
        assert!(m.synced, "system mode trusts the host clock from the start");
        assert_eq!((m.uncertainty_us, m.measured_offset_us), (None, None));

        for i in 0..MIN_SAMPLES_TO_SYNC as u64 {
            clock.record("robot", exchange(i * 250 * MS, 3 * S as i64, MS, MS, 0));
        }
        let m = clock.metrics();
        assert_eq!(m.offset_us, 0);
        assert_eq!(m.measured_offset_us, Some(3 * S as i64));

        let before = system_now_us();
        let now = clock.now_us();
        assert!(now.abs_diff(before) < S, "system mode reads the host clock unshifted");
    }

    #[test]
    fn portal_mode_applies_the_estimate() {
        let clock = SyncedClock::new(options(false, TimeSyncSource::Portal));
        for i in 0..MIN_SAMPLES_TO_SYNC as u64 {
            clock.record("robot", exchange(i * 250 * MS, 3 * S as i64, MS, MS, 0));
        }
        let m = clock.metrics();
        assert_eq!((m.offset_us, m.measured_offset_us), (3 * S as i64, Some(3 * S as i64)));
    }

    fn options(is_reference: bool, source: TimeSyncSource) -> ClockOptions {
        ClockOptions { is_reference, source, skew_us: 0 }
    }
}
