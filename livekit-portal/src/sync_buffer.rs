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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::config::FieldSpec;
use crate::metrics::MetricsRegistry;
use crate::placeholder;
use crate::types::*;

#[cfg(test)]
use crate::dtype::DType;

/// Minimum gap between sync-drop warnings during a sustained burst. The
/// first drop in a burst logs immediately; further drops fold into a single
/// summary emitted at most this often. Every drop is still counted in the
/// `states_dropped` metric regardless of logging.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// Rate-limiter state for the unsyncable-state drop warning. Wall-clock is
/// used **only** to throttle the log line so a stalled video track doesn't
/// spam at the state-publish rate. It never influences a sync decision —
/// match/wait/drop is still computed purely from sender timestamps (see the
/// module docs and `docs/synchronization.md`).
#[derive(Default)]
struct DropWarn {
    // Drops counted since the last emitted warning.
    count: u64,
    // Largest "video is ahead of the dropped state" gap since the last
    // warning, in microseconds. `newest_frame_ts - state_ts` at drop time.
    worst_ahead_us: u64,
    // When the last warning was emitted. `None` until the first drop.
    last_log: Option<Instant>,
}

/// Result of a `push_frame` / `push_state` call. Callers dispatch these
/// (invoke callbacks, enqueue into the pull-based buffer) *after* releasing
/// the SyncBuffer lock so slow consumers don't stall the hot path.
pub(crate) struct SyncOutput {
    pub observations: Vec<Observation>,
    /// State samples that could not be matched to a video frame. Typed
    /// per the declared state schema, same shape as `Observation.state`.
    /// Populated by sync-fail and state-buffer overflow. Under the default
    /// strict policy, sync-fail fires whenever the video horizon advances
    /// past a state with no in-range match. Under `reuse_stale_frames`,
    /// sync-fail only fires before a track has emitted its first frame;
    /// after that, states fall back to the last-emitted frame rather than
    /// dropping.
    pub drops: Vec<HashMap<String, TypedValue>>,
}

impl SyncOutput {
    pub fn empty() -> Self {
        Self { observations: Vec::new(), drops: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty() && self.drops.is_empty()
    }
}

/// Per-track outcome of one `try_sync` iteration. `drain_to` controls buffer
/// drainage and `last_emitted_frames` advancement; `stale` drives the
/// `stale_observations_emitted` metric. The two are independent: a
/// below-horizon frame is both drained (so it can't clog the buffer) and
/// stale (the state is outside its tolerance).
struct MatchSlot {
    frame: Arc<VideoFrameData>,
    // If `Some(idx)`, drain `video_buffers[track][0..=idx]` on emit and
    // set `last_emitted_frames[track] = frame`. If `None`, the frame is
    // the stored last-emitted fallback — buffer and pointer stay put.
    drain_to: Option<usize>,
    // How the frame relates to the state it is being attached to. `Live`
    // for an in-range match; `Stale` for either reuse path (pure reuse or a
    // below-horizon drain-match). Stamped onto the emitted frame so
    // consumers can tell a measurement from a substitute.
    source: FrameSource,
}

pub(crate) struct SyncBuffer {
    track_names: Vec<String>,
    track_index: HashMap<String, usize>,
    // Parallel to `track_names`; indexed by track position.
    video_buffers: Vec<VecDeque<Arc<VideoFrameData>>>,
    state_buffer: VecDeque<(u64, Vec<f64>)>, // (timestamp_us, values)
    /// State schema — field names and their declared dtypes. Used to
    /// reconstruct typed values into each `Observation` emitted.
    state_schema: Vec<FieldSpec>,
    config: SyncConfig,

    // Per-track cached placeholder pixels for `on_stall: omit`, with the
    // geometry they were rendered for. Rebuilt only when a track's frame
    // size changes, so a silent track costs one render rather than one per
    // observation — the buffer is `W*H*3` and would otherwise be re-synthesized
    // at the state rate for as long as the track stays down.
    placeholders: Vec<Option<(u32, u32, Bytes)>>,

    // Per-track stall handling, parallel to `track_names`: how long to wait
    // for a silent track and how to resolve the moment when the wait ends.
    stall: Vec<StallConfig>,

    // True iff any track has a finite `max_lag_us`. Lets the hot path skip
    // computing `logical_now` entirely under the default config.
    any_max_lag: bool,

    // Per-track cursor: the largest index whose frame ts is <= head state ts
    // (or 0 if all frames are > head ts). Advances monotonically with state_ts
    // so sync work amortizes to O(N+M) across the stream.
    cursors: Vec<usize>,

    // The track that caused the last try_sync attempt to wait. `None` means
    // "unknown — run try_sync on the next push." Used to skip sync attempts
    // on pushes to tracks that cannot change head-state matchability.
    blocker: Option<usize>,

    // Reused across try_sync calls to avoid allocating a match map per iteration.
    matched_scratch: Vec<Option<MatchSlot>>,

    // Per-track: the most recent frame emitted in an observation. Used as a
    // stale fallback when the current state has no in-range match, so no
    // observation is ever dropped due to missing video — the video "freezes"
    // on the last good frame instead. None until the track emits its first
    // frame.
    last_emitted_frames: Vec<Option<Arc<VideoFrameData>>>,

    // True when the previous push_state hit state-buffer overflow. Used to
    // suppress the warn log on consecutive overflows so a sustained halt
    // logs once per burst instead of once per state tick. Metrics still
    // count every drop.
    in_overflow_burst: bool,

    // Rate-limiter for the unsyncable-state drop warning. Logging only —
    // see `DropWarn`.
    drop_warn: DropWarn,

    metrics: Arc<MetricsRegistry>,
}

impl SyncBuffer {
    /// `stall` is the per-track stall configuration, parallel to
    /// `video_track_names`. A length mismatch (or an empty vec) falls back to
    /// `config.default_stall` for every track, so test and embedding callers
    /// that do not care can pass `Vec::new()`.
    pub fn new(
        video_track_names: &[String],
        state_schema: Vec<FieldSpec>,
        config: SyncConfig,
        stall: Vec<StallConfig>,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        let track_names: Vec<String> = video_track_names.to_vec();
        let track_index: HashMap<String, usize> =
            track_names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();
        let video_buffers = (0..track_names.len()).map(|_| VecDeque::new()).collect();
        let cursors = vec![0; track_names.len()];
        let matched_scratch: Vec<Option<MatchSlot>> =
            (0..track_names.len()).map(|_| None).collect();
        let last_emitted_frames = vec![None; track_names.len()];
        let stall = if stall.len() == track_names.len() {
            stall
        } else {
            vec![config.default_stall; track_names.len()]
        };
        let any_max_lag = stall.iter().any(|s| s.max_lag_us.is_some());
        let placeholders = vec![None; track_names.len()];
        Self {
            track_names,
            track_index,
            video_buffers,
            state_buffer: VecDeque::new(),
            state_schema,
            config,
            placeholders,
            stall,
            any_max_lag,
            cursors,
            blocker: None,
            matched_scratch,
            last_emitted_frames,
            in_overflow_burst: false,
            drop_warn: DropWarn::default(),
            metrics,
        }
    }

    /// Count one unsyncable-state drop and emit a throttled, self-explanatory
    /// warning. `ahead_us` is how far the video stream had moved past the
    /// dropped state (`newest_frame_ts - state_ts`), which is why no match
    /// was possible. The first drop in a burst logs immediately; subsequent
    /// drops fold into a summary emitted at most once per `DROP_WARN_INTERVAL`.
    fn note_unsyncable_drop(&mut self, ahead_us: u64) {
        self.drop_warn.count += 1;
        self.drop_warn.worst_ahead_us = self.drop_warn.worst_ahead_us.max(ahead_us);

        let now = Instant::now();
        let elapsed = self.drop_warn.last_log.map(|t| now.duration_since(t));
        let should_log = match elapsed {
            None => true,
            Some(d) => d >= DROP_WARN_INTERVAL,
        };
        if !should_log {
            return;
        }

        let range_ms = self.config.search_range_us as f64 / 1_000.0;
        let ahead_ms = self.drop_warn.worst_ahead_us as f64 / 1_000.0;
        match elapsed {
            // First drop in a burst. Cause and fix live at docs/logging.md#sync-drop.
            None => log::warn!(
                "[sync-drop] dropping states: no frame within ±{range_ms:.0}ms of \
                 the state timestamp (video {ahead_ms:.0}ms ahead). Throttling \
                 further [sync-drop] warnings to once per {}s.",
                DROP_WARN_INTERVAL.as_secs(),
            ),
            // Sustained burst: one rolled-up summary per interval.
            Some(d) => log::warn!(
                "[sync-drop] dropped {} more states in {:.0}s: no frame within \
                 ±{range_ms:.0}ms (video up to {ahead_ms:.0}ms ahead).",
                self.drop_warn.count,
                d.as_secs_f64(),
            ),
        }
        self.drop_warn.last_log = Some(now);
        self.drop_warn.count = 0;
        self.drop_warn.worst_ahead_us = 0;
    }

