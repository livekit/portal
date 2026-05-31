use crate::codec::Codec;
use crate::dtype::DType;
use crate::types::{Role, SyncConfig};

/// Default JPEG quality for `add_video` when MJPEG is selected without an
/// explicit value. Tuned for inference workloads: visually near-lossless on
/// natural images, ~10-20x compression versus raw RGB.
pub const DEFAULT_MJPEG_QUALITY: u8 = 90;

/// Default H264 encoder bitrate ceiling (kbps) for `add_video` when no
/// explicit `max_bitrate_kbps` is given. 10 Mbps is a generous cap: the
/// encoder still picks a much lower operating bitrate from content. The cap
/// only exists so high-motion bursts don't force frame drops. Lower it to
/// hold a hard bandwidth budget; raise it to let the encoder spend more on
/// motion.
pub const DEFAULT_H264_MAX_BITRATE_KBPS: u32 = 10_000;

/// A single schema entry: field name plus declared on-wire dtype.
///
/// Named for parity with the UniFFI-facing `FieldSpec` record the
/// bindings expose. Tuple form `(name, dtype)` is still accepted by the
/// `add_*_typed` methods — `FieldSpec` is the self-documenting
/// alternative.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpec {
    pub name: String,
    pub dtype: DType,
}

impl FieldSpec {
    pub fn new(name: impl Into<String>, dtype: DType) -> Self {
        Self { name: name.into(), dtype }
    }
}

impl<S: Into<String>> From<(S, DType)> for FieldSpec {
    fn from((name, dtype): (S, DType)) -> Self {
        Self { name: name.into(), dtype }
    }
}

impl From<FieldSpec> for (String, DType) {
    fn from(f: FieldSpec) -> Self {
        (f.name, f.dtype)
    }
}

/// One byte-stream video track declaration: name, codec, and per-codec
/// quality.
///
/// These tracks bypass the WebRTC media path and ride a reliable byte-stream
/// channel instead. Each frame is encoded once on the sender (Raw / PNG /
/// MJPEG) and decoded back to RGB on the receiver. The user-facing API is
/// identical to WebRTC video — `send_video_frame` / `on_video_frame` /
/// `get_video_frame` — only the wire transport differs. Selected at config
/// time by passing a non-`H264` codec to `PortalConfig::add_video`.
///
/// `quality` is honored for `Mjpeg` (1..=100) and ignored for `Raw` and
/// `Png`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameVideoSpec {
    pub name: String,
    pub codec: Codec,
    pub quality: u8,
}

impl FrameVideoSpec {
    pub fn new(name: impl Into<String>, codec: Codec, quality: u8) -> Self {
        Self { name: name.into(), codec, quality }
    }
}

/// One WebRTC video track declaration: name, WebRTC codec, and an optional
/// encoder bitrate ceiling.
///
/// The WebRTC counterpart to `FrameVideoSpec`. These tracks ride the WebRTC
/// media path (RTP/SRTP). `codec` is always a WebRTC codec (`H264` / `Vp8` /
/// `Vp9` / `Av1` / `H265`) — `add_video` routes byte-stream codecs to
/// `FrameVideoSpec` instead. `max_bitrate_kbps` caps the encoder's peak rate
/// in kilobits per second; `None` means use `DEFAULT_H264_MAX_BITRATE_KBPS`.
/// The cap is a ceiling, not a target — libwebrtc still picks a lower
/// operating bitrate from content. Selected at config time by passing a
/// WebRTC codec to `PortalConfig::add_video`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackSpec {
    pub name: String,
    pub codec: Codec,
    pub max_bitrate_kbps: Option<u32>,
}

impl VideoTrackSpec {
    pub fn new(name: impl Into<String>, codec: Codec, max_bitrate_kbps: Option<u32>) -> Self {
        Self { name: name.into(), codec, max_bitrate_kbps }
    }
}

/// Schema for one named action chunk: a fixed-horizon batch of per-field
/// values that the operator publishes as a single packet.
///
/// Equivalent to a `[horizon, fields.len()]` tensor with per-field dtype.
/// Multiple chunks can be declared on a Portal — each is dispatched to the
/// right callback by its own schema fingerprint, so chunk names are unique
/// per Portal but cross-Portal collisions are impossible by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkSpec {
    pub name: String,
    pub horizon: u32,
    pub fields: Vec<FieldSpec>,
}

impl ChunkSpec {
    pub fn new(
        name: impl Into<String>,
        horizon: u32,
        fields: impl IntoIterator<Item = impl Into<FieldSpec>>,
    ) -> Self {
        Self {
            name: name.into(),
            horizon,
            fields: fields.into_iter().map(Into::into).collect(),
        }
    }
}

