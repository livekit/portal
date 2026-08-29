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

use crate::codec::Codec;
use crate::dtype::DType;
use crate::types::{Role, StallBehavior, StallConfig, SyncConfig};
use std::collections::HashMap;

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

/// One WebRTC video track declaration: name, WebRTC codec, an optional
/// encoder bitrate ceiling, and the two encoder-behavior toggles.
///
/// The WebRTC counterpart to `FrameVideoSpec`. These tracks ride the WebRTC
/// media path (RTP/SRTP). `codec` is always a WebRTC codec (`H264` / `Vp8` /
/// `Vp9` / `Av1` / `H265`) — `add_video` routes byte-stream codecs to
/// `FrameVideoSpec` instead. `max_bitrate_kbps` caps the encoder's peak rate
/// in kilobits per second; `None` means use `DEFAULT_H264_MAX_BITRATE_KBPS`.
/// The cap is a ceiling, not a target — libwebrtc still picks a lower
/// operating bitrate from content. Selected at config time by passing a
/// WebRTC codec to `PortalConfig::add_video`.
///
/// `simulcast` and `screencast` both default to `false`. See
/// `PortalConfig::add_video` for what each one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackSpec {
    pub name: String,
    pub codec: Codec,
    pub max_bitrate_kbps: Option<u32>,
    /// Publish multiple spatial layers so the SFU can pick per subscriber.
    /// Costs encode CPU and only pays off with several subscribers on
    /// varied networks.
    pub simulcast: bool,
    /// Mark the source as screen content. Flips libwebrtc's degradation
    /// preference from `MAINTAIN_FRAMERATE` to `MAINTAIN_RESOLUTION`, so
    /// congestion and CPU pressure drop framerate instead of rescaling the
    /// frame.
    pub screencast: bool,
}

