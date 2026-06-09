use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

/// Snapshot of portal metrics. Counters are cumulative since construction
/// (or the last `reset_metrics()` call); gauges reflect instantaneous state.
#[derive(Debug, Clone, Default)]
pub struct PortalMetrics {
    pub sync: SyncMetrics,
    pub transport: TransportMetrics,
    pub buffers: BufferMetrics,
    pub rtt: RttMetrics,
    pub policy: PolicyMetrics,
}

#[derive(Debug, Clone, Default)]
pub struct SyncMetrics {
    pub observations_emitted: u64,
    /// Subset of `observations_emitted` where at least one track contributed a
    /// stale, previously-emitted frame (reuse fallback). Always 0 unless
    /// `reuse_stale_frames` is enabled. A rising counter at steady observation
    /// rate signals a silently frozen video track.
    pub stale_observations_emitted: u64,
    pub states_dropped: u64,
    /// Worst per-track alignment `max_t |state_ts − frame_ts|` across the
    /// tracks in each emitted observation, over a rolling 256-sample window.
    pub match_delta_us_p50: Option<u64>,
    pub match_delta_us_p95: Option<u64>,
    /// Name of the most recent track that stalled sync. Sticky: only updates
    /// when a new block occurs, so it still tells you who was slow after
    /// things recover.
    pub last_blocker_track: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TransportMetrics {
    pub frames_sent: HashMap<String, u64>,
    pub frames_received: HashMap<String, u64>,
    /// Per-track count of frames the publisher dropped because its in-flight
    /// queue was at the cap (slow link, SFU backpressure). Currently only
    /// frame-video tracks can drop here — WebRTC frames flow through libwebrtc's
    /// own backpressure pipeline. A non-zero value at steady state means the
    /// publisher is offering frames faster than the link can ship them.
    pub frames_dropped_publisher_full: HashMap<String, u64>,
    /// Per-track cumulative bytes encoded and queued for send (full
    /// on-wire payload including the framing header). Frame-video tracks
    /// only — WebRTC frames are encoded by libwebrtc inside its own
    /// transport, so we cannot observe their byte count from here.
    /// Average frame size on the wire is `bytes_sent / frames_sent`.
    pub bytes_sent: HashMap<String, u64>,
    /// Per-track cumulative bytes received as on-wire payload (header +
    /// codec). Symmetric to `bytes_sent`. Frame-video only.
    pub bytes_received: HashMap<String, u64>,
    pub states_sent: u64,
    pub states_received: u64,
    pub actions_sent: u64,
    pub actions_received: u64,
    pub action_chunks_sent: u64,
    pub action_chunks_received: u64,
    /// Per-stream RFC 3550 inter-arrival jitter estimate (EWMA, α=1/16).
    pub frame_jitter_us: HashMap<String, u64>,
    pub state_jitter_us: u64,
    pub action_jitter_us: u64,
    pub action_chunk_jitter_us: u64,
}

/// End-to-end policy latency, measured from the observation timestamp the
/// peer correlates against (`in_reply_to_ts_us`) to the local receive
/// time of the resulting action or chunk. Both percentiles populate only
/// once at least one correlated action/chunk has been received.
#[derive(Debug, Clone, Default)]
pub struct PolicyMetrics {
    pub e2e_us_p50: Option<u64>,
    pub e2e_us_p95: Option<u64>,
    /// Cumulative count of correlated actions/chunks received — useful as
    /// a denominator if the user wants to know how many of the actions
    /// arriving carry timing data versus uncorrelated.
    pub correlated_received: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BufferMetrics {
    pub video_fill: HashMap<String, usize>,
    pub state_fill: usize,
    /// Per-video-track cumulative evictions from overflow.
    pub evictions: HashMap<String, u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RttMetrics {
    pub rtt_us_last: Option<u64>,
    pub rtt_us_mean: Option<u64>,
    pub rtt_us_p95: Option<u64>,
    pub pings_sent: u64,
    pub pongs_received: u64,
}

// --- Internal collectors ---

const SAMPLE_RING_CAP: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataStream {
    State,
    Action,
    Chunk,
}

pub(crate) struct MetricsRegistry {
    track_order: Vec<String>,
    per_track: HashMap<String, Arc<TrackMetrics>>,

    states_sent: AtomicU64,
    states_received: AtomicU64,
    actions_sent: AtomicU64,
    actions_received: AtomicU64,
    action_chunks_sent: AtomicU64,
    action_chunks_received: AtomicU64,
    state_jitter: Mutex<JitterState>,
    action_jitter: Mutex<JitterState>,
    chunk_jitter: Mutex<JitterState>,
    e2e_samples: Mutex<SampleRing>,
    correlated_received: AtomicU64,

    observations_emitted: AtomicU64,
    stale_observations_emitted: AtomicU64,
    states_dropped: AtomicU64,
    match_deltas: Mutex<SampleRing>,
    // Index into `track_order`; `usize::MAX` means "no blocker recorded".
    // Stored as an atomic so `record_blocker` allocates nothing on the hot path.
    last_blocker_track: AtomicUsize,

    rtt_samples: Mutex<SampleRing>,
    // 0 sentinel = "no sample yet"; samples of 0 are bumped to 1 on record.
    rtt_last: AtomicU64,
    pings_sent: AtomicU64,
    pongs_received: AtomicU64,
}

impl MetricsRegistry {
    pub fn new(video_tracks: &[String]) -> Self {
        let per_track: HashMap<String, Arc<TrackMetrics>> =
            video_tracks.iter().map(|n| (n.clone(), Arc::new(TrackMetrics::new()))).collect();
        Self {
            track_order: video_tracks.to_vec(),
            per_track,
            states_sent: AtomicU64::new(0),
            states_received: AtomicU64::new(0),
            actions_sent: AtomicU64::new(0),
            actions_received: AtomicU64::new(0),
            action_chunks_sent: AtomicU64::new(0),
            action_chunks_received: AtomicU64::new(0),
            state_jitter: Mutex::new(JitterState::default()),
            action_jitter: Mutex::new(JitterState::default()),
            chunk_jitter: Mutex::new(JitterState::default()),
            e2e_samples: Mutex::new(SampleRing::new(SAMPLE_RING_CAP)),
            correlated_received: AtomicU64::new(0),
            observations_emitted: AtomicU64::new(0),
            stale_observations_emitted: AtomicU64::new(0),
            states_dropped: AtomicU64::new(0),
            match_deltas: Mutex::new(SampleRing::new(SAMPLE_RING_CAP)),
            last_blocker_track: AtomicUsize::new(usize::MAX),
            rtt_samples: Mutex::new(SampleRing::new(SAMPLE_RING_CAP)),
            rtt_last: AtomicU64::new(0),
            pings_sent: AtomicU64::new(0),
            pongs_received: AtomicU64::new(0),
        }
    }

    pub fn track(&self, name: &str) -> Option<Arc<TrackMetrics>> {
        self.per_track.get(name).cloned()
    }

    pub fn bump_sent(&self, stream: DataStream) {
        match stream {
            DataStream::State => self.states_sent.fetch_add(1, Ordering::Relaxed),
            DataStream::Action => self.actions_sent.fetch_add(1, Ordering::Relaxed),
            DataStream::Chunk => self.action_chunks_sent.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Bump the appropriate received counter and feed the per-stream
    /// inter-arrival jitter sampler. Mirrors `bump_sent`'s dispatch shape
    /// so all three streams share one entry point.
    pub fn record_received(&self, stream: DataStream, send_ts_us: u64, recv_ts_us: u64) {
        match stream {
            DataStream::State => {
                self.states_received.fetch_add(1, Ordering::Relaxed);
                self.state_jitter.lock().sample(send_ts_us, recv_ts_us);
            }
            DataStream::Action => {
                self.actions_received.fetch_add(1, Ordering::Relaxed);
                self.action_jitter.lock().sample(send_ts_us, recv_ts_us);
            }
            DataStream::Chunk => {
                self.action_chunks_received.fetch_add(1, Ordering::Relaxed);
                self.chunk_jitter.lock().sample(send_ts_us, recv_ts_us);
            }
        }
    }

    /// Record an observation→action e2e latency from the action's
    /// `in_reply_to_ts_us`. Skips when `None` (uncorrelated publish) or
    /// when local receive time isn't strictly after `reply_ts` (rare
    /// clock skew on loopback runs — we'd rather drop the sample than
    /// log a wrapped u64).
    pub fn record_e2e(&self, in_reply_to_ts_us: Option<u64>, recv_ts_us: u64) {
        let Some(reply_ts) = in_reply_to_ts_us else { return };
        if recv_ts_us <= reply_ts {
            return;
        }
        self.correlated_received.fetch_add(1, Ordering::Relaxed);
        self.e2e_samples.lock().push(recv_ts_us - reply_ts);
    }

    pub fn record_observation(&self, worst_delta_us: u64) {
        self.observations_emitted.fetch_add(1, Ordering::Relaxed);
        self.match_deltas.lock().push(worst_delta_us);
    }

    pub fn record_stale_observation(&self) {
        self.stale_observations_emitted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_state_dropped(&self, n: u64) {
        self.states_dropped.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_blocker(&self, track_index: usize) {
        self.last_blocker_track.store(track_index, Ordering::Relaxed);
    }

    pub fn record_ping_sent(&self) {
        self.pings_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_rtt(&self, rtt_us: u64) {
        self.pongs_received.fetch_add(1, Ordering::Relaxed);
        self.rtt_samples.lock().push(rtt_us);
        self.rtt_last.store(rtt_us.max(1), Ordering::Relaxed);
    }

    pub fn snapshot(&self, video_fill: HashMap<String, usize>, state_fill: usize) -> PortalMetrics {
        let n = self.track_order.len();
        let mut frames_sent = HashMap::with_capacity(n);
        let mut frames_received = HashMap::with_capacity(n);
        let mut frame_jitter_us = HashMap::with_capacity(n);
        let mut evictions = HashMap::with_capacity(n);
        let mut frames_dropped_publisher_full = HashMap::with_capacity(n);
        let mut bytes_sent = HashMap::with_capacity(n);
        let mut bytes_received = HashMap::with_capacity(n);
        for name in &self.track_order {
            if let Some(t) = self.per_track.get(name) {
                frames_sent.insert(name.clone(), t.frames_sent.load(Ordering::Relaxed));
                frames_received.insert(name.clone(), t.frames_received.load(Ordering::Relaxed));
                frame_jitter_us.insert(name.clone(), t.jitter.lock().jitter_us);
                evictions.insert(name.clone(), t.evictions.load(Ordering::Relaxed));
                frames_dropped_publisher_full
                    .insert(name.clone(), t.frames_dropped_publisher_full.load(Ordering::Relaxed));
                bytes_sent.insert(name.clone(), t.bytes_sent.load(Ordering::Relaxed));
                bytes_received.insert(name.clone(), t.bytes_received.load(Ordering::Relaxed));
            }
        }

        let (match_p50, match_p95) = {
            let ring = self.match_deltas.lock();
            (ring.percentile(0.50), ring.percentile(0.95))
        };
        let (rtt_mean, rtt_p95) = {
            let ring = self.rtt_samples.lock();
            (ring.mean(), ring.percentile(0.95))
        };
        let rtt_last_raw = self.rtt_last.load(Ordering::Relaxed);
        let rtt_us_last = (rtt_last_raw != 0).then_some(rtt_last_raw);

        let (e2e_p50, e2e_p95) = {
            let ring = self.e2e_samples.lock();
            (ring.percentile(0.50), ring.percentile(0.95))
        };

        let last_blocker_track = {
            let idx = self.last_blocker_track.load(Ordering::Relaxed);
            (idx != usize::MAX).then(|| self.track_order.get(idx).cloned()).flatten()
        };

        PortalMetrics {
            sync: SyncMetrics {
                observations_emitted: self.observations_emitted.load(Ordering::Relaxed),
                stale_observations_emitted: self.stale_observations_emitted.load(Ordering::Relaxed),
                states_dropped: self.states_dropped.load(Ordering::Relaxed),
                match_delta_us_p50: match_p50,
                match_delta_us_p95: match_p95,
                last_blocker_track,
            },
            transport: TransportMetrics {
                frames_sent,
                frames_received,
                frames_dropped_publisher_full,
                bytes_sent,
                bytes_received,
                states_sent: self.states_sent.load(Ordering::Relaxed),
                states_received: self.states_received.load(Ordering::Relaxed),
                actions_sent: self.actions_sent.load(Ordering::Relaxed),
                actions_received: self.actions_received.load(Ordering::Relaxed),
                action_chunks_sent: self.action_chunks_sent.load(Ordering::Relaxed),
                action_chunks_received: self.action_chunks_received.load(Ordering::Relaxed),
                frame_jitter_us,
                state_jitter_us: self.state_jitter.lock().jitter_us,
                action_jitter_us: self.action_jitter.lock().jitter_us,
                action_chunk_jitter_us: self.chunk_jitter.lock().jitter_us,
            },
            buffers: BufferMetrics { video_fill, state_fill, evictions },
            rtt: RttMetrics {
                rtt_us_last,
                rtt_us_mean: rtt_mean,
                rtt_us_p95: rtt_p95,
                pings_sent: self.pings_sent.load(Ordering::Relaxed),
                pongs_received: self.pongs_received.load(Ordering::Relaxed),
            },
            policy: PolicyMetrics {
                e2e_us_p50: e2e_p50,
                e2e_us_p95: e2e_p95,
                correlated_received: self.correlated_received.load(Ordering::Relaxed),
            },
        }
    }

    pub fn reset(&self) {
        self.states_sent.store(0, Ordering::Relaxed);
        self.states_received.store(0, Ordering::Relaxed);
        self.actions_sent.store(0, Ordering::Relaxed);
        self.actions_received.store(0, Ordering::Relaxed);
        self.action_chunks_sent.store(0, Ordering::Relaxed);
        self.action_chunks_received.store(0, Ordering::Relaxed);
        *self.state_jitter.lock() = JitterState::default();
        *self.action_jitter.lock() = JitterState::default();
        *self.chunk_jitter.lock() = JitterState::default();
        self.e2e_samples.lock().clear();
        self.correlated_received.store(0, Ordering::Relaxed);
        self.observations_emitted.store(0, Ordering::Relaxed);
        self.stale_observations_emitted.store(0, Ordering::Relaxed);
        self.states_dropped.store(0, Ordering::Relaxed);
        self.match_deltas.lock().clear();
        self.last_blocker_track.store(usize::MAX, Ordering::Relaxed);
        self.rtt_samples.lock().clear();
        self.rtt_last.store(0, Ordering::Relaxed);
        self.pings_sent.store(0, Ordering::Relaxed);
        self.pongs_received.store(0, Ordering::Relaxed);
        for t in self.per_track.values() {
            t.reset();
        }
    }
}

pub(crate) struct TrackMetrics {
    pub frames_sent: AtomicU64,
    pub frames_received: AtomicU64,
    pub evictions: AtomicU64,
    pub frames_dropped_publisher_full: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub jitter: Mutex<JitterState>,
}

impl TrackMetrics {
    pub fn new() -> Self {
        Self {
            frames_sent: AtomicU64::new(0),
            frames_received: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            frames_dropped_publisher_full: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            jitter: Mutex::new(JitterState::default()),
        }
    }

    pub fn record_sent(&self) {
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Frame-video only: record the wire-payload byte count alongside
    /// the send count, so the avg frame size is derivable. WebRTC tracks
    /// don't surface this — libwebrtc owns their encode + transport.
    pub fn record_sent_bytes(&self, n: usize) {
        self.frames_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn record_received(&self, send_ts_us: u64, recv_ts_us: u64) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
        self.jitter.lock().sample(send_ts_us, recv_ts_us);
    }

    /// Frame-video only: like `record_received` but also bumps the wire
    /// byte counter.
    pub fn record_received_bytes(&self, send_ts_us: u64, recv_ts_us: u64, n: usize) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
        self.jitter.lock().sample(send_ts_us, recv_ts_us);
    }

    pub fn record_evictions(&self, n: u64) {
        self.evictions.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_dropped_publisher_full(&self) {
        self.frames_dropped_publisher_full.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        self.frames_sent.store(0, Ordering::Relaxed);
        self.frames_received.store(0, Ordering::Relaxed);
        self.evictions.store(0, Ordering::Relaxed);
        self.frames_dropped_publisher_full.store(0, Ordering::Relaxed);
        self.bytes_sent.store(0, Ordering::Relaxed);
        self.bytes_received.store(0, Ordering::Relaxed);
        *self.jitter.lock() = JitterState::default();
    }
}

#[derive(Default)]
pub(crate) struct JitterState {
    last_send_ts: Option<u64>,
    last_recv_ts: Option<u64>,
    pub jitter_us: u64,
}

impl JitterState {
    /// RFC 3550 inter-arrival jitter: `J = J + (|D| − J) / 16`, where
    /// `D = (recv − last_recv) − (send − last_send)`.
    pub fn sample(&mut self, send_ts_us: u64, recv_ts_us: u64) {
        if let (Some(ls), Some(lr)) = (self.last_send_ts, self.last_recv_ts) {
            let d_recv = recv_ts_us as i128 - lr as i128;
            let d_send = send_ts_us as i128 - ls as i128;
            let abs_d = (d_recv - d_send).unsigned_abs() as u64;
            let j = self.jitter_us as i128;
            let next = j + ((abs_d as i128) - j) / 16;
            self.jitter_us = next.max(0) as u64;
        }
        self.last_send_ts = Some(send_ts_us);
        self.last_recv_ts = Some(recv_ts_us);
    }
}

/// Fixed-capacity ring of u64 samples for bounded-memory percentile/mean.
/// Sort on read (O(N log N), N≤256) is cheap and keeps writes O(1).
pub(crate) struct SampleRing {
    buf: Vec<u64>,
    cap: usize,
    idx: usize,
    len: usize,
}

impl SampleRing {
    pub fn new(cap: usize) -> Self {
        Self { buf: vec![0; cap], cap, idx: 0, len: 0 }
    }

    pub fn push(&mut self, v: u64) {
        self.buf[self.idx] = v;
        self.idx = (self.idx + 1) % self.cap;
        if self.len < self.cap {
            self.len += 1;
        }
    }

    pub fn clear(&mut self) {
        self.idx = 0;
        self.len = 0;
    }

    pub fn percentile(&self, p: f64) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        let mut v: Vec<u64> = self.buf[..self.len].to_vec();
        v.sort_unstable();
        let rank = ((p * (self.len as f64 - 1.0)).round() as usize).min(self.len - 1);
        Some(v[rank])
    }

    pub fn mean(&self) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        let sum: u128 = self.buf[..self.len].iter().map(|v| *v as u128).sum();
        Some((sum / self.len as u128) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_percentile_and_mean() {
        let mut r = SampleRing::new(4);
        assert_eq!(r.percentile(0.5), None);
        assert_eq!(r.mean(), None);
        for v in [10u64, 20, 30, 40] {
            r.push(v);
        }
        assert_eq!(r.mean(), Some(25));
        assert_eq!(r.percentile(0.0), Some(10));
        assert_eq!(r.percentile(1.0), Some(40));

        // Overwrite wraps and bumps len stays at cap.
        r.push(100);
        assert_eq!(r.len, 4);
        assert_eq!(r.mean(), Some((20 + 30 + 40 + 100) / 4));
    }

    #[test]
    fn jitter_converges_toward_deviation() {
        let mut j = JitterState::default();
        // Perfect alignment: zero jitter forever.
        for i in 0..20u64 {
            j.sample(i * 1_000, i * 1_000);
        }
        assert_eq!(j.jitter_us, 0);

        // 1ms of variation on every other arrival.
        for i in 0..50u64 {
            let send = i * 1_000;
            let recv = send + if i % 2 == 0 { 0 } else { 1_000 };
            j.sample(send, recv);
        }
        // After many samples EWMA should be well below |D|=1000.
        assert!(j.jitter_us > 0 && j.jitter_us < 1_500);
    }

    #[test]
    fn registry_snapshot_picks_up_track_counters() {
        let tracks = vec!["cam1".to_string(), "cam2".to_string()];
        let reg = MetricsRegistry::new(&tracks);
        reg.track("cam1").unwrap().record_sent();
        reg.track("cam1").unwrap().record_sent();
        reg.track("cam2").unwrap().record_received(0, 100);

        let snap = reg.snapshot(HashMap::new(), 0);
        assert_eq!(snap.transport.frames_sent["cam1"], 2);
        assert_eq!(snap.transport.frames_sent["cam2"], 0);
        assert_eq!(snap.transport.frames_received["cam2"], 1);
    }

    #[test]
    fn registry_reset_clears_everything() {
        let tracks = vec!["cam1".to_string()];
        let reg = MetricsRegistry::new(&tracks);
        reg.track("cam1").unwrap().record_sent();
        reg.record_observation(500);
        reg.record_rtt(1000);
        reg.record_blocker(0);

        reg.reset();
        let snap = reg.snapshot(HashMap::new(), 0);
        assert_eq!(snap.transport.frames_sent["cam1"], 0);
        assert_eq!(snap.sync.observations_emitted, 0);
        assert!(snap.rtt.rtt_us_last.is_none());
        assert!(snap.sync.last_blocker_track.is_none());
    }
}