/// Configuration for a Portal session. Built incrementally before connecting.
#[derive(Debug, Clone)]
pub struct PortalConfig {
    pub(crate) session: String,
    pub(crate) role: Role,
    pub(crate) video_tracks: Vec<VideoTrackSpec>,
    pub(crate) frame_video_tracks: Vec<FrameVideoSpec>,
    pub(crate) state_schema: Vec<FieldSpec>,
    pub(crate) action_schema: Vec<FieldSpec>,
    pub(crate) action_chunks: Vec<ChunkSpec>,
    pub(crate) state_reliable: bool,
    pub(crate) action_reliable: bool,
    pub(crate) fps: u32,
    pub(crate) slack: u32,
    pub(crate) tolerance: f32,
    pub(crate) ping_ms: u64,
    pub(crate) reuse_stale_frames: bool,
    pub(crate) shared_key: Option<Vec<u8>>,
    /// Operator-side: subscribe to executed actions. Off by default —
    /// most operators are pure controllers and do not want the bandwidth
    /// or callback noise. Recorders, shadow eval policies, and live
    /// monitoring opt in. When on:
    ///   * `(Role::Operator, ACTION_TOPIC)` packets are deserialized and
    ///     fired through `on_action` / `get_action`, gated by
    ///     `sender == active_operator` (same gate the robot applies)
    ///   * `(Role::Operator, ACTION_CHUNK_TOPIC)` byte streams are read
    ///     and fired through `on_action_chunk` / `get_action_chunk`
    ///   * `send_action` / `send_action_chunk` echo a local copy after
    ///     publish when `local_identity == active_operator`, since
    ///     LiveKit does not fan out a publisher's own data packets
    pub(crate) action_subscription: bool,
}

impl PortalConfig {
    pub fn new(session: impl Into<String>, role: Role) -> Self {
        Self {
            session: session.into(),
            role,
            video_tracks: Vec::new(),
            frame_video_tracks: Vec::new(),
            state_schema: Vec::new(),
            action_schema: Vec::new(),
            action_chunks: Vec::new(),
            state_reliable: true,
            action_reliable: true,
            fps: 30,
            slack: 5,
            tolerance: 1.5,
            ping_ms: 1000,
            reuse_stale_frames: false,
            shared_key: None,
            action_subscription: false,
        }
    }

    /// Operator-side opt-in for receiving executed actions. Off by default.
    /// When on, the operator subscribes to actions and chunks from the
    /// active operator and gets a local echo of its own sends when active.
    /// Used by recorders, shadow eval policies, and monitoring UIs.
    /// No-op on the Robot side — the robot always processes actions.
    pub fn set_action_subscription(&mut self, enable: bool) {
        self.action_subscription = enable;
    }

    /// Whether action subscription is enabled for this config.
    pub fn action_subscription(&self) -> bool {
        self.action_subscription
    }

    /// Set a shared E2EE key. Both peers must call this with the same key
    /// before connecting. The key is used as a GCM-AES shared secret for all
    /// media tracks and data channels.
    pub fn set_e2ee_key(&mut self, key: Vec<u8>) {
        self.shared_key = Some(key);
    }