impl VideoTrackSpec {
    pub fn new(
        name: impl Into<String>,
        codec: Codec,
        max_bitrate_kbps: Option<u32>,
        simulcast: bool,
        screencast: bool,
    ) -> Self {
        Self { name: name.into(), codec, max_bitrate_kbps, simulcast, screencast }
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
        Self { name: name.into(), horizon, fields: fields.into_iter().map(Into::into).collect() }
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
    /// Deprecated alias, retained so existing callers keep working. Folded
    /// into the stall behavior in `sync_config()`; see `set_reuse_stale_frames`.
    pub(crate) reuse_stale_frames: bool,
    /// Default stall behavior for tracks with no per-track override.
    pub(crate) stall_behavior: StallBehavior,
    /// Default `max_lag` in milliseconds. `None` derives it from `slack` and
    /// `fps`, which reproduces the historical capacity-eviction timing.
    pub(crate) max_lag_ms: Option<u32>,
    /// Per-track overrides, keyed by track name. Each is independent: a
    /// track may override the policy, the lag budget, or both.
    pub(crate) track_stall_behavior: HashMap<String, StallBehavior>,
    pub(crate) track_max_lag_ms: HashMap<String, u32>,
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
            stall_behavior: StallBehavior::Drop,
            max_lag_ms: None,
            track_stall_behavior: HashMap::new(),
            track_max_lag_ms: HashMap::new(),
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
    /// `simulcast` and `screencast` apply to the WebRTC codecs only and are
    /// ignored for the byte-stream codecs. Both default to `false` when
    /// `None` is passed.
    ///
    /// - `simulcast` publishes several spatial layers at once so the SFU can
    ///   hand each subscriber the layer their link can carry. This costs
    ///   encode CPU per extra layer and only pays off when several operators
    ///   subscribe over links of differing quality. A single-operator teleop
    ///   session gains nothing from it.
    /// - `screencast` marks the source as screen content. libwebrtc picks its
    ///   degradation preference from this flag: camera content defaults to
    ///   `MAINTAIN_FRAMERATE`, which holds the frame rate and *rescales the
    ///   frame* whenever CPU or bandwidth gets tight. Screen content uses
    ///   `MAINTAIN_RESOLUTION` instead, which pins the frame geometry and
    ///   drops frames under the same pressure. Turn this on when a stable,
    ///   unchanging resolution matters more than smooth motion. A policy
    ///   consuming fixed-shape frames is the usual case.
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
        simulcast: Option<bool>,
        screencast: Option<bool>,
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
            self.video_tracks.push(VideoTrackSpec::new(
                name,
                codec,
                max_bitrate_kbps,
                simulcast.unwrap_or(false),
                screencast.unwrap_or(false),
            ));
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
    #[deprecated(
        since = "0.3.0",
        note = "use set_stall_behavior(StallBehavior::Freeze) — equivalent to this plus set_max_lag_ms(0)"
    )]
    pub fn set_reuse_stale_frames(&mut self, enable: bool) {
        self.reuse_stale_frames = enable;
    }

    /// How a moment is resolved when a video track goes silent past its
    /// `max_lag`. Applies to every track without a per-track override; see
    /// [`set_track_stall_behavior`](Self::set_track_stall_behavior).
    ///
    /// **Receiving side only.** Observations are assembled where they are
    /// consumed, so this is read on the `Operator` and is a no-op on a
    /// `Robot` config. Nothing about a stall crosses the wire: a silent track
    /// is, by definition, sending nothing, so the substitute frame is
    /// synthesized locally by the subscriber that noticed the gap.
    ///
    /// [`Drop`](StallBehavior::Drop) (the default) emits nothing for that
    /// moment — the state still reaches the drop callback, but the healthy
    /// tracks in it are discarded too, so an operator screen goes dark while
    /// one camera is down. [`Freeze`](StallBehavior::Freeze) keeps the last
    /// good frame on the silent track. [`Omit`](StallBehavior::Omit) emits a
    /// visible placeholder for it, so the healthy tracks keep flowing.
    ///
    /// Whatever the policy, the frame carries a [`FrameSource`] saying which
    /// of the three it was — check it before feeding an observation to a
    /// policy or writing it to a dataset.
    pub fn set_stall_behavior(&mut self, behavior: StallBehavior) {
        self.stall_behavior = behavior;
    }

    /// How far the fastest-advancing stream may run past a moment before it
    /// is resolved without a silent track, in milliseconds of **sender-clock
    /// time** — not wall-clock.
    ///
    /// This is a statement about stream position, not a stopwatch: it is
    /// evaluated when a packet arrives, so a burst of buffered frames can
    /// cross the threshold in far less real time, and if every stream goes
    /// quiet nothing fires at all (nothing is being emitted either). Keeping
    /// it on sender clocks is what makes sync decisions reproducible.
    ///
    /// Defaults to `slack / fps` — the point at which state-buffer capacity
    /// would have evicted the moment anyway — so the default timing matches
    /// the historical behavior. `0` resolves immediately, without waiting.
    ///
    /// **Receiving side only**, like [`set_stall_behavior`](Self::set_stall_behavior).
    pub fn set_max_lag_ms(&mut self, ms: u32) {
        self.max_lag_ms = Some(ms);
    }

    /// Per-track override for [`set_stall_behavior`](Self::set_stall_behavior). Use it
    /// when tracks differ in how load-bearing they are: a wrist camera a
    /// policy depends on may warrant `Drop` (no observation beats a wrong
    /// one), while a scene camera warrants `Omit` so its failure does not
    /// take the rest of the frame set down with it.
    pub fn set_track_stall_behavior(&mut self, track: impl Into<String>, behavior: StallBehavior) {
        self.track_stall_behavior.insert(track.into(), behavior);
    }

    /// Per-track override for [`set_max_lag_ms`](Self::set_max_lag_ms).
    pub fn set_track_max_lag_ms(&mut self, track: impl Into<String>, ms: u32) {
        self.track_max_lag_ms.insert(track.into(), ms);
    }

    /// Effective stall config for one track, after applying per-track
    /// overrides and the `reuse_stale_frames` alias over the defaults.
    pub fn stall_for(&self, track: &str) -> StallConfig {
        let base = self.default_stall();
        StallConfig {
            behavior: self.track_stall_behavior.get(track).copied().unwrap_or(base.behavior),
            max_lag_us: self
                .track_max_lag_ms
                .get(track)
                .map(|ms| Some(*ms as u64 * 1_000))
                .unwrap_or(base.max_lag_us),
        }
    }

    /// Stall config for a track with no override, after folding in the
    /// deprecated `reuse_stale_frames` alias. Kept separate from
    /// [`stall_for`](Self::stall_for) so the fallback never has to be spelled
    /// as a lookup for a track name that cannot exist.
    fn default_stall(&self) -> StallConfig {
        // The alias only applies while the modern knob sits at its default,
        // so an explicit `set_stall_behavior` always wins — a caller migrating one
        // setting at a time is never silently overridden by a leftover.
        let behavior = if self.reuse_stale_frames && self.stall_behavior == StallBehavior::Drop {
            StallBehavior::Freeze
        } else {
            self.stall_behavior
        };

        let max_lag_us = match self.max_lag_ms {
            Some(ms) => ms as u64 * 1_000,
            // `reuse_stale_frames` substituted immediately, with no wait.
            None if self.reuse_stale_frames => 0,
            // Otherwise: the point capacity eviction would have reached anyway,
            // so the default timing matches earlier versions.
            None => self.slack as u64 * 1_000_000 / self.fps.max(1) as u64,
        };

        StallConfig { max_lag_us: Some(max_lag_us), behavior }
    }

    /// Per-track stall config for every registered track, in the order the
    /// sync buffer indexes them.
    pub(crate) fn stall_configs(&self, track_names: &[String]) -> Vec<StallConfig> {
        track_names.iter().map(|n| self.stall_for(n)).collect()
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

    /// Session name this config was built for.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Role this config is pinned to.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Unified observation rate in Hz. Defaults to 30.
    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Ticks of pipeline headroom. Defaults to 5.
    pub fn slack(&self) -> u32 {
        self.slack
    }

    /// Frame-match window, in tick intervals at `fps`. Defaults to 1.5.
    pub fn tolerance(&self) -> f32 {
        self.tolerance
    }

    /// RTT ping cadence in milliseconds; `0` means active pinging is off.
    pub fn ping_ms(&self) -> u64 {
        self.ping_ms
    }

    /// Whether state packets are published on the reliable channel.
    pub fn state_reliable(&self) -> bool {
        self.state_reliable
    }

    /// Whether action packets are published on the reliable channel.
    pub fn action_reliable(&self) -> bool {
        self.action_reliable
    }

    /// Whether a state past its video match window reuses the last emitted
    /// frame instead of being dropped.
    pub fn reuse_stale_frames(&self) -> bool {
        self.reuse_stale_frames
    }

    /// Whether a shared E2EE key has been set. The key bytes are not
    /// readable back — this only reports presence.
    pub fn has_e2ee_key(&self) -> bool {
        self.shared_key.is_some()
    }

    /// Derived sync config used internally by the sync buffer. Not public.
    pub(crate) fn sync_config(&self) -> SyncConfig {
        let search_range_us = (self.tolerance * 1_000_000.0 / self.fps as f32) as u64;
        SyncConfig {
            video_buffer_size: self.slack,
            state_buffer_size: self.slack,
            search_range_us,
            default_stall: self.default_stall(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PortalConfig {
        PortalConfig::new("demo", Role::Robot)
    }

    /// The default budget is the point state-buffer capacity would have
    /// evicted the moment anyway (`slack / fps`), so the default timing
    /// matches the historical behavior rather than changing it.
    #[test]
    fn default_stall_is_drop_at_slack_over_fps() {
        let c = cfg(); // slack 5, fps 30
        let s = c.stall_for("cam1");
        assert_eq!(s.behavior, StallBehavior::Drop);
        assert_eq!(s.max_lag_us, Some(5 * 1_000_000 / 30));
    }

    #[test]
    fn default_budget_tracks_slack_and_fps() {
        let mut c = cfg();
        c.set_slack(10);
        c.set_fps(50);
        assert_eq!(c.stall_for("cam1").max_lag_us, Some(200_000));
    }

    /// `reuse_stale_frames` is exactly `Freeze` with a zero budget: resolve
    /// immediately, substituting the last good frame.
    #[test]
    fn reuse_stale_frames_aliases_to_freeze_with_zero_budget() {
        let mut c = cfg();
        #[allow(deprecated)]
        c.set_reuse_stale_frames(true);
        let s = c.stall_for("cam1");
        assert_eq!(s.behavior, StallBehavior::Freeze);
        assert_eq!(s.max_lag_us, Some(0));
    }

    /// An explicit modern knob always beats the deprecated alias, in both
    /// directions, so a caller migrating one setting at a time is never
    /// silently overridden by a leftover `reuse_stale_frames`.
    #[test]
    fn explicit_knobs_win_over_the_alias() {
        let mut c = cfg();
        #[allow(deprecated)]
        c.set_reuse_stale_frames(true);
        c.set_stall_behavior(StallBehavior::Omit);
        c.set_max_lag_ms(200);
        let s = c.stall_for("cam1");
        assert_eq!(s.behavior, StallBehavior::Omit);
        assert_eq!(s.max_lag_us, Some(200_000));
    }

    #[test]
    fn max_lag_ms_converts_to_micros() {
        let mut c = cfg();
        c.set_max_lag_ms(200);
        assert_eq!(c.stall_for("cam1").max_lag_us, Some(200_000));
        c.set_max_lag_ms(0);
        assert_eq!(c.stall_for("cam1").max_lag_us, Some(0), "0 resolves immediately");
    }

    /// Per-track overrides are independent: a track may override the policy,
    /// the budget, or both, and untouched tracks keep the defaults.
    #[test]
    fn per_track_overrides_apply_independently() {
        let mut c = cfg();
        c.set_stall_behavior(StallBehavior::Drop);
        c.set_max_lag_ms(100);
        c.set_track_stall_behavior("scene", StallBehavior::Omit);
        c.set_track_max_lag_ms("wrist", 20);

        let scene = c.stall_for("scene");
        assert_eq!(scene.behavior, StallBehavior::Omit, "behavior overridden");
        assert_eq!(scene.max_lag_us, Some(100_000), "budget inherited");

        let wrist = c.stall_for("wrist");
        assert_eq!(wrist.behavior, StallBehavior::Drop, "behavior inherited");
        assert_eq!(wrist.max_lag_us, Some(20_000), "budget overridden");

        let other = c.stall_for("other");
        assert_eq!(other.behavior, StallBehavior::Drop);
        assert_eq!(other.max_lag_us, Some(100_000));
    }

    /// A track literally named "" must resolve like any other track, not
    /// collide with the fallback. Guards the empty-string lookup the earlier
    /// implementation used to express "no override".
    #[test]
    fn empty_track_name_is_not_the_fallback() {
        let mut c = cfg();
        c.set_stall_behavior(StallBehavior::Drop);
        c.set_track_stall_behavior("", StallBehavior::Omit);
        assert_eq!(c.stall_for("").behavior, StallBehavior::Omit);
        assert_eq!(c.stall_for("cam1").behavior, StallBehavior::Drop, "fallback unaffected");
        assert_eq!(c.sync_config().default_stall.behavior, StallBehavior::Drop);
    }

    /// `stall_configs` resolves in the order the sync buffer indexes tracks,
    /// since the buffer addresses them positionally.
    #[test]
    fn stall_configs_follow_track_order() {
        let mut c = cfg();
        c.set_track_stall_behavior("b", StallBehavior::Freeze);
        let names = vec!["a".to_string(), "b".to_string()];
        let got = c.stall_configs(&names);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].behavior, StallBehavior::Drop);
        assert_eq!(got[1].behavior, StallBehavior::Freeze);
    }
}