    /// Largest sender timestamp currently buffered across every stream — the
    /// newest state plus each track's newest frame. This is the "stream
    /// clock" `max_lag` is measured against: it advances whenever *any*
    /// stream is still flowing, so a track that has gone silent is detected
    /// through the clocks of the tracks that have not.
    ///
    /// Reads only sender timestamps, never a wall clock, so stall decisions
    /// stay reproducible and testable — the reason a wall-clock deadline was
    /// rejected in the first place. The corollary is that this is not a
    /// watchdog: if every stream goes silent the clock stops and nothing
    /// resolves, which is harmless because nothing is being emitted either.
    ///
    /// Always `>= state_buffer[0]`, since the head is the front of
    /// `state_buffer` and its back is `>=` its front. O(tracks), and only
    /// called when at least one track has a finite `max_lag`.
    fn logical_now(&self) -> u64 {
        let mut now = self.state_buffer.back().map(|(ts, _)| *ts).unwrap_or(0);
        for buf in &self.video_buffers {
            if let Some(frame) = buf.back() {
                now = now.max(frame.timestamp_us);
            }
        }
        now
    }

    /// Cached placeholder pixels for one track at one geometry.
    ///
    /// Free function over the cache slot rather than a `&mut self` method so
    /// it can be called while the track's frame buffer is borrowed. Rendering
    /// is deterministic, so caching on `(width, height)` is sound; a track
    /// that renegotiates resolution simply re-renders once.
    fn placeholder_pixels(
        slot: &mut Option<(u32, u32, Bytes)>,
        track_name: &str,
        width: u32,
        height: u32,
    ) -> Bytes {
        if let Some((w, h, data)) = slot
            && *w == width
            && *h == height
        {
            return data.clone();
        }
        let data = placeholder::render(width, height, track_name);
        *slot = Some((width, height, data.clone()));
        data
    }

    /// Build the typed state map once per emission. Separate so the two
    /// call sites (overflow drop and sync emit) stay in lockstep, and
    /// distinctly named to avoid shadowing the conceptual "typed state"
    /// field on `Observation`.
    fn build_typed_state_map(&self, values: &[f64]) -> HashMap<String, TypedValue> {
        to_value_maps(&self.state_schema, values).0
    }

    pub fn push_frame(&mut self, track_name: &str, frame: Arc<VideoFrameData>) -> SyncOutput {
        let idx = match self.track_index.get(track_name) {
            Some(&i) => i,
            None => return SyncOutput::empty(),
        };

        let cap = self.config.video_buffer_size as usize;
        let buf = &mut self.video_buffers[idx];
        buf.push_back(frame);

        let mut evicted = 0usize;
        while buf.len() > cap {
            buf.pop_front();
            evicted += 1;
        }
        if evicted > 0 {
            let cursor = &mut self.cursors[idx];
            *cursor = cursor.saturating_sub(evicted);
            if let Some(tm) = self.metrics.track(track_name) {
                tm.record_evictions(evicted as u64);
            }
            log::warn!("[video-overflow] '{track_name}' buffer full, evicted {evicted} frame(s)");
        }

        // Skip try_sync when this push cannot have changed head-state matchability:
        //   - another track is blocking (a push to a non-blocker doesn't unblock it), AND
        //   - no eviction happened on this track (eviction can newly-transition a track
        //     from matching → unmatchable, which must be checked), AND
        //   - no track has a finite `max_lag`.
        //
        // That last clause is what makes a stall resolve when a healthy track
        // is the only thing still moving: a push to a *non-blocking* track
        // advances the stream clock, which can carry the head past its lag
        // budget even though this track was never the blocker. Skipping here
        // would strand the head exactly in the case the budget exists to fix.
        // It costs the blocker short-circuit whenever a budget is set, which
        // is an O(tracks) early-return per frame — cheap next to decoding one.
        let should_run = match self.blocker {
            None => true,
            Some(b) if b == idx => true,
            Some(_) => evicted > 0 || self.any_max_lag,
        };

        if should_run { self.try_sync() } else { SyncOutput::empty() }
    }

    pub fn push_state(&mut self, timestamp_us: u64, values: Vec<f64>) -> SyncOutput {
        let old_head_ts = self.state_buffer.front().map(|(ts, _)| *ts);
        self.state_buffer.push_back((timestamp_us, values));

        let mut overflow_drops: Vec<HashMap<String, TypedValue>> = Vec::new();
        while self.state_buffer.len() > self.config.state_buffer_size as usize {
            let (_, vals) = self.state_buffer.pop_front().unwrap();
            overflow_drops.push(self.build_typed_state_map(&vals));
        }
        if !overflow_drops.is_empty() {
            self.metrics.record_state_dropped(overflow_drops.len() as u64);
            // Log once per overflow burst so a sustained halt doesn't spam
            // at the state tick rate. The `states_dropped` metric still
            // reflects every drop.
            if !self.in_overflow_burst {
                log::warn!(
                    "[state-overflow] state buffer full ({}), dropped {} oldest. \
                     Further drops in this burst won't be re-logged.",
                    self.config.state_buffer_size,
                    overflow_drops.len(),
                );
                self.in_overflow_burst = true;
            }
        } else {
            self.in_overflow_burst = false;
        }
        // If eviction (or first-ever push) changed the head state, the old blocker
        // hint no longer applies.
        let new_head_ts = self.state_buffer.front().map(|(ts, _)| *ts);
        if new_head_ts != old_head_ts {
            self.blocker = None;
        }

        let mut output = self.try_sync();
        if !overflow_drops.is_empty() {
            // Overflow drops precede any sync-fail drops temporally.
            overflow_drops.append(&mut output.drops);
            output.drops = overflow_drops;
        }
        output
    }

    pub fn clear(&mut self) {
        for buf in &mut self.video_buffers {
            buf.clear();
        }
        self.state_buffer.clear();
        for c in &mut self.cursors {
            *c = 0;
        }
        for slot in &mut self.last_emitted_frames {
            *slot = None;
        }
        for slot in &mut self.placeholders {
            *slot = None;
        }
        self.blocker = None;
        self.in_overflow_burst = false;
        self.drop_warn = DropWarn::default();
    }