    /// Declare a video track.
    ///
    /// `codec` picks both the encoding and the wire transport:
    ///
    /// - The WebRTC codecs (`Codec::H264`, `Codec::Vp8`, `Codec::Vp9`,
    ///   `Codec::Av1`, `Codec::H265`) ride the WebRTC media path (RTP/SRTP, lossy,
    ///   best-effort delivery, lowest latency at scale). `quality` is
    ///   ignored — libwebrtc picks the operating bitrate. `max_bitrate_kbps`
    ///   caps the encoder's peak rate (a ceiling, not a target); `None` uses
    ///   `DEFAULT_H264_MAX_BITRATE_KBPS` (10 Mbps).
    /// - `Codec::Mjpeg`, `Codec::Png`, `Codec::Raw` ride a reliable
    ///   per-frame byte-stream channel. The receiver decodes back to RGB so
    ///   the user-facing `on_video_frame` / `get_video_frame` API is
    ///   identical to H264. `quality` is in `1..=100` for `Mjpeg` and
    ///   ignored for `Raw` / `Png`. Use `DEFAULT_MJPEG_QUALITY` (90) when
    ///   in doubt. `max_bitrate_kbps` is ignored for these codecs.
    ///
    /// `quality` and `max_bitrate_kbps` are independent per-codec knobs: H264
    /// honors bitrate and ignores quality, the byte-stream codecs honor
    /// quality and ignore bitrate.
    ///
    /// Track names must be unique across all `add_video` calls regardless
    /// of codec; a duplicate panics.
    ///
    /// **Byte-stream latency**: frames on the byte-stream path pay roughly
    /// `1 ms + 2 ms × ⌈size / BYTE_STREAM_CHUNK_SIZE⌉` per frame, set by
    /// the SCTP data channel drain rate (not Portal's encode cost). Pick a
    /// codec whose encoded size fits in one chunk for low-latency
    /// closed-loop work. MJPEG at 224×224 to 480p typically does. Raw at
    /// anything above ~70×70 spills into multiple chunks.
    pub fn add_video(
        &mut self,
        name: impl Into<String>,
        codec: Codec,
        quality: u8,
        max_bitrate_kbps: Option<u32>,
    ) {
        let name = name.into();
        assert!(
            !self.has_track(&name),
            "video track '{name}' already declared (each track name must be unique \
             across add_video calls)"
        );
        if codec == Codec::Mjpeg {
            assert!(
                (1..=100).contains(&quality),
                "MJPEG quality must be in 1..=100, got {quality}"
            );
        }
        if let Some(kbps) = max_bitrate_kbps {
            assert!(kbps > 0, "max_bitrate_kbps must be > 0, got {kbps}");
        }
        if codec.is_webrtc() {
            self.video_tracks.push(VideoTrackSpec::new(name, codec, max_bitrate_kbps));
        } else {
            self.frame_video_tracks.push(FrameVideoSpec::new(name, codec, quality));
        }
    }

    fn has_track(&self, name: &str) -> bool {
        self.video_tracks.iter().any(|s| s.name == name)
            || self.frame_video_tracks.iter().any(|s| s.name == name)
    }

    /// Declare state fields with per-field dtype. Order is significant and
    /// must match on both peers. Appends to any previous declaration.
    ///
    /// Accepts anything iterable yielding a `FieldSpec` or anything
    /// convertible to one — `&[(&str, DType)]`, `[FieldSpec, ...]`,
    /// `Vec<(String, DType)>`, mapped iterators.
    pub fn add_state_typed<F, I>(&mut self, schema: I)
    where
        F: Into<FieldSpec>,
        I: IntoIterator<Item = F>,
    {
        self.state_schema.extend(schema.into_iter().map(Into::into));
    }

    /// Declare action fields with per-field dtype. Order is significant and
    /// must match on both peers. Appends to any previous declaration.
    ///
    /// Same input flexibility as `add_state_typed`.
    pub fn add_action_typed<F, I>(&mut self, schema: I)
    where
        F: Into<FieldSpec>,
        I: IntoIterator<Item = F>,
    {
        self.action_schema.extend(schema.into_iter().map(Into::into));
    }

    /// Declare an action chunk: a named, fixed-horizon batch of typed
    /// per-field values published as one packet. Multiple chunks can be
    /// declared. Names must be unique within a Portal — a duplicate panics
    /// at config time so the bug doesn't surface as a silent late-bind
    /// dispatch ambiguity at receive time.
    ///
    /// Use this in place of repeated `send_action` calls when a policy
    /// emits a horizon of future actions per inference step (the standard
    /// VLA shape).
    pub fn add_action_chunk(
        &mut self,
        name: impl Into<String>,
        horizon: u32,
        fields: impl IntoIterator<Item = impl Into<FieldSpec>>,
    ) {
        assert!(horizon > 0, "action chunk horizon must be > 0");
        let spec = ChunkSpec::new(name, horizon, fields);
        assert!(
            !self.action_chunks.iter().any(|c| c.name == spec.name),
            "duplicate action chunk name '{}'",
            spec.name
        );
        self.action_chunks.push(spec);
    }

    /// Unified observation rate (set to the video capture rate if state and
    /// video differ). Drives `search_range = tolerance/fps`.
    pub fn set_fps(&mut self, fps: u32) {
        assert!(fps > 0, "fps must be > 0");
        self.fps = fps;
    }

    /// How far (in tick intervals at `fps`) a state may reach when matching
    /// a video frame. `search_range = tolerance / fps`.
    ///
    /// - `0.5` (tight): state only matches a frame within ±half a tick.
    ///   One lost frame → one dropped observation. Lowest misalignment risk.
    /// - `1.5` (default, widened): state matches its own frame, or falls
    ///   back to T±1 if its native frame was lost. Preserves observations
    ///   at the cost of occasional ±1-tick misalignment. A fair-share check
    ///   prevents an earlier state from stealing a frame closer to a later
    ///   state already in the buffer.
    /// - `> 2.0`: state may match T±2 frames. Higher recovery, higher
    ///   misalignment risk. Rarely worth it.
    ///
    /// Values must be in `(0, ∞)`. Defaults to `1.5`.
    pub fn set_tolerance(&mut self, ticks: f32) {
        assert!(ticks > 0.0, "tolerance must be > 0");
        self.tolerance = ticks;
    }

