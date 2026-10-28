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

//! Local time sources. The sync math lives in `livekit_portal_core::clock`; the core
//! is handed raw local time from here on every step.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use livekit_portal_core::types::TimeSyncSource;

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

/// The raw local clock a Portal feeds its core: the ping/pong timestamps
/// and every step's `now_us` are taken on it.
#[derive(Clone, Copy)]
pub(crate) struct RawClock {
    pub source: TimeSyncSource,
    /// Shifts the raw clock, for tests that simulate hosts whose clocks
    /// disagree.
    pub skew_us: i64,
}

impl RawClock {
    pub fn now_us(self) -> u64 {
        let raw = match self.source {
            TimeSyncSource::Portal => anchored_now_us(),
            TimeSyncSource::System => system_now_us(),
        };
        raw.saturating_add_signed(self.skew_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    #[test]
    fn system_mode_reads_the_host_clock_unshifted() {
        let clock = RawClock { source: TimeSyncSource::System, skew_us: 0 };
        assert!(clock.now_us().abs_diff(system_now_us()) < S);
    }

    #[test]
    fn skew_shifts_the_raw_clock() {
        let skewed = RawClock { source: TimeSyncSource::Portal, skew_us: 5 * S as i64 };
        let plain = RawClock { source: TimeSyncSource::Portal, skew_us: 0 };
        let diff = skewed.now_us() as i64 - plain.now_us() as i64;
        assert!((diff - 5 * S as i64).abs() < S as i64);
    }

    #[test]
    fn portal_mode_never_goes_backwards() {
        let a = anchored_now_us();
        let b = anchored_now_us();
        assert!(b >= a);
    }
}