    fn try_sync(&mut self) -> SyncOutput {
        let mut output = SyncOutput::empty();
        let range = self.config.search_range_us;

        loop {
            if self.state_buffer.is_empty() {
                self.blocker = None;
                return output;
            }

            let state_ts = self.state_buffer[0].0;
            // Next state in the buffer (if any) — used for fair-share: if a
            // candidate frame is closer to the next state than to the head
            // state, we skip it so the later state can claim it.
            let next_state_ts = self.state_buffer.get(1).map(|(ts, _)| *ts);

            for slot in &mut self.matched_scratch {
                *slot = None;
            }

            // Stream clock for `max_lag`, computed once per candidate state
            // rather than per track. Skipped entirely when no track has a
            // finite `max_lag`, which is the default.
            let logical_now = if self.any_max_lag { self.logical_now() } else { 0 };

            // Per-iteration status. Priority: drop > wait > emit. We scan every
            // track (even after a wait-on-earlier-track) so that a drop-eligible
            // track later in the list can override the wait — otherwise a state
            // could stall forever waiting on cam1 while cam2 has already moved
            // beyond the match horizon.
            //
            // A track that cannot match waits until either no future frame
            // could match it or its `max_lag` elapses, then resolves per its
            // `on_stall` policy. `Freeze` and `Omit` still wait through the
            // startup window before the track's first frame, when they have
            // nothing to substitute. State-buffer overflow (handled in
            // `push_state`) remains the hard safety net against a fully
            // halted video stream.
            let mut should_drop = false;
            // How far the video stream had moved past `state_ts` when the drop
            // fired (`newest_frame_ts - state_ts`). Captured for the warning so
            // it can report why no match was possible.
            let mut drop_ahead_us = 0u64;
            let mut iter_blocker: Option<usize> = None;

            for track_i in 0..self.video_buffers.len() {
                let frame_buf = &self.video_buffers[track_i];

                // Cursor maintenance / fresh match search. Guarded on buffer
                // being non-empty; if empty we fall through to the reuse /
                // wait branches below.
                let mut best_idx: Option<usize> = None;
                if !frame_buf.is_empty() {
                    let cursor = &mut self.cursors[track_i];
                    // Defensive clamp in case capacity shrunk or mutation missed an adjustment.
                    if *cursor >= frame_buf.len() {
                        *cursor = frame_buf.len() - 1;
                    }
                    // Rewind if the cursor is already past state_ts (can happen if
                    // states arrive out of order on unreliable delivery).
                    while *cursor > 0 && frame_buf[*cursor].timestamp_us > state_ts {
                        *cursor -= 1;
                    }
                    // Advance while the next frame is still at or before state_ts.
                    while *cursor + 1 < frame_buf.len()
                        && frame_buf[*cursor + 1].timestamp_us <= state_ts
                    {
                        *cursor += 1;
                    }

                    let cursor_val = *cursor;
                    let mut best_delta = u64::MAX;
                    for candidate in
                        [Some(cursor_val), cursor_val.checked_add(1)].into_iter().flatten()
                    {
                        if let Some(f) = frame_buf.get(candidate) {
                            let d = state_ts.abs_diff(f.timestamp_us);
                            if d >= range || d >= best_delta {
                                continue;
                            }
                            // Fair-share: if a later buffered state has a strictly
                            // closer claim, leave the frame for it. Prevents a
                            // greedy head-state from stealing its neighbor's frame
                            // when tolerance > 1 tick.
                            if let Some(nts) = next_state_ts
                                && nts.abs_diff(f.timestamp_us) < d
                            {
                                continue;
                            }
                            best_delta = d;
                            best_idx = Some(candidate);
                        }
                    }
                }

                if let Some(idx) = best_idx {
                    self.matched_scratch[track_i] = Some(MatchSlot {
                        frame: self.video_buffers[track_i][idx].clone(),
                        drain_to: Some(idx),
                        source: FrameSource::Live,
                    });
                    continue;
                }

                // No fresh in-range match on this track. Two questions, in
                // order: can waiting still help, and — if not, or if we have
                // already waited long enough — how does this track's policy
                // resolve the moment?
                //
                // Waiting cannot help once the newest buffered frame is
                // already past the horizon: frame timestamps are monotonic,
                // so every future frame is at least that new and none of them
                // can match. (Testing the newest rather than the front
                // detects this immediately, instead of only after eviction
                // drags the tail past the horizon — a latency bug of up to
                // `video_buffer_size` frames.) `>=` matches the strict
                // `d < range` match rule: a frame at exactly `state_ts +
                // range` is not a match, so the state cannot match it either.
                let newest_ts = frame_buf.back().map(|f| f.timestamp_us);
                let unmatchable = newest_ts.is_some_and(|n| n >= state_ts.saturating_add(range));

                // Otherwise a future frame could still land in range, so we
                // wait — but only until the fastest-advancing stream's sender
                // clock has run `max_lag` past this moment. `None` keeps the
                // historical behavior of waiting until state-buffer capacity
                // evicts the moment (or forever, if state output has stopped
                // too). See `logical_now`.
                let stall = self.stall[track_i];
                let lagged =
                    stall.max_lag_us.is_some_and(|max| logical_now.saturating_sub(state_ts) >= max);

                if !unmatchable && !lagged {
                    if iter_blocker.is_none() {
                        iter_blocker = Some(track_i);
                    }
                    continue;
                }

                // Resolve without this track.
                match stall.policy {
                    // Prefer the newest below-horizon buffered frame
                    // (ts ≤ state_ts, |Δ| ≥ range) over the stored
                    // last-emitted fallback: it tracks forward with state_ts
                    // so match_delta stays bounded, and draining it keeps the
                    // buffer from wedging at cap while a track is
                    // systematically behind. Since state_ts advances
                    // monotonically, no future state could fresh-match a
                    // frame with ts ≤ state_ts − range, so consuming it now is
                    // safe. Past-horizon frames (ts > state_ts) are left for a
                    // later state to claim, and we fall back to pure reuse.
                    StallPolicy::Freeze => {
                        if !frame_buf.is_empty() {
                            let c = self.cursors[track_i];
                            let cand = &frame_buf[c];
                            if cand.timestamp_us <= state_ts
                                && state_ts - cand.timestamp_us >= range
                            {
                                self.matched_scratch[track_i] = Some(MatchSlot {
                                    frame: cand.clone(),
                                    drain_to: Some(c),
                                    source: FrameSource::Stale,
                                });
                                continue;
                            }
                        }
                        if let Some(stale) = self.last_emitted_frames[track_i].clone() {
                            self.matched_scratch[track_i] = Some(MatchSlot {
                                frame: stale,
                                drain_to: None,
                                source: FrameSource::Stale,
                            });
                            continue;
                        }
                    }
                    // Keep the moment alive with a stand-in, so the healthy
                    // tracks and the state still reach the consumer. Geometry
                    // and recency both come from the track's last real frame:
                    // without one there is nothing to size a placeholder
                    // from, and reusing its timestamp keeps `match_delta`
                    // reporting the true age of the newest real pixels rather
                    // than a flattering zero.
                    StallPolicy::Omit => {
                        if let Some(last) = self.last_emitted_frames[track_i].clone() {
                            let data = Self::placeholder_pixels(
                                &mut self.placeholders[track_i],
                                &self.track_names[track_i],
                                last.width,
                                last.height,
                            );
                            self.matched_scratch[track_i] = Some(MatchSlot {
                                frame: Arc::new(VideoFrameData {
                                    width: last.width,
                                    height: last.height,
                                    data,
                                    timestamp_us: last.timestamp_us,
                                    source: FrameSource::Omitted,
                                }),
                                drain_to: None,
                                source: FrameSource::Omitted,
                            });
                            if let Some(tm) = self.metrics.track(&self.track_names[track_i]) {
                                tm.record_frame_omitted();
                            }
                            continue;
                        }
                    }
                    StallPolicy::Drop => {}
                }

                // `Freeze` and `Omit` land here only before the track's first
                // frame, with nothing to stand in with and no geometry to
                // synthesize from. If a future frame could still match, keep
                // waiting rather than discarding a moment we may yet be able
                // to satisfy; that startup window is exactly the historical
                // `reuse_stale_frames` behavior. Otherwise drop.
                if !unmatchable && stall.policy != StallPolicy::Drop {
                    if iter_blocker.is_none() {
                        iter_blocker = Some(track_i);
                    }
                    continue;
                }

                should_drop = true;
                drop_ahead_us = newest_ts.unwrap_or(logical_now).saturating_sub(state_ts);
                break;
            }

            if should_drop {
                self.note_unsyncable_drop(drop_ahead_us);
                let (_, values) = self.state_buffer.pop_front().unwrap();
                output.drops.push(self.build_typed_state_map(&values));
                self.metrics.record_state_dropped(1);
                // Retry next state with fresh iteration.
                continue;
            }

            if let Some(b) = iter_blocker {
                self.blocker = Some(b);
                self.metrics.record_blocker(b);
                return output;
            }

            // Record worst-case per-track alignment (against whichever frame
            // got used, fresh or stale — stale deltas can be arbitrarily large
            // so this surfaces video-freeze duration in metrics). Separately
            // flag observations that involved any stale match so ops can
            // distinguish "video is silently frozen / behind" from normal
            // operation.
            let mut worst_delta = 0u64;
            let mut any_stale = false;
            for slot in &self.matched_scratch {
                if let Some(s) = slot.as_ref() {
                    worst_delta = worst_delta.max(state_ts.abs_diff(s.frame.timestamp_us));
                    if s.source == FrameSource::Stale {
                        any_stale = true;
                    }
                }
            }
            self.metrics.record_observation(worst_delta);
            if any_stale {
                self.metrics.record_stale_observation();
            }

            let (ts, values) = self.state_buffer.pop_front().unwrap();

            let mut frames_map: HashMap<String, VideoFrameData> =
                HashMap::with_capacity(self.track_names.len());
            for track_i in 0..self.track_names.len() {
                if let Some(slot) = self.matched_scratch[track_i].take() {
                    if let Some(idx) = slot.drain_to {
                        // Drain the buffer up to and including the chosen
                        // frame and remember it as the stale fallback for
                        // later states that can't find a fresh match. Done
                        // for fresh matches AND for below-horizon reuse
                        // matches, so `last_emitted_frames` always advances
                        // with state_ts.
                        self.video_buffers[track_i].drain(0..=idx);
                        // Cursor was at or just past idx; after draining, shift it back.
                        self.cursors[track_i] = self.cursors[track_i].saturating_sub(idx + 1);
                        self.last_emitted_frames[track_i] = Some(slot.frame.clone());
                    }
                    // Pure reuse (drain_to == None): leave buffer / cursor /
                    // last-emitted untouched so a future in-range frame can
                    // still be claimed by a later state.
                    //
                    // Cheap clone: VideoFrameData carries Arc<[u8]>. The
                    // buffered frame is always stored as `Live`; stamp how it
                    // was actually used on the copy handed to the consumer.
                    let mut frame = (*slot.frame).clone();
                    frame.source = slot.source;
                    frames_map.insert(self.track_names[track_i].clone(), frame);
                }
            }

            let (typed_state, raw_state) = to_value_maps(&self.state_schema, &values);
            output.observations.push(Observation {
                state: typed_state,
                raw_state,
                frames: frames_map,
                timestamp_us: ts,
            });
        }
    }