    /// Ticks of pipeline headroom — how much jitter, loss-detection latency,
    /// and consumer lag the pipeline tolerates before dropping. Applies to
    /// the per-track video sync buffer, the state sync buffer, and the
    /// pull-side observation buffer.
    pub fn set_slack(&mut self, ticks: u32) {
        assert!(ticks > 0, "slack must be > 0");
        self.slack = ticks;
    }

    pub fn set_state_reliable(&mut self, reliable: bool) {
        self.state_reliable = reliable;
    }

    pub fn set_action_reliable(&mut self, reliable: bool) {
        self.action_reliable = reliable;
    }

    /// RTT ping cadence. Set to `0` to disable active pinging on this side;
    /// the pong echo path remains active so the peer can still measure.
    pub fn set_ping_ms(&mut self, ms: u64) {
        self.ping_ms = ms;
    }

    /// When enabled, a state whose video match window has elapsed reuses
    /// the most recent already-emitted frame on that track instead of
    /// being dropped. Video "freezes" on the last good frame during loss
    /// while state keeps flowing — every state becomes an observation
    /// once every track has emitted at least once.
    ///
    /// Drops still happen in two cases: (1) a track that has not yet
    /// emitted its first frame (pre-first-emission, or after `clear()`
    /// resets the last-emitted slots) — either sync-fail on a
    /// past-horizon frame or state-buffer overflow, same as strict mode,
    /// and (2) state-buffer overflow itself, which remains a hard safety
    /// net against a fully halted video pipeline.
    ///
    /// Monitoring note: under reuse, `last_blocker_track` only updates
    /// during pre-first-emission and won't point at a silently frozen
    /// track. Use `stale_observations_emitted` as the freeze signal.
    /// `match_delta_us_p95` also becomes unbounded (stale deltas can be
    /// arbitrarily large), so alerts keyed on that metric need reshaping.
    ///
    /// Off by default, which preserves the strict drop-on-horizon policy.
    /// Turn this on for data collection or logging pipelines where
    /// losing a state is worse than a transient video freeze; leave it
    /// off for real-time control where a stale frame would misalign the
    /// perception/action loop.
    pub fn set_reuse_stale_frames(&mut self, enable: bool) {
        self.reuse_stale_frames = enable;
    }

    /// Declared WebRTC (H264) video tracks (name + optional bitrate cap), in
    /// declaration order.
    pub fn video_tracks(&self) -> &[VideoTrackSpec] {
        &self.video_tracks
    }

    /// WebRTC (H264) video track names, derived from `video_tracks`.
    pub fn video_track_names(&self) -> impl Iterator<Item = &str> {
        self.video_tracks.iter().map(|s| s.name.as_str())
    }

    /// Declared frame-video tracks (name + codec + quality), in declaration
    /// order.
    pub fn frame_video_tracks(&self) -> &[FrameVideoSpec] {
        &self.frame_video_tracks
    }

    /// Frame-video track names, derived from `frame_video_tracks`.
    pub fn frame_video_track_names(&self) -> impl Iterator<Item = &str> {
        self.frame_video_tracks.iter().map(|s| s.name.as_str())
    }

    /// Ordered state field names. Derived from `state_schema`; does not
    /// allocate.
    pub fn state_fields(&self) -> impl Iterator<Item = &str> {
        self.state_schema.iter().map(|f| f.name.as_str())
    }

    /// Ordered action field names. Derived from `action_schema`; does not
    /// allocate.
    pub fn action_fields(&self) -> impl Iterator<Item = &str> {
        self.action_schema.iter().map(|f| f.name.as_str())
    }

    /// Full state schema.
    pub fn state_schema(&self) -> &[FieldSpec] {
        &self.state_schema
    }

    /// Full action schema.
    pub fn action_schema(&self) -> &[FieldSpec] {
        &self.action_schema
    }

    /// All declared action chunks.
    pub fn action_chunks(&self) -> &[ChunkSpec] {
        &self.action_chunks
    }

    /// Derived sync config used internally by the sync buffer. Not public.
    pub(crate) fn sync_config(&self) -> SyncConfig {
        let search_range_us = (self.tolerance * 1_000_000.0 / self.fps as f32) as u64;
        SyncConfig {
            video_buffer_size: self.slack,
            state_buffer_size: self.slack,
            search_range_us,
            reuse_stale_frames: self.reuse_stale_frames,
        }
    }
}
