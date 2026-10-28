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

//! Clock-sync math, free of IO: the `portal_clock` wire codec, the offset
//! estimator and the monotonic clock. Every peer other than the robot
//! estimates the robot's clock from an NTP-style ping/pong, and reads it
//! through a clock that never repeats and, after the first sync, never goes
//! backwards.
//!
//!   * `ClockPacket` — wire codec
//!   * `OffsetEstimator` — samples in, offset out
//!   * `MonotonicClock` — raw time + offset in, timestamps out

use std::collections::VecDeque;

pub const CLOCK_TOPIC: &str = "portal_clock";

const PING_KIND: u8 = 0;
const PONG_KIND: u8 = 1;
const PING_LEN: usize = 1 + 4 + 8;
const PONG_LEN: usize = 1 + 4 + 8 + 8 + 8;

const WINDOW_US: u64 = 10_000_000;
/// Samples needed before the first estimate is trusted: one second at the
/// unsynced rate, enough for the min-rtt pick to skip an unlucky first sample.
pub const MIN_SAMPLES_TO_SYNC: usize = 4;
const JUMP_THRESHOLD_US: u64 = 1_000_000;
const RESYNC_RUN: usize = 30;
/// Jump candidates within this distance of each other count as agreeing.
const JUMP_AGREEMENT_US: u64 = 100_000;
/// A backward correction removes at most `elapsed / SLEW_DIVISOR` per step,
/// so the clock keeps running at no less than 95% of real speed.
const SLEW_DIVISOR: u64 = 20;
/// Further behind than this (a resync, or a restarted robot), the clock runs
/// at half speed instead, so a 10 s step back takes ~22 s to absorb, not ~200 s.
const FAST_SLEW_ABOVE_US: u64 = 100_000;
const FAST_SLEW_DIVISOR: u64 = 2;

// --- Wire ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockPacket {
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
pub struct Sample {
    pub offset_us: i64,
    pub rtt_us: u64,
    /// Local receive time (`t4`), used to age samples out of the window.
    pub at_us: u64,
}

/// Timestamps of one ping/pong exchange: `t1` ping sent (local), `t2` ping
/// received (robot), `t3` pong sent (robot), `t4` pong received (local).
#[derive(Debug, Clone, Copy)]
pub struct Exchange {
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
pub enum EstimatorEvent {
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
pub struct OffsetEstimator {
    window: VecDeque<Sample>,
    synced: bool,
    /// Consecutive samples that each implied a jump and agree with each other.
    jump_run: Vec<Sample>,
    resyncs: u64,
    samples_rejected: u64,
}

impl OffsetEstimator {
    /// An estimator for the reference clock, synced from the start.
    pub fn reference() -> Self {
        Self { synced: true, ..Default::default() }
    }

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
/// that strictly increase once synced. Forward corrections apply at once;
/// backward ones are absorbed gradually so time never runs backwards or
/// stands still.
#[derive(Debug, Default)]
pub struct MonotonicClock {
    applied_offset_us: i64,
    target_offset_us: i64,
    last_raw_us: Option<u64>,
    /// Last value `now` handed out, `None` until the first one.
    last_out_us: Option<u64>,
    /// Whether a target has been set, i.e. the first sync has happened.
    synced: bool,
}

impl MonotonicClock {
    /// Applies a forward correction at once and slews a backward one. The
    /// first target applies at once either way: until then the clock is local
    /// time, a different timeline, so it switches over in one step rather
    /// than stamping ahead of the robot while it slews.
    pub fn set_target(&mut self, offset_us: i64) {
        self.target_offset_us = offset_us;
        if !self.synced {
            self.synced = true;
            self.applied_offset_us = offset_us;
            self.last_out_us = None;
        } else if offset_us > self.applied_offset_us {
            self.applied_offset_us = offset_us;
        }
    }

    /// A timestamp to hand out: never repeats, and never goes backwards
    /// except once, at the first sync.
    pub fn now(&mut self, raw_us: u64) -> u64 {
        let candidate = self.estimate(raw_us);
        let out = self.last_out_us.map_or(candidate, |last| candidate.max(last + 1));
        self.last_out_us = Some(out);
        out
    }

    /// Current best reading of the robot's clock, for local measurements
    /// such as receive times. Unlike `now`, it isn't bumped to stay unique.
    pub fn estimate(&mut self, raw_us: u64) -> u64 {
        let elapsed = self.last_raw_us.map_or(0, |last| raw_us.saturating_sub(last));
        self.last_raw_us = Some(raw_us);

        let behind = self.applied_offset_us - self.target_offset_us;
        if behind > 0 {
            let divisor = match behind as u64 > FAST_SLEW_ABOVE_US {
                true => FAST_SLEW_DIVISOR,
                false => SLEW_DIVISOR,
            };
            let step = (elapsed / divisor).min(behind as u64) as i64;
            self.applied_offset_us -= step;
        }
        raw_us.saturating_add_signed(self.applied_offset_us)
    }

    /// Offset currently in effect, which lags the target while slewing back.
    pub fn applied_offset_us(&self) -> i64 {
        self.applied_offset_us
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
    fn first_backward_correction_applies_immediately() {
        let mut clock = MonotonicClock::default();
        clock.set_target(-5 * S as i64);
        assert_eq!(clock.now(10 * S), 5 * S);
    }

    #[test]
    fn first_correction_applies_immediately_after_timestamps_were_handed_out() {
        let mut clock = MonotonicClock::default();
        clock.estimate(10 * S);
        clock.now(10 * S);
        clock.set_target(-5 * S as i64);
        assert_eq!(clock.now(10 * S + 1), 5 * S + 1);
    }

    #[test]
    fn later_backward_correction_is_slewed() {
        let mut clock = MonotonicClock::default();
        clock.set_target(0);
        let before = clock.now(10 * S);
        clock.set_target(-5 * S as i64);
        assert!(clock.now(10 * S + 1) > before);
        assert_eq!(clock.applied_offset_us(), 0);
    }

    /// Slews from `behind` to zero in 10 ms steps, checking the clock never
    /// runs slower than `min_speed_pct`. Returns the time it took.
    fn absorb(behind: u64, min_speed_pct: u64) -> u64 {
        let mut clock = MonotonicClock::default();
        clock.set_target(behind as i64);
        let mut raw = 100 * S;
        let mut last = clock.now(raw);
        clock.set_target(0);

        let step = 10 * MS;
        let mut elapsed = 0;
        while clock.applied_offset_us() > 0 {
            raw += step;
            elapsed += step;
            let out = clock.now(raw);
            assert!(out - last >= step * min_speed_pct / 100, "clock ran too slow");
            last = out;
            assert!(elapsed <= 60 * S, "never caught up");
        }
        raw += step;
        assert_eq!(clock.now(raw), raw);
        elapsed
    }

    #[test]
    fn small_backward_correction_is_absorbed_gradually() {
        let elapsed = absorb(50 * MS, 95);
        assert!((950 * MS..=1_050 * MS).contains(&elapsed), "{elapsed}");
    }

    #[test]
    fn large_backward_correction_is_absorbed_at_half_speed() {
        // ~19.8 s at half speed down to 100 ms, then ~2 s at 95%.
        let elapsed = absorb(10 * S, 50);
        assert!((21 * S..=23 * S).contains(&elapsed), "{elapsed}");
    }
}