    pub fn video_fill_snapshot(&self) -> HashMap<String, usize> {
        self.track_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), self.video_buffers[i].len()))
            .collect()
    }

    pub fn state_fill(&self) -> usize {
        self.state_buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(track: &str, ts: u64) -> (String, Arc<VideoFrameData>) {
        (
            track.to_string(),
            Arc::new(VideoFrameData {
                width: 2,
                height: 2,
                data: bytes::Bytes::from(vec![0u8; 12]),
                timestamp_us: ts,
                source: FrameSource::Live,
            }),
        )
    }

    fn push_f(buf: &mut SyncBuffer, track: &str, ts: u64) -> SyncOutput {
        let (name, frame) = make_frame(track, ts);
        buf.push_frame(&name, frame)
    }

    fn mk(names: &[String], fields: Vec<String>, config: SyncConfig) -> SyncBuffer {
        let metrics = Arc::new(MetricsRegistry::new(names));
        // Tests were written before typed fields — default every name to F64
        // so the internal observation builder has a dtype per position.
        let schema: Vec<FieldSpec> =
            fields.into_iter().map(|n| FieldSpec::new(n, DType::F64)).collect();
        SyncBuffer::new(names, schema, config, Vec::new(), metrics)
    }

    #[test]
    fn sync_single_track() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string(), "j2".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        assert!(push_f(&mut buf, "cam1", 1000).observations.is_empty());

        let out = buf.push_state(1010, vec![1.0, 2.0]);
        assert_eq!(out.observations.len(), 1);
        let obs = &out.observations[0];
        assert_eq!(obs.state["j1"], TypedValue::F64(1.0));
        assert_eq!(obs.state["j2"], TypedValue::F64(2.0));
        assert_eq!(obs.timestamp_us, 1010);
    }

    #[test]
    fn sync_multi_track() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        assert!(push_f(&mut buf, "cam1", 1000).observations.is_empty());
        assert!(buf.push_state(1005, vec![5.0]).observations.is_empty());

        let out = push_f(&mut buf, "cam2", 1002);
        assert_eq!(out.observations.len(), 1);
        assert!(out.observations[0].frames.contains_key("cam1"));
        assert!(out.observations[0].frames.contains_key("cam2"));
    }

    #[test]
    fn drop_unsyncable_state() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        assert!(buf.push_state(100, vec![1.0]).is_empty());
        let out = push_f(&mut buf, "cam1", 200_000);
        assert!(out.observations.is_empty());
        assert_eq!(out.drops.len(), 1);
        assert_eq!(out.drops[0]["j1"], TypedValue::F64(1.0));
    }

    #[test]
    fn out_of_range_waits() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        assert!(buf.push_state(50_000, vec![1.0]).is_empty());
        let out = push_f(&mut buf, "cam1", 50_010);
        assert_eq!(out.observations.len(), 1);
    }

    #[test]
    fn buffer_overflow_evicts_oldest() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config =
            SyncConfig { video_buffer_size: 2, state_buffer_size: 2, ..Default::default() };
        let mut buf = mk(&tracks, fields, config);

        for ts in [100, 200, 300] {
            let _ = push_f(&mut buf, "cam1", ts);
        }

        let cam_buf = &buf.video_buffers[buf.track_index["cam1"]];
        assert_eq!(cam_buf.len(), 2);
        assert_eq!(cam_buf[0].timestamp_us, 200);
        assert_eq!(cam_buf[1].timestamp_us, 300);
    }

    #[test]
    fn clear_flushes_all() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        let _ = push_f(&mut buf, "cam1", 1000);
        let _ = buf.push_state(1000, vec![1.0]);
        buf.clear();

        assert!(buf.video_buffers.iter().all(|b| b.is_empty()));
        assert!(buf.state_buffer.is_empty());
        assert!(buf.cursors.iter().all(|&c| c == 0));
        assert!(buf.blocker.is_none());
    }

    // --- New algorithm edge cases ---

    /// Cursor should advance monotonically across many sequential syncs.
    #[test]
    fn cursor_advances_across_sequential_matches() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig { video_buffer_size: 100, ..Default::default() };
        let mut buf = mk(&tracks, fields, config);

        // Push 10 frames at 1000us intervals.
        for i in 0..10 {
            let _ = push_f(&mut buf, "cam1", 1_000 + i * 1_000);
        }
        // Match each with a state, each state should consume one frame.
        let mut matched_ts = Vec::new();
        for i in 0..10 {
            let out = buf.push_state(1_010 + i * 1_000, vec![i as f64]);
            assert_eq!(out.observations.len(), 1, "state #{} should produce 1 obs", i);
            matched_ts.push(out.observations[0].frames["cam1"].timestamp_us);
        }
        assert_eq!(matched_ts, (0..10).map(|i| 1_000 + i * 1_000).collect::<Vec<_>>());
    }

    /// Non-blocker push should defer try_sync, but a subsequent push to the
    /// blocker must still produce the observation (no lost state).
    #[test]
    fn non_blocker_push_defers_but_converges() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        // State + cam2 present; cam1 empty → cam1 is the blocker.
        assert!(buf.push_state(1_000, vec![1.0]).is_empty());
        assert!(push_f(&mut buf, "cam2", 1_005).is_empty());
        assert_eq!(buf.blocker, Some(buf.track_index["cam1"]));

        // Push another cam2 frame — not the blocker, try_sync should skip.
        // The observation count stays at 0 either way, so we just check no
        // spurious work: buffer accepted the push.
        assert!(push_f(&mut buf, "cam2", 1_006).is_empty());
        assert_eq!(buf.video_buffers[buf.track_index["cam2"]].len(), 2);

        // Now push to the blocker — observation must fire.
        let out = push_f(&mut buf, "cam1", 1_008);
        assert_eq!(out.observations.len(), 1);
        assert!(buf.blocker.is_none());
    }

    /// If eviction on a non-blocker track removes the only in-range frame,
    /// the state must drop (not silently stall).
    #[test]
    fn eviction_on_non_blocker_can_trigger_drop() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig {
            video_buffer_size: 1,
            state_buffer_size: 10,
            search_range_us: 30_000,
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        // State at 1_000; cam1 empty (blocker); cam2 has a frame in range.
        assert!(buf.push_state(1_000, vec![1.0]).is_empty());
        assert!(push_f(&mut buf, "cam2", 1_005).is_empty());
        assert_eq!(buf.blocker, Some(buf.track_index["cam1"]));

        // Push new cam2 frame far in the future; cap=1 means the in-range
        // frame is evicted. Eager drop path must fire even though cam2 is not
        // the blocker.
        let out = push_f(&mut buf, "cam2", 500_000);
        assert!(out.observations.is_empty());
        assert_eq!(out.drops.len(), 1, "state should be dropped once its cam2 match is evicted");
    }

    /// Out-of-order state timestamps must still find the correct match via
    /// cursor rewind.
    #[test]
    fn out_of_order_state_rewinds_cursor() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        // Pre-populate frames spanning a wide range.
        for ts in [1_000u64, 5_000, 10_000, 50_000, 100_000] {
            let _ = push_f(&mut buf, "cam1", ts);
        }

        // First match at high ts advances cursor forward.
        let out = buf.push_state(100_005, vec![0.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 100_000);

        // Re-populate so there's a frame near an earlier ts, then push an
        // earlier state — cursor rewind must find it.
        let _ = push_f(&mut buf, "cam1", 200_000);
        let _ = push_f(&mut buf, "cam1", 200_005);
        let out = buf.push_state(200_002, vec![0.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 200_000);
    }

    /// State eviction pushing a new head state clears the blocker so the new
    /// head gets re-evaluated immediately.
    #[test]
    fn state_eviction_updates_head_and_clears_blocker() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig { state_buffer_size: 1, ..Default::default() };
        let mut buf = mk(&tracks, fields, config);

        // No frames yet: both push_state calls see an empty cam1 → wait.
        // cap_state=1 means the second state evicts the first.
        assert!(buf.push_state(1_000, vec![1.0]).is_empty());
        assert_eq!(buf.blocker, Some(0));
        // Second push evicts state@1000; overflow surfaces as a drop.
        let out = buf.push_state(2_000, vec![2.0]);
        assert!(out.observations.is_empty());
        assert_eq!(out.drops.len(), 1);
        assert_eq!(out.drops[0]["j1"], TypedValue::F64(1.0));

        // Only the second state remains. A frame matching it fires the obs.
        let out = push_f(&mut buf, "cam1", 2_005);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(
            out.observations[0].state["j1"],
            TypedValue::F64(2.0),
            "evicted state should not leak through"
        );
        assert_eq!(out.observations[0].timestamp_us, 2_000);
    }

    /// Drop must fire when the *newest* frame is past the horizon, even if an
    /// older frame is still buffered below the match window. Under the old
    /// front-based check, the state would stall until eviction dragged the old
    /// frame through the horizon.
    #[test]
    fn drop_triggers_on_back_past_horizon() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig {
            video_buffer_size: 10,
            state_buffer_size: 10,
            search_range_us: 500,
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        let _ = push_f(&mut buf, "cam1", 1_000); // far below state - range (2_500)
        assert!(buf.push_state(3_000, vec![1.0]).is_empty());

        // Newest frame lands past state + range (3_500). Even though the old
        // 1_000 frame is still in the buffer, no future frame can be < 5_000,
        // so the state is permanently unmatchable.
        let out = push_f(&mut buf, "cam1", 5_000);
        assert!(out.observations.is_empty());
        assert_eq!(out.drops.len(), 1, "state should drop as soon as back passes horizon");
    }

    /// Boundary: a frame landing exactly at `state_ts + range` is not a match
    /// (strict `<`), and all future frames are ≥ that ts, so the state drops.
    #[test]
    fn drop_fires_at_exact_range_boundary() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig {
            video_buffer_size: 10,
            state_buffer_size: 10,
            search_range_us: 500,
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        assert!(buf.push_state(1_000, vec![1.0]).is_empty());
        let out = push_f(&mut buf, "cam1", 1_500); // delta == range, not a match
        assert!(out.observations.is_empty());
        assert_eq!(out.drops.len(), 1);
    }

    /// State-buffer overflow must surface evicted states via `output.drops`
    /// so the `on_drop` callback can fire, matching spec behavior.
    #[test]
    fn state_overflow_with_tracks_reports_drops() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig { state_buffer_size: 2, ..Default::default() };
        let mut buf = mk(&tracks, fields, config);

        // No frames: each push_state blocks (no sync), fills the state buffer.
        assert!(buf.push_state(100, vec![1.0]).drops.is_empty());
        assert!(buf.push_state(200, vec![2.0]).drops.is_empty());
        // Third push triggers overflow; state@100 must appear in drops.
        let out = buf.push_state(300, vec![3.0]);
        assert_eq!(out.drops.len(), 1);
        assert_eq!(out.drops[0]["j1"], TypedValue::F64(1.0));
    }

    /// With a widened range (>1 tick), a state whose exact frame was lost
    /// falls back to an adjacent frame if no later state has a closer claim.
    #[test]
    fn wide_range_matches_neighbor_when_native_lost() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        // 30fps ticks = 33_333us; tolerance 1.5 → range = 50_000us.
        let config = SyncConfig {
            video_buffer_size: 5,
            state_buffer_size: 5,
            search_range_us: 50_000,
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        // Frame at tick 0 stands in for "T−1"; frame at T was lost; only
        // frame@0 is available for state@33_333.
        let _ = push_f(&mut buf, "cam1", 0);
        let out = buf.push_state(33_333, vec![1.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 0);
    }

    /// Fair-share: if an earlier state and a later state are both in the
    /// buffer and a single frame sits closer to the later state, the earlier
    /// state must NOT steal it. It may drop, but the later state gets to use
    /// its own frame.
    #[test]
    fn fair_share_prevents_stealing() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig {
            video_buffer_size: 5,
            state_buffer_size: 5,
            search_range_us: 50_000, // tolerance 1.5 at 30fps
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        // Both states buffered before any frames arrive.
        assert!(buf.push_state(0, vec![1.0]).is_empty());
        assert!(buf.push_state(33_333, vec![2.0]).is_empty());

        // frame@33_333 is closer to state@33_333 than to state@0;
        // fair-share must keep state@0 from grabbing it.
        let out = push_f(&mut buf, "cam1", 33_333);
        assert!(
            out.observations.is_empty(),
            "state@0 must not steal frame@33_333 from state@33_333"
        );

        // Push a later frame past state@0's horizon to force the drop;
        // state@33_333 then matches its own frame.
        let out = push_f(&mut buf, "cam1", 100_000);
        assert_eq!(out.drops.len(), 1, "state@0 drops once its horizon is crossed");
        assert_eq!(out.drops[0]["j1"], TypedValue::F64(1.0));
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].state["j1"], TypedValue::F64(2.0));
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 33_333);
    }

    /// Tight range (<1 tick) preserves the legacy drop-on-loss behavior:
    /// a state can't reach an adjacent frame, so it drops as soon as a
    /// later frame crosses the horizon.
    #[test]
    fn tight_range_still_drops_on_loss() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        // tolerance 0.5 at 30fps → range = 16_666us, adjacent frames unreachable.
        let config = SyncConfig {
            video_buffer_size: 5,
            state_buffer_size: 5,
            search_range_us: 16_666,
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        let _ = push_f(&mut buf, "cam1", 0);
        assert!(buf.push_state(33_333, vec![1.0]).is_empty()); // blocks: no match in range
        let out = push_f(&mut buf, "cam1", 100_000); // crosses horizon, fires drop
        assert!(out.observations.is_empty());
        assert_eq!(out.drops.len(), 1, "tight range must drop when native frame is missing");
    }

    // --- reuse_stale_frames (opt-in): no state is dropped to video-frame
    //     loss once every track has emitted at least once. Video "freezes"
    //     on the last good frame instead. State-buffer overflow and the
    //     pre-first-emission startup window are the only remaining drop
    //     sources.

    fn reuse_config() -> SyncConfig {
        SyncConfig {
            video_buffer_size: 5,
            state_buffer_size: 5,
            search_range_us: 500,
            default_stall: StallConfig { max_lag_us: Some(0), policy: StallPolicy::Freeze },
        }
    }

    /// After one successful emission, a subsequent state pushed with an
    /// empty video buffer reuses the last-emitted frame and emits
    /// immediately.
    #[test]
    fn reuse_empty_buffer_emits_with_last_frame() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        // First emission establishes a last-emitted frame.
        let _ = push_f(&mut buf, "cam1", 1_000);
        let out = buf.push_state(1_100, vec![1.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 1_000);

        // Next state: no new frames arrived. Strict policy would wait;
        // reuse emits with the last good frame.
        let out = buf.push_state(2_000, vec![2.0]);
        assert_eq!(out.drops.len(), 0);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].state["j1"], TypedValue::F64(2.0));
        assert_eq!(
            out.observations[0].frames["cam1"].timestamp_us, 1_000,
            "stale reuse uses the last emitted frame"
        );
    }

    /// When a frame arrives past the head state's horizon (no in-range match
    /// possible), reuse emits with the last-emitted frame instead of dropping.
    /// The newly arrived frame is left in the buffer for a later state.
    #[test]
    fn reuse_past_horizon_emits_with_last_frame() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        // First emission sets last-emitted to frame@0.
        let _ = push_f(&mut buf, "cam1", 0);
        let out = buf.push_state(10, vec![0.0]);
        assert_eq!(out.observations.len(), 1);

        // Load the buffer with a frame past state@100's horizon (d = 900,
        // range = 500, so no match and newest >= state + range).
        let _ = push_f(&mut buf, "cam1", 900);
        let out = buf.push_state(100, vec![1.0]);
        assert_eq!(out.drops.len(), 0, "reuse replaces the horizon drop");
        assert_eq!(out.observations.len(), 1);
        assert_eq!(
            out.observations[0].frames["cam1"].timestamp_us, 0,
            "stale reuse, not the unmatched buffered frame"
        );
        // The buffered frame must still be available for a later state.
        let out = buf.push_state(800, vec![2.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 900);
    }

    /// Before any frame has ever been emitted on a track, reuse has no
    /// fallback. A frame arriving past the horizon still drops the state
    /// (matching the strict policy), so the buffer stays bounded during a
    /// broken-video startup.
    #[test]
    fn reuse_startup_no_fallback_still_drops() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        assert!(buf.push_state(1_000, vec![1.0]).is_empty());
        let out = push_f(&mut buf, "cam1", 100_000);
        assert_eq!(out.drops.len(), 1, "no last-emitted frame yet — reuse can't save the state");
        assert_eq!(out.observations.len(), 0);
    }

    /// Startup with a totally dead track: state-buffer overflow drops
    /// accumulated states so memory stays bounded.
    #[test]
    fn reuse_startup_overflow_still_drops() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig { state_buffer_size: 2, ..reuse_config() };
        let mut buf = mk(&tracks, fields, config);

        assert!(buf.push_state(100, vec![1.0]).is_empty());
        assert!(buf.push_state(200, vec![2.0]).is_empty());
        let out = buf.push_state(300, vec![3.0]);
        assert_eq!(out.drops.len(), 1, "overflow drops during total video loss");
        assert_eq!(out.drops[0]["j1"], TypedValue::F64(1.0));
    }

    /// Multi-track: one track freezes while the other keeps delivering fresh
    /// frames. Observations keep flowing, mixing stale and fresh frames.
    #[test]
    fn reuse_multi_track_freeze_one_keeps_other_fresh() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        // First emission on both tracks.
        let _ = push_f(&mut buf, "cam1", 1_000);
        let _ = push_f(&mut buf, "cam2", 1_000);
        let out = buf.push_state(1_050, vec![1.0]);
        assert_eq!(out.observations.len(), 1);

        // cam1 freezes; cam2 keeps delivering. State still emits, with stale
        // cam1 and fresh cam2.
        let _ = push_f(&mut buf, "cam2", 2_000);
        let out = buf.push_state(2_050, vec![2.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 1_000);
        assert_eq!(out.observations[0].frames["cam2"].timestamp_us, 2_000);
    }

    /// Fresh match still wins over stale reuse: if an in-range frame exists,
    /// it's used, the buffer drains, and `last_emitted` advances.
    #[test]
    fn reuse_prefers_fresh_match_when_available() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        let _ = push_f(&mut buf, "cam1", 1_000);
        let _ = buf.push_state(1_050, vec![1.0]);

        // Push a fresh in-range frame; state should match it, not reuse f@1000.
        let _ = push_f(&mut buf, "cam1", 2_000);
        let out = buf.push_state(2_100, vec![2.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].timestamp_us, 2_000);
        assert!(buf.last_emitted_frames[0].as_ref().unwrap().timestamp_us == 2_000);
    }

    /// clear() resets last-emitted frames so reuse after clear behaves like
    /// a fresh start.
    #[test]
    fn reuse_clear_resets_last_emitted() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        let _ = push_f(&mut buf, "cam1", 1_000);
        let out = buf.push_state(1_050, vec![1.0]);
        assert_eq!(out.observations.len(), 1);
        assert!(buf.last_emitted_frames[0].is_some());

        buf.clear();
        assert!(buf.last_emitted_frames[0].is_none());

        // After clear we're back in startup: no fallback, past-horizon drops.
        assert!(buf.push_state(2_000, vec![2.0]).is_empty());
        let out = push_f(&mut buf, "cam1", 100_000);
        assert_eq!(out.drops.len(), 1);
    }

    /// Below-horizon case: buffer holds only frames too old to fresh-match.
    /// Under reuse, the state emits immediately with the newest below-horizon
    /// frame (not the first-ever-emitted one), the buffer drains those old
    /// frames, and `last_emitted_frames` advances so match_delta stays
    /// bounded. No future state can fresh-match a frame with ts ≤ state_ts −
    /// range (state_ts is monotonic), so consuming it is safe.
    #[test]
    fn reuse_below_horizon_advances_to_best_buffered() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        // First emission sets last_emitted = f@0.
        let _ = push_f(&mut buf, "cam1", 0);
        let out = buf.push_state(10, vec![0.0]);
        assert_eq!(out.observations.len(), 1);

        // f@200 is below state@5_000's horizon (Δ = 4_800 ≫ range=500) but
        // newer than the stored last-emitted f@0. Reuse must prefer it and
        // drain it — otherwise match_delta keeps growing against f@0 forever.
        let _ = push_f(&mut buf, "cam1", 200);
        let out = buf.push_state(5_000, vec![1.0]);
        assert_eq!(out.drops.len(), 0);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(
            out.observations[0].frames["cam1"].timestamp_us, 200,
            "stale reuse should advance to the newest below-horizon frame"
        );
        // Buffer drained; last-emitted advanced.
        assert_eq!(buf.video_buffers[0].len(), 0);
        assert_eq!(buf.last_emitted_frames[0].as_ref().unwrap().timestamp_us, 200);
    }

    /// Regression: under steady-state stale reuse, `last_emitted_frames` must
    /// keep advancing with state_ts as long as frames keep arriving below
    /// horizon. Before the fix, the first-ever-emitted frame became a
    /// permanent fallback, causing match_delta to grow linearly with session
    /// length and buf_vid_max to pin at cap.
    #[test]
    fn reuse_steady_state_advances_last_emitted_below_horizon() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        // Narrow range so incoming frames are never a fresh match.
        let config = SyncConfig {
            video_buffer_size: 5,
            state_buffer_size: 5,
            search_range_us: 100,
            default_stall: StallConfig { max_lag_us: Some(0), policy: StallPolicy::Freeze },
        };
        let mut buf = mk(&tracks, fields, config);

        // First emission establishes last_emitted = f@0.
        let _ = push_f(&mut buf, "cam1", 0);
        let out = buf.push_state(50, vec![0.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(buf.last_emitted_frames[0].as_ref().unwrap().timestamp_us, 0);

        // Simulate a track that arrives consistently behind state_ts by more
        // than `range` — every state stale-reuses. The pointer must still
        // advance to the newest below-horizon frame each round.
        for i in 1..=10u64 {
            let frame_ts = i * 1_000; // behind state_ts by 500us, d=500 >= range
            let state_ts = frame_ts + 500;
            let _ = push_f(&mut buf, "cam1", frame_ts);
            let out = buf.push_state(state_ts, vec![i as f64]);
            assert_eq!(out.observations.len(), 1);
            assert_eq!(out.drops.len(), 0);
            assert_eq!(
                out.observations[0].frames["cam1"].timestamp_us, frame_ts,
                "round #{i}: stale match should be the newest below-horizon frame"
            );
            // Buffer drains so it never pins at cap.
            assert_eq!(buf.video_buffers[0].len(), 0);
            assert_eq!(buf.last_emitted_frames[0].as_ref().unwrap().timestamp_us, frame_ts);
        }
    }

    /// `stale_observations_emitted` counts only observations that used a
    /// reused frame. Fresh emissions leave the counter alone.
    #[test]
    fn reuse_stale_metric_counts_only_stale_emissions() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let metrics = Arc::new(MetricsRegistry::new(&tracks));
        let schema: Vec<FieldSpec> =
            fields.into_iter().map(|n| FieldSpec::new(n, DType::F64)).collect();
        let mut buf = SyncBuffer::new(&tracks, schema, reuse_config(), Vec::new(), metrics.clone());

        // Fresh emission #1.
        let _ = buf.push_frame(
            "cam1",
            Arc::new(VideoFrameData {
                width: 2,
                height: 2,
                data: bytes::Bytes::from(vec![0u8; 12]),
                timestamp_us: 1_000,
                source: FrameSource::Live,
            }),
        );
        let _ = buf.push_state(1_050, vec![1.0]);

        // Fresh emission #2.
        let _ = buf.push_frame(
            "cam1",
            Arc::new(VideoFrameData {
                width: 2,
                height: 2,
                data: bytes::Bytes::from(vec![0u8; 12]),
                timestamp_us: 2_000,
                source: FrameSource::Live,
            }),
        );
        let _ = buf.push_state(2_050, vec![2.0]);

        let snap = metrics.snapshot(HashMap::new(), 0);
        assert_eq!(snap.sync.observations_emitted, 2);
        assert_eq!(snap.sync.stale_observations_emitted, 0);

        // Stale emission: no new frame, state reuses f@2_000.
        let _ = buf.push_state(3_000, vec![3.0]);
        let snap = metrics.snapshot(HashMap::new(), 0);
        assert_eq!(snap.sync.observations_emitted, 3);
        assert_eq!(snap.sync.stale_observations_emitted, 1);
    }

    /// Under reuse, once every track has emitted at least once, any
    /// push_state emits immediately (fresh match or stale reuse) — states
    /// never linger in the buffer for the eviction-escape hatch to save.
    /// Verified here: a long run of pushes with one track fully frozen
    /// produces an observation per state and zero drops, and the state
    /// buffer stays empty.
    #[test]
    fn reuse_steady_state_keeps_state_buffer_empty() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        // First emission on both tracks.
        let _ = push_f(&mut buf, "cam1", 1_000);
        let _ = push_f(&mut buf, "cam2", 1_000);
        let out = buf.push_state(1_050, vec![0.0]);
        assert_eq!(out.observations.len(), 1);

        // cam1 freezes; cam2 keeps delivering. Each state must emit with a
        // fresh cam2 frame and a stale cam1 frame, leaving the state buffer
        // empty after every push.
        let mut emitted = 0;
        for i in 1..10u64 {
            let ts = 1_000 + i * 1_000;
            let _ = push_f(&mut buf, "cam2", ts);
            let out = buf.push_state(ts + 50, vec![i as f64]);
            emitted += out.observations.len();
            assert_eq!(out.drops.len(), 0);
            assert_eq!(buf.state_fill(), 0, "state buffer should not accumulate under reuse");
        }
        assert_eq!(emitted, 9);
    }

    /// Default config (reuse off) preserves strict drop-on-horizon behavior
    /// even when a last-emitted frame exists.
    #[test]
    fn reuse_off_by_default_preserves_strict_drop() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let config = SyncConfig {
            video_buffer_size: 5,
            state_buffer_size: 5,
            search_range_us: 500,
            // reuse_stale_frames default: false
            ..Default::default()
        };
        let mut buf = mk(&tracks, fields, config);

        let _ = push_f(&mut buf, "cam1", 1_000);
        let _ = buf.push_state(1_050, vec![1.0]);

        // Past-horizon frame must drop the state under strict policy.
        let _ = push_f(&mut buf, "cam1", 2_000);
        let out = buf.push_state(100, vec![2.0]);
        assert_eq!(out.observations.len(), 0);
        assert_eq!(out.drops.len(), 1);
    }

    /// Sanity: inputs that stress the binary/cursor path with many empty and
    /// partial iterations should never panic or produce spurious observations.
    #[test]
    fn stress_no_spurious_observations() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        let mut total_obs = 0;
        // Push 100 interleaved events; each state needs frames on BOTH tracks
        // within 30ms.
        for i in 0..100u64 {
            let ts = 1_000 + i * 1_000;
            let out1 = push_f(&mut buf, "cam1", ts);
            let out2 = push_f(&mut buf, "cam2", ts + 100);
            let out3 = buf.push_state(ts + 50, vec![i as f64]);
            total_obs += out1.observations.len();
            total_obs += out2.observations.len();
            total_obs += out3.observations.len();
        }
        assert_eq!(total_obs, 100);
    }

    // --- FrameSource tagging -------------------------------------------
    //     Every frame handed to a consumer records how it was actually
    //     used, so a single observation is self-describing: a policy can
    //     tell a measurement from a substitute without consulting a
    //     process-wide metric.

    /// A frame matched inside the tolerance window is tagged `Live`.
    #[test]
    fn fresh_match_tagged_live() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, SyncConfig::default());

        let _ = push_f(&mut buf, "cam1", 1_000);
        let out = buf.push_state(1_000, vec![1.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam1"].source, FrameSource::Live);
    }

    /// Reusing the last-emitted frame tags it `Stale`, and the frame keeps
    /// its own timestamp so the consumer can compute the age itself.
    #[test]
    fn reused_last_frame_tagged_stale() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        let _ = push_f(&mut buf, "cam1", 1_000);
        let out = buf.push_state(1_100, vec![1.0]);
        assert_eq!(out.observations[0].frames["cam1"].source, FrameSource::Live);

        // No new frame arrives; the next state reuses the 1_000 frame.
        let out = buf.push_state(2_000, vec![2.0]);
        assert_eq!(out.observations.len(), 1);
        let f = &out.observations[0].frames["cam1"];
        assert_eq!(f.source, FrameSource::Stale);
        assert_eq!(f.timestamp_us, 1_000, "stale frame keeps its own ts");
        assert_eq!(out.observations[0].timestamp_us, 2_000);
    }

    /// The buffered below-horizon reuse path is tagged `Stale` too, not just
    /// the stored last-emitted fallback.
    #[test]
    fn below_horizon_reuse_tagged_stale() {
        let tracks = vec!["cam1".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        // Establish a last-emitted frame.
        let _ = push_f(&mut buf, "cam1", 1_000);
        let _ = buf.push_state(1_100, vec![1.0]);

        // A frame lands far below the next state's horizon: drained and
        // reused rather than matched.
        let _ = push_f(&mut buf, "cam1", 2_000);
        let out = buf.push_state(9_000, vec![2.0]);
        assert_eq!(out.observations.len(), 1);
        let f = &out.observations[0].frames["cam1"];
        assert_eq!(f.source, FrameSource::Stale);
        assert_eq!(f.timestamp_us, 2_000, "drained the below-horizon frame");
    }

    /// Tagging is per-track: a live track stays `Live` in the same
    /// observation where another track is reusing.
    #[test]
    fn source_is_per_track() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, reuse_config());

        let _ = push_f(&mut buf, "cam1", 1_000);
        let _ = push_f(&mut buf, "cam2", 1_000);
        let out = buf.push_state(1_100, vec![1.0]);
        assert_eq!(out.observations.len(), 1);

        // cam1 keeps sending, cam2 goes silent.
        let _ = push_f(&mut buf, "cam1", 2_000);
        let out = buf.push_state(2_050, vec![2.0]);
        assert_eq!(out.observations.len(), 1);
        let frames = &out.observations[0].frames;
        assert_eq!(frames["cam1"].source, FrameSource::Live);
        assert_eq!(frames["cam2"].source, FrameSource::Stale);
    }

    // --- max_lag / on_stall ---------------------------------------------

    fn stall_config(max_lag_us: Option<u64>, policy: StallPolicy) -> SyncConfig {
        SyncConfig {
            // Large buffers, so capacity eviction is never what resolves a
            // moment in these tests — the lag budget is.
            video_buffer_size: 100,
            state_buffer_size: 100,
            search_range_us: 1_000,
            default_stall: StallConfig { max_lag_us, policy },
        }
    }

    /// With no budget (the historical default), a head blocked by a silent
    /// track waits however far the other streams run ahead. Guards the
    /// backward-compatible path.
    #[test]
    fn no_max_lag_waits_indefinitely() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(None, StallPolicy::Drop));

        let _ = push_f(&mut buf, "cam1", 10_000);
        assert!(buf.push_state(10_000, vec![1.0]).is_empty(), "waits on cam2");

        let out = buf.push_state(500_000, vec![2.0]);
        assert!(out.observations.is_empty());
        assert!(out.drops.is_empty(), "no budget → the head waits, never resolves");
    }

    /// The stream clock advances on *any* stream, so a head blocked by a
    /// silent track resolves even when state output has stopped. Capacity
    /// eviction can never reach this case: it only runs on `push_state`.
    ///
    /// Regression guard for the blocker short-circuit in `push_frame` — if
    /// pushes to a non-blocking track skip `try_sync`, the budget never gets
    /// evaluated and the head strands forever.
    #[test]
    fn max_lag_resolves_when_only_video_advances() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(50_000), StallPolicy::Drop));

        let _ = push_f(&mut buf, "cam1", 10_000);
        assert!(buf.push_state(10_000, vec![1.0]).is_empty(), "waits on cam2");

        // No further states at all. cam1 keeps sending; the stream clock
        // crosses the 50ms budget and the head resolves on a frame push.
        let out = push_f(&mut buf, "cam1", 70_000);
        assert_eq!(out.drops.len(), 1, "budget resolved the stranded head");
        assert!(out.observations.is_empty());
        assert_eq!(buf.metrics.snapshot(HashMap::new(), 0).sync.states_dropped, 1);
    }

    /// A budget never resolves a moment that still has an in-range match on
    /// every track — no spurious drops during normal operation.
    #[test]
    fn max_lag_does_not_touch_matched_states() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(1), StallPolicy::Drop));

        for i in 0..20u64 {
            let ts = 10_000 + i * 10_000;
            let _ = push_f(&mut buf, "cam1", ts);
            let _ = push_f(&mut buf, "cam2", ts);
            let out = buf.push_state(ts, vec![i as f64]);
            assert_eq!(out.observations.len(), 1, "tick {i} must emit");
            assert!(out.drops.is_empty(), "tick {i} must not drop");
        }
    }

    /// `Freeze` resolves a stalled track with its last good frame, so the
    /// healthy tracks and the state keep flowing.
    #[test]
    fn freeze_keeps_observations_flowing() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(50_000), StallPolicy::Freeze));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let _ = push_f(&mut buf, "cam2", 10_000);
        assert_eq!(buf.push_state(10_000, vec![1.0]).observations.len(), 1);

        // cam2 goes silent. The next moment is still inside its budget — it
        // has only just arrived — so it waits rather than resolving early.
        let _ = push_f(&mut buf, "cam1", 70_000);
        let out = buf.push_state(70_000, vec![2.0]);
        assert!(out.observations.is_empty(), "still inside the budget");

        // cam1 carries the stream clock 60ms past that moment, over the 50ms
        // budget. It now resolves with cam2 frozen rather than discarded.
        let out = push_f(&mut buf, "cam1", 130_000);
        assert_eq!(out.observations.len(), 1, "freeze keeps the moment");
        let frames = &out.observations[0].frames;
        assert_eq!(frames["cam1"].source, FrameSource::Live);
        assert_eq!(frames["cam1"].timestamp_us, 70_000);
        assert_eq!(frames["cam2"].source, FrameSource::Stale);
        assert_eq!(frames["cam2"].timestamp_us, 10_000, "cam2's last good frame");
    }

    /// Policies are per track: a `Drop` track and a `Freeze` track in the
    /// same buffer resolve independently.
    #[test]
    fn stall_policy_is_per_track() {
        let tracks = vec!["wrist".to_string(), "scene".to_string()];
        let config = stall_config(Some(50_000), StallPolicy::Drop);
        let metrics = Arc::new(MetricsRegistry::new(&tracks));
        let schema = vec![FieldSpec::new("j1".to_string(), DType::F64)];
        // wrist is load-bearing: no observation beats a wrong one. scene is
        // not: freeze it rather than losing the moment.
        let stall = vec![
            StallConfig { max_lag_us: Some(50_000), policy: StallPolicy::Drop },
            StallConfig { max_lag_us: Some(50_000), policy: StallPolicy::Freeze },
        ];
        let mut buf = SyncBuffer::new(&tracks, schema, config, stall, metrics);

        let _ = push_f(&mut buf, "wrist", 10_000);
        let _ = push_f(&mut buf, "scene", 10_000);
        assert_eq!(buf.push_state(10_000, vec![1.0]).observations.len(), 1);

        // scene stalls. wrist carries the clock past the budget, and scene's
        // Freeze policy keeps the moment alive.
        let _ = push_f(&mut buf, "wrist", 70_000);
        assert!(buf.push_state(70_000, vec![2.0]).observations.is_empty());
        let out = push_f(&mut buf, "wrist", 130_000);
        assert_eq!(out.observations.len(), 1, "scene freezes, moment survives");
        assert_eq!(out.observations[0].frames["scene"].source, FrameSource::Stale);

        // Now the reverse: wrist stalls while scene carries the clock. Its
        // Drop policy discards the moment rather than substituting.
        let _ = push_f(&mut buf, "scene", 200_000);
        assert!(buf.push_state(200_000, vec![3.0]).observations.is_empty());
        let out = push_f(&mut buf, "scene", 260_000);
        assert!(out.observations.is_empty(), "wrist is load-bearing");
        assert_eq!(out.drops.len(), 1, "moment discarded rather than faked");
    }

    /// `Freeze` still waits through the startup window: before a track's
    /// first frame there is nothing to substitute, and a future frame could
    /// still match, so the moment is held rather than discarded.
    #[test]
    fn freeze_waits_before_first_frame() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(0), StallPolicy::Freeze));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let out = buf.push_state(10_000, vec![1.0]);
        assert!(out.observations.is_empty(), "cam2 has never sent");
        assert!(out.drops.is_empty(), "and nothing is discarded yet");

        // cam2's first frame arrives in range: the held moment now emits.
        let out = push_f(&mut buf, "cam2", 10_000);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam2"].source, FrameSource::Live);
    }

    // --- on_stall: omit --------------------------------------------------

    /// The point of `Omit`: one silent camera must not blank the others.
    /// Under `Drop` this same sequence yields no observation at all, so the
    /// operator loses every camera because one died.
    #[test]
    fn omit_keeps_healthy_tracks_visible() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(50_000), StallPolicy::Omit));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let _ = push_f(&mut buf, "cam2", 10_000);
        assert_eq!(buf.push_state(10_000, vec![1.0]).observations.len(), 1);

        // cam2 dies. cam1 carries the clock past the budget.
        let _ = push_f(&mut buf, "cam1", 70_000);
        assert!(buf.push_state(70_000, vec![2.0]).observations.is_empty());
        let out = push_f(&mut buf, "cam1", 130_000);

        assert_eq!(out.observations.len(), 1, "the moment survives");
        let frames = &out.observations[0].frames;
        assert_eq!(frames["cam1"].source, FrameSource::Live, "healthy camera still live");
        assert_eq!(frames["cam2"].source, FrameSource::Omitted);
        assert_eq!(frames.len(), 2, "every declared track still has a key");
    }

    /// The key is never removed, so `frames[name]` cannot start failing when
    /// a camera dies — the reason a placeholder is substituted rather than
    /// the entry dropped.
    #[test]
    fn omit_never_removes_the_key() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(0), StallPolicy::Omit));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let _ = push_f(&mut buf, "cam2", 10_000);
        assert_eq!(buf.push_state(10_000, vec![1.0]).observations.len(), 1);

        for i in 1..5u64 {
            let ts = 10_000 + i * 60_000;
            let _ = push_f(&mut buf, "cam1", ts);
            let _ = buf.push_state(ts, vec![i as f64]);
            let out = push_f(&mut buf, "cam1", ts + 60_000);
            for obs in &out.observations {
                assert!(obs.frames.contains_key("cam2"), "cam2 key present at tick {i}");
            }
        }
    }

    /// A placeholder carries the last real frame's timestamp, not the
    /// state's: `match_delta` must report how stale the newest real pixels
    /// are, not a flattering zero.
    #[test]
    fn omit_frame_reports_real_recency() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(50_000), StallPolicy::Omit));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let _ = push_f(&mut buf, "cam2", 10_000);
        let _ = buf.push_state(10_000, vec![1.0]);

        let _ = push_f(&mut buf, "cam1", 70_000);
        assert!(buf.push_state(70_000, vec![2.0]).observations.is_empty());
        let out = push_f(&mut buf, "cam1", 130_000);
        let f = &out.observations[0].frames["cam2"];
        assert_eq!(f.timestamp_us, 10_000, "last real frame's ts, not the state's");
        assert_eq!(f.width, 2, "geometry inherited from the real frame");
        assert_eq!(f.height, 2);
    }

    /// Omission is counted per track, so ops can tell which camera is down
    /// without diffing key sets.
    #[test]
    fn omit_is_counted_per_track() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(50_000), StallPolicy::Omit));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let _ = push_f(&mut buf, "cam2", 10_000);
        let _ = buf.push_state(10_000, vec![1.0]);
        let _ = push_f(&mut buf, "cam1", 70_000);
        let _ = buf.push_state(70_000, vec![2.0]);
        let _ = push_f(&mut buf, "cam1", 130_000);

        let snap = buf.metrics.snapshot(HashMap::new(), 0);
        assert_eq!(snap.sync.frames_omitted.get("cam2"), Some(&1));
        assert_eq!(snap.sync.frames_omitted.get("cam1"), Some(&0), "healthy track untouched");
    }

    /// Before a track's first frame there is no geometry to synthesize from,
    /// so `Omit` waits like the others rather than inventing a size.
    #[test]
    fn omit_waits_before_first_frame() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(0), StallPolicy::Omit));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let out = buf.push_state(10_000, vec![1.0]);
        assert!(out.observations.is_empty(), "cam2 has never sent");
        assert!(out.drops.is_empty(), "and nothing is discarded yet");

        let out = push_f(&mut buf, "cam2", 10_000);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam2"].source, FrameSource::Live);
    }

    /// A recovering track goes straight back to `Live` — the placeholder is
    /// not sticky.
    #[test]
    fn omit_recovers_when_the_track_returns() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let fields = vec!["j1".to_string()];
        let mut buf = mk(&tracks, fields, stall_config(Some(50_000), StallPolicy::Omit));

        let _ = push_f(&mut buf, "cam1", 10_000);
        let _ = push_f(&mut buf, "cam2", 10_000);
        let _ = buf.push_state(10_000, vec![1.0]);

        let _ = push_f(&mut buf, "cam1", 70_000);
        assert!(buf.push_state(70_000, vec![2.0]).observations.is_empty());
        let out = push_f(&mut buf, "cam1", 130_000);
        assert_eq!(out.observations[0].frames["cam2"].source, FrameSource::Omitted);

        // cam2 comes back in range of the next moment.
        let _ = push_f(&mut buf, "cam1", 200_000);
        let _ = push_f(&mut buf, "cam2", 200_000);
        let out = buf.push_state(200_000, vec![3.0]);
        assert_eq!(out.observations.len(), 1);
        assert_eq!(out.observations[0].frames["cam2"].source, FrameSource::Live);
    }
}
