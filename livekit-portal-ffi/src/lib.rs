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

//! UniFFI wrapper around `livekit-portal`.
//!
//! The core `livekit_portal::Portal` stays free of binding concerns; this
//! crate re-exposes it as a proc-macro-annotated UniFFI surface that
//! generates Python (and, later, Swift/Kotlin) bindings directly from Rust.
//!
//! Shape:
//!   * `PortalConfig` and `Portal` are `#[uniffi::Object]`s. Constructors and
//!     methods run through UniFFI's Arc-based lifecycle.
//!   * `RobotConfig` / `OperatorConfig` and `Robot` / `Operator` are thin
//!     role-split wrappers over those, so every binding inherits the split
//!     from UniFFI instead of hand-reimplementing it per host language.
//!   * Records (`VideoFrame`, `Observation`, `Action`, `State`, metrics)
//!     cross the boundary by value. Callbacks always own their payload.
//!   * `PortalCallbacks` is a foreign trait (`with_foreign`). The foreign
//!     side implements it once; the five closures registered into
//!     `core::Portal` fan out into its methods.
//!   * `connect`/`disconnect` are native `async` — no more request/async_id
//!     correlation.

#![recursion_limit = "256"]

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use livekit_portal as core;

uniffi::setup_scaffolding!();

/// Initialize `env_logger` when the cdylib is loaded to allow outputting logs via `RUST_LOG`.
#[ctor::ctor(unsafe)]
fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .try_init();
}

// ---------------------------------------------------------------------------
// Enums & records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Role {
    Robot,
    Operator,
    Observer,
}

impl From<Role> for core::Role {
    fn from(r: Role) -> Self {
        match r {
            Role::Robot => core::Role::Robot,
            Role::Operator => core::Role::Operator,
            Role::Observer => core::Role::Observer,
        }
    }
}

impl From<core::Role> for Role {
    fn from(r: core::Role) -> Self {
        match r {
            core::Role::Robot => Role::Robot,
            core::Role::Operator => Role::Operator,
            core::Role::Observer => Role::Observer,
        }
    }
}

/// Video codec. Selected per-track at config time via
/// `PortalConfig::add_video`. Codec choice picks both the encoding and the
/// wire transport: the WebRTC codecs (`H264` / `Vp8` / `Vp9` / `Av1` /
/// `H265`) ride the WebRTC media path; the rest ride a reliable per-frame
/// byte-stream channel. Mirrors `livekit_portal::Codec`.
///
/// **Foreign binding casing**: UniFFI emits enum variants in the host
/// language's idiomatic case. Python code uses `VideoCodec.H264` /
/// `VideoCodec.VP8` / `VideoCodec.RAW` / `VideoCodec.MJPEG` (UPPER), not
/// the Rust spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VideoCodec {
    /// WebRTC H.264. Real-time RTP/SRTP transport, lossy, best-effort.
    /// `quality` is ignored — libwebrtc picks the operating bitrate.
    H264,
    /// WebRTC VP8. Same media path and trade-offs as `H264`.
    Vp8,
    /// WebRTC VP9. Same media path as `H264`, better compression, higher CPU.
    Vp9,
    /// WebRTC AV1. Same media path as `H264`, best compression, highest CPU.
    Av1,
    /// WebRTC H.265 / HEVC. Same media path as `H264`. Support is platform-
    /// and build-dependent in libwebrtc.
    H265,
    /// Uncompressed RGB24. Largest payload, zero encode cost. Byte-stream
    /// transport.
    Raw,
    /// PNG, lossless. ~2-3x compression on natural images. Byte-stream
    /// transport.
    Png,
    /// Motion JPEG, lossy. ~10-20x compression at quality 90. Each frame is
    /// an independent JPEG so frame loss is contained. Byte-stream
    /// transport.
    Mjpeg,
}

impl From<VideoCodec> for core::Codec {
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::H264 => core::Codec::H264,
            VideoCodec::Vp8 => core::Codec::Vp8,
            VideoCodec::Vp9 => core::Codec::Vp9,
            VideoCodec::Av1 => core::Codec::Av1,
            VideoCodec::H265 => core::Codec::H265,
            VideoCodec::Raw => core::Codec::Raw,
            VideoCodec::Png => core::Codec::Png,
            VideoCodec::Mjpeg => core::Codec::Mjpeg,
        }
    }
}

impl From<core::Codec> for VideoCodec {
    fn from(c: core::Codec) -> Self {
        match c {
            core::Codec::H264 => VideoCodec::H264,
            core::Codec::Vp8 => VideoCodec::Vp8,
            core::Codec::Vp9 => VideoCodec::Vp9,
            core::Codec::Av1 => VideoCodec::Av1,
            core::Codec::H265 => VideoCodec::H265,
            core::Codec::Raw => VideoCodec::Raw,
            core::Codec::Png => VideoCodec::Png,
            core::Codec::Mjpeg => VideoCodec::Mjpeg,
        }
    }
}

/// Per-field dtype declared in state/action schemas. Mirrors
/// `livekit_portal::DType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DType {
    F64,
    F32,
    I32,
    I16,
    I8,
    U32,
    U16,
    U8,
    Bool,
}

impl From<DType> for core::DType {
    fn from(d: DType) -> Self {
        match d {
            DType::F64 => core::DType::F64,
            DType::F32 => core::DType::F32,
            DType::I32 => core::DType::I32,
            DType::I16 => core::DType::I16,
            DType::I8 => core::DType::I8,
            DType::U32 => core::DType::U32,
            DType::U16 => core::DType::U16,
            DType::U8 => core::DType::U8,
            DType::Bool => core::DType::Bool,
        }
    }
}

impl From<core::DType> for DType {
    fn from(d: core::DType) -> Self {
        match d {
            core::DType::F64 => DType::F64,
            core::DType::F32 => DType::F32,
            core::DType::I32 => DType::I32,
            core::DType::I16 => DType::I16,
            core::DType::I8 => DType::I8,
            core::DType::U32 => DType::U32,
            core::DType::U16 => DType::U16,
            core::DType::U8 => DType::U8,
            core::DType::Bool => DType::Bool,
        }
    }
}

/// One declared field: name + dtype. Crosses the FFI boundary as a record so
/// bindings can pass a list of these to `add_state_typed` / `add_action_typed`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FieldSpec {
    pub name: String,
    pub dtype: DType,
}

/// One declared video track: name, codec, and the per-codec options.
///
/// One record for every track regardless of transport, mirroring the core
/// `VideoTrackSpec` — `codec` says which transport it rides and therefore
/// which options apply. `quality` is meaningful for `VideoCodec.Mjpeg`
/// (1..=100); `max_bitrate_kbps`, `simulcast` and `screencast` are
/// meaningful for the WebRTC codecs. Each is ignored by the other transport
/// — the YAML loader rejects a mismatched option outright, while `add_video`
/// accepts and ignores it, so a spec may carry a value its codec never
/// reads. `quality` is the exception: it reads back as `0` on every codec
/// but `Mjpeg`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VideoTrackSpec {
    pub name: String,
    pub codec: VideoCodec,
    pub quality: u8,
    pub max_bitrate_kbps: Option<u32>,
    pub simulcast: bool,
    pub screencast: bool,
}

fn videotrackspec_from_core(s: &core::VideoTrackSpec) -> VideoTrackSpec {
    VideoTrackSpec {
        name: s.name.clone(),
        codec: s.codec.into(),
        quality: s.quality,
        max_bitrate_kbps: s.max_bitrate_kbps,
        simulcast: s.simulcast,
        screencast: s.screencast,
    }
}

/// Where the pixels in a delivered frame came from. Mirrors
/// `livekit_portal::FrameSource`.
///
/// Frames delivered on the raw video callbacks are always `Live`. The other
/// variants appear only on frames inside an `Observation`, when the sync
/// buffer had to resolve a track that went silent past its `max_lag`.
/// Check it before feeding an observation to a policy or a dataset — `Stale`
/// and `Omitted` frames are not measurements of the moment they hang off.
///
/// **Foreign binding casing**: UniFFI emits enum variants in the host
/// language's idiomatic case. Python code uses `FrameSource.LIVE` /
/// `FrameSource.STALE` / `FrameSource.OMITTED` (UPPER), not the Rust
/// spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FrameSource {
    /// A real frame matched to this state within the tolerance window.
    Live,
    /// A real frame from an earlier moment, reused because nothing in range
    /// arrived (`stall_behavior: freeze`). Age is
    /// `observation.timestamp_us - frame.timestamp_us`.
    Stale,
    /// A synthesized placeholder standing in for a silent track
    /// (`stall_behavior: omit`). Not camera output.
    Omitted,
}

impl From<core::FrameSource> for FrameSource {
    fn from(s: core::FrameSource) -> Self {
        match s {
            core::FrameSource::Live => FrameSource::Live,
            core::FrameSource::Stale => FrameSource::Stale,
            core::FrameSource::Omitted => FrameSource::Omitted,
        }
    }
}

/// How a moment is resolved when a video track goes silent past its
/// `max_lag`. Mirrors `livekit_portal::StallBehavior`.
///
/// **Foreign binding casing**: UniFFI emits enum variants in the host
/// language's idiomatic case. Python code uses `StallBehavior.DROP` /
/// `StallBehavior.FREEZE` / `StallBehavior.OMIT` (UPPER).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StallBehavior {
    /// Emit no observation. The state still reaches the drop callback, but
    /// the healthy tracks in that moment are discarded with it.
    Drop,
    /// Emit with the track's last good frame, tagged `FrameSource.STALE`.
    Freeze,
    /// Emit with a visible placeholder for the silent track, tagged
    /// `FrameSource.OMITTED`. The map key is still present.
    Omit,
}

impl From<StallBehavior> for core::StallBehavior {
    fn from(p: StallBehavior) -> Self {
        match p {
            StallBehavior::Drop => core::StallBehavior::Drop,
            StallBehavior::Freeze => core::StallBehavior::Freeze,
            StallBehavior::Omit => core::StallBehavior::Omit,
        }
    }
}

/// Which received actions reach `on_action` / `get_action` on an operator:
/// `NONE` (default), `ACTIVE` (the active operator's), or `ALL` (every
/// operator's, with `Action.active` marking the ones the gate dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Enum)]
pub enum ActionSubscription {
    #[default]
    None,
    Active,
    All,
}

impl From<ActionSubscription> for core::ActionSubscription {
    fn from(s: ActionSubscription) -> Self {
        match s {
            ActionSubscription::None => core::ActionSubscription::None,
            ActionSubscription::Active => core::ActionSubscription::Active,
            ActionSubscription::All => core::ActionSubscription::All,
        }
    }
}

impl From<core::ActionSubscription> for ActionSubscription {
    fn from(s: core::ActionSubscription) -> Self {
        match s {
            core::ActionSubscription::None => ActionSubscription::None,
            core::ActionSubscription::Active => ActionSubscription::Active,
            core::ActionSubscription::All => ActionSubscription::All,
        }
    }
}

/// Where `now_us()` comes from. `PORTAL` syncs to the robot's clock;
/// `SYSTEM` trusts the host clock, for hosts kept in step by PTP or GPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, uniffi::Enum)]
pub enum TimeSyncSource {
    #[default]
    Portal,
    System,
}

impl From<TimeSyncSource> for core::TimeSyncSource {
    fn from(s: TimeSyncSource) -> Self {
        match s {
            TimeSyncSource::Portal => core::TimeSyncSource::Portal,
            TimeSyncSource::System => core::TimeSyncSource::System,
        }
    }
}

impl From<core::TimeSyncSource> for TimeSyncSource {
    fn from(s: core::TimeSyncSource) -> Self {
        match s {
            core::TimeSyncSource::Portal => TimeSyncSource::Portal,
            core::TimeSyncSource::System => TimeSyncSource::System,
        }
    }
}

/// Decoded video frame. `data` is packed RGB24 (R,G,B byte order, `W*H*3`
/// bytes) on both sides — `send_video_frame` accepts RGB, and receive-side
/// frames are color-converted from I420 (WebRTC) or codec-decoded (frame
/// video) back to RGB before delivery.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub timestamp_us: u64,
    /// Whether these pixels are a live match, a reused earlier frame, or a
    /// synthesized placeholder. Always `Live` outside of `Observation`.
    pub source: FrameSource,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Observation {
    pub timestamp_us: u64,
    pub state: HashMap<String, f64>,
    pub frames: HashMap<String, VideoFrame>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Action {
    pub values: HashMap<String, f64>,
    pub timestamp_us: u64,
    /// Sender-side observation timestamp this action was produced from,
    /// or `None` for unsolicited publishes.
    pub in_reply_to_ts_us: Option<u64>,
    /// Identity of the operator that produced this action, captured at
    /// the active-operator gate (or, for the local echo path, the
    /// publisher's own identity).
    pub sender: String,
    /// Whether `sender` was the active operator at gate time. `false` marks
    /// a shadow action, seen only with `ActionSubscription.ALL`.
    pub active: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct State {
    pub values: HashMap<String, f64>,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct SyncMetrics {
    pub observations_emitted: u64,
    pub stale_observations_emitted: u64,
    pub states_dropped: u64,
    /// Per-track count of synthesized placeholder frames emitted under
    /// `stall_behavior: omit` — that track is silent and its moments are being kept
    /// alive with a stand-in. The frames carry `FrameSource.OMITTED`.
    pub frames_omitted: HashMap<String, u64>,
    pub match_delta_us_p50: Option<u64>,
    pub match_delta_us_p95: Option<u64>,
    pub last_blocker_track: Option<String>,
}

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct TransportMetrics {
    pub frames_sent: HashMap<String, u64>,
    pub frames_received: HashMap<String, u64>,
    /// Per-track count of frames the publisher dropped because its in-flight
    /// queue was at the cap. Frame-video tracks only — WebRTC frames flow
    /// through libwebrtc's own backpressure pipeline. Non-zero at steady
    /// state means the publisher is offering frames faster than the link
    /// can ship them.
    pub frames_dropped_publisher_full: HashMap<String, u64>,
    /// Per-track cumulative on-wire bytes sent (header + codec payload).
    /// Frame-video only. Average frame size = `bytes_sent / frames_sent`.
    pub bytes_sent: HashMap<String, u64>,
    /// Per-track cumulative on-wire bytes received. Frame-video only.
    pub bytes_received: HashMap<String, u64>,
    pub states_sent: u64,
    pub states_received: u64,
    pub actions_sent: u64,
    pub actions_received: u64,
    pub frame_jitter_us: HashMap<String, u64>,
    pub state_jitter_us: u64,
    pub action_jitter_us: u64,
}

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct PolicyMetrics {
    pub e2e_us_p50: Option<u64>,
    pub e2e_us_p95: Option<u64>,
    pub correlated_received: u64,
}

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct BufferMetrics {
    pub video_fill: HashMap<String, u64>,
    pub state_fill: u64,
    pub evictions: HashMap<String, u64>,
}

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct RttMetrics {
    pub rtt_us_last: Option<u64>,
    pub rtt_us_mean: Option<u64>,
    pub rtt_us_p95: Option<u64>,
    pub pings_sent: u64,
    pub pongs_received: u64,
}

/// Clock sync with the robot. `reset_metrics()` only zeroes the counters.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct TimeSyncMetrics {
    pub source: TimeSyncSource,
    pub synced: bool,
    pub offset_us: i64,
    pub uncertainty_us: Option<u64>,
    pub measured_offset_us: Option<i64>,
    pub resyncs: u64,
    pub samples_rejected: u64,
}

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct PortalMetrics {
    pub sync: SyncMetrics,
    pub transport: TransportMetrics,
    pub buffers: BufferMetrics,
    pub rtt: RttMetrics,
    pub time_sync: TimeSyncMetrics,
    pub policy: PolicyMetrics,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum PortalError {
    #[error("room error: {0}")]
    Room(String),

    #[error("portal is already connected")]
    AlreadyConnected,

    #[error("portal is not connected")]
    NotConnected,

    #[error("no peer in the room")]
    NoPeer,

    #[error("room has multiple remote participants; pass destination explicitly")]
    AmbiguousPeer,

    #[error("unknown video track: {0}")]
    UnknownVideoTrack(String),

    #[error("wrong frame size: expected {expected} bytes, got {got}")]
    WrongFrameSize { expected: u64, got: u64 },

    #[error("invalid frame dimensions: {width}x{height} (must both be even)")]
    InvalidFrameDimensions { width: u32, height: u32 },

    #[error("deserialization error: {0}")]
    Deserialization(String),

    #[error("frame codec error: {0}")]
    Codec(String),

    #[error("operation not available for role {0:?}")]
    WrongRole(Role),

    #[error("observation sync is off on this peer")]
    ObservationSyncDisabled,

    #[error("field '{field}' declared as {expected:?} but sent as {got}")]
    DtypeMismatch { field: String, expected: DType, got: String },

    #[error("rpc error {code}: {message}")]
    Rpc { code: u32, message: String, data: Option<String> },
}

impl From<core::PortalError> for PortalError {
    fn from(e: core::PortalError) -> Self {
        match e {
            core::PortalError::Room(s) => PortalError::Room(s),
            core::PortalError::AlreadyConnected => PortalError::AlreadyConnected,
            core::PortalError::NotConnected => PortalError::NotConnected,
            core::PortalError::NoPeer => PortalError::NoPeer,
            core::PortalError::AmbiguousPeer => PortalError::AmbiguousPeer,
            core::PortalError::UnknownVideoTrack { name } => PortalError::UnknownVideoTrack(name),
            core::PortalError::WrongFrameSize { expected, got } => {
                PortalError::WrongFrameSize { expected: expected as u64, got: got as u64 }
            }
            core::PortalError::InvalidFrameDimensions { width, height } => {
                PortalError::InvalidFrameDimensions { width, height }
            }
            core::PortalError::Deserialization(s) => PortalError::Deserialization(s),
            core::PortalError::Codec(s) => PortalError::Codec(s),
            core::PortalError::WrongRole(r) => PortalError::WrongRole(r.into()),
            core::PortalError::ObservationSyncDisabled => PortalError::ObservationSyncDisabled,
            core::PortalError::DtypeMismatch { field, expected, got } => {
                PortalError::DtypeMismatch {
                    field,
                    expected: expected.into(),
                    got: got.to_string(),
                }
            }
            core::PortalError::Rpc(e) => {
                PortalError::Rpc { code: e.code, message: e.message, data: e.data }
            }
        }
    }
}

pub type PortalResult<T> = Result<T, PortalError>;

/// Errors raised by `PortalConfig::from_yaml_str`. Mirrors
/// `livekit_portal::ConfigFileError` and is exposed as its own UniFFI
/// error type so bindings can catch YAML problems separately from
/// runtime portal failures.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum ConfigFileError {
    #[error("yaml parse error: {0}")]
    Parse(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("unsupported config-file version {got}; this build supports version {supported}")]
    UnsupportedVersion { got: u32, supported: u32 },
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error(
        "`action_chunks` is no longer supported: action chunks were removed in v0.3. \
         Declare the per-step fields under `action` and call `send_action` once per control tick"
    )]
    ActionChunksRemoved,
}

impl From<core::ConfigFileError> for ConfigFileError {
    fn from(e: core::ConfigFileError) -> Self {
        match e {
            core::ConfigFileError::Parse(s) => ConfigFileError::Parse(s),
            core::ConfigFileError::Io(e) => ConfigFileError::Io(e.to_string()),
            core::ConfigFileError::UnsupportedVersion { got, supported } => {
                ConfigFileError::UnsupportedVersion { got, supported }
            }
            core::ConfigFileError::Invalid(s) => ConfigFileError::Invalid(s),
            core::ConfigFileError::ActionChunksRemoved => ConfigFileError::ActionChunksRemoved,
        }
    }
}

// ---------------------------------------------------------------------------
// RPC types
// ---------------------------------------------------------------------------

/// Handler-side view of an incoming RPC invocation.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RpcInvocationData {
    pub request_id: String,
    pub caller_identity: String,
    pub payload: String,
    pub response_timeout_ms: u64,
}

impl From<core::RpcInvocationData> for RpcInvocationData {
    fn from(d: core::RpcInvocationData) -> Self {
        Self {
            request_id: d.request_id,
            caller_identity: d.caller_identity,
            payload: d.payload,
            response_timeout_ms: d.response_timeout.as_millis() as u64,
        }
    }
}

/// Error raised by an RPC handler or returned from `perform_rpc`. A
/// single-variant enum to satisfy UniFFI (which requires errors to be
/// enums); foreign handlers raise `RpcError.Error(code=..., message=...,
/// data=...)` to signal failure.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum RpcError {
    #[error("rpc error {code}: {message}")]
    Error { code: u32, message: String, data: Option<String> },
}

impl From<core::RpcError> for RpcError {
    fn from(e: core::RpcError) -> Self {
        RpcError::Error { code: e.code, message: e.message, data: e.data }
    }
}

impl From<RpcError> for core::RpcError {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Error { code, message, data } => core::RpcError::new(code, message, data),
        }
    }
}

/// Foreign-implemented handler for a single RPC method.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait RpcHandler: Send + Sync {
    async fn handle(&self, data: RpcInvocationData) -> Result<String, RpcError>;
}

// ---------------------------------------------------------------------------
// Foreign callback trait — the five push events plus the drop notification.
// The foreign side implements this once per `Portal`.
// ---------------------------------------------------------------------------

#[uniffi::export(with_foreign)]
pub trait PortalCallbacks: Send + Sync {
    fn on_action(&self, action: Action);
    fn on_state(&self, state: State);
    fn on_observation(&self, observation: Observation);
    fn on_video_frame(&self, track_name: String, frame: VideoFrame);
    fn on_drop(&self, dropped: Vec<HashMap<String, f64>>);
    /// Fires when an operator joins the room (post role-attribute discovery).
    fn on_operator_joined(&self, identity: String);
    /// Fires when an operator leaves the room. The robot's
    /// `active_operator` pointer is **not** auto-cleared on disconnect.
    fn on_operator_left(&self, identity: String);
    /// Fires when the robot's `active_operator` attribute changes (or, on
    /// the Robot side, when the local pointer is updated). Empty string
    /// means the pointer was cleared.
    fn on_active_operator_changed(&self, identity: Option<String>);
    /// First sync with the robot's clock, and after each resync.
    fn on_time_synced(&self);
}

// ---------------------------------------------------------------------------
// PortalConfig
// ---------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct PortalConfig {
    inner: Mutex<core::PortalConfig>,
}

#[uniffi::export]
impl PortalConfig {
    #[uniffi::constructor]
    pub fn new(session: String, role: Role) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(core::PortalConfig::new(session, role.into())) })
    }

    /// Build a `PortalConfig` from a YAML string. The file describes the
    /// shareable wire contract (schemas, video tracks, sync knobs);
    /// `session` and `role` are supplied here because they're per-process
    /// identity. The shared E2EE key, when used, must be applied with
    /// `set_e2ee_key` after loading.
    #[uniffi::constructor]
    pub fn from_yaml_str(
        yaml: String,
        session: String,
        role: Role,
    ) -> Result<Arc<Self>, ConfigFileError> {
        let cfg = core::PortalConfig::from_yaml_str(&yaml, session, role.into())?;
        Ok(Arc::new(Self { inner: Mutex::new(cfg) }))
    }

    /// Declare a video track. `codec` picks both the encoding and the wire
    /// transport: `H264` rides the WebRTC media path; `Mjpeg`, `Png`, and
    /// `Raw` ride a reliable per-frame byte-stream channel and the
    /// receiver decodes back to RGB so the user-facing frame API is
    /// identical. `quality` is `1..=100` for `Mjpeg` and ignored for
    /// `H264` / `Raw` / `Png`. `max_bitrate_kbps` caps the H264 encoder's
    /// peak rate (a ceiling, not a target); `None` uses the default 10 Mbps.
    /// It is ignored for the byte-stream codecs.
    ///
    /// `simulcast` publishes several spatial layers so the SFU can choose per
    /// subscriber, at the cost of encode CPU per layer. `screencast` marks the
    /// source as screen content, which pins the resolution and drops frames
    /// under CPU or bandwidth pressure instead of rescaling the frame. Both
    /// apply to the WebRTC codecs only and default to `false`.
    /// `stall_behavior` and `max_lag_ms` are per-track overrides of
    /// [`set_stall_behavior`] / [`set_max_lag_ms`], applied at the declaration
    /// site so a track's whole configuration reads in one place. `None` on
    /// either inherits the config-wide default. Both are read on the receiving
    /// side; see `set_stall_behavior`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_video(
        &self,
        name: String,
        codec: VideoCodec,
        quality: u8,
        max_bitrate_kbps: Option<u32>,
        simulcast: Option<bool>,
        screencast: Option<bool>,
        stall_behavior: Option<StallBehavior>,
        max_lag_ms: Option<u32>,
    ) {
        let mut cfg = self.inner.lock();
        cfg.add_video(name.clone(), codec.into(), quality, max_bitrate_kbps, simulcast, screencast);
        if let Some(b) = stall_behavior {
            cfg.set_track_stall_behavior(name.clone(), b.into());
        }
        if let Some(ms) = max_lag_ms {
            cfg.set_track_max_lag_ms(name, ms);
        }
    }

    pub fn add_state_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.lock().add_state_typed(schema.into_iter().map(|f| (f.name, f.dtype.into())));
    }

    pub fn add_action_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.lock().add_action_typed(schema.into_iter().map(|f| (f.name, f.dtype.into())));
    }

    pub fn set_fps(&self, fps: u32) {
        self.inner.lock().set_fps(fps);
    }

    pub fn set_slack(&self, ticks: u32) {
        self.inner.lock().set_slack(ticks);
    }

    pub fn set_tolerance(&self, ticks: f32) {
        self.inner.lock().set_tolerance(ticks);
    }

    pub fn set_state_reliable(&self, reliable: bool) {
        self.inner.lock().set_state_reliable(reliable);
    }

    pub fn set_action_reliable(&self, reliable: bool) {
        self.inner.lock().set_action_reliable(reliable);
    }

    /// Test hook: shifts this peer's local clock.
    pub fn set_clock_skew_us(&self, skew_us: i64) {
        self.inner.lock().set_clock_skew_us(skew_us);
    }

    pub fn set_time_sync_source(&self, source: TimeSyncSource) {
        self.inner.lock().set_time_sync_source(source.into());
    }

    pub fn time_sync_source(&self) -> TimeSyncSource {
        self.inner.lock().time_sync_source().into()
    }

    /// Operator-side: bundle state and frames into observations. On by
    /// default for operators.
    pub fn set_observation_sync(&self, enable: bool) {
        self.inner.lock().set_observation_sync(enable);
    }

    pub fn observation_sync(&self) -> bool {
        self.inner.lock().observation_sync()
    }

    #[allow(deprecated)]
    pub fn set_reuse_stale_frames(&self, enable: bool) {
        self.inner.lock().set_reuse_stale_frames(enable);
    }

    /// How a moment is resolved when a video track goes silent past its
    /// `max_lag`. Applies to tracks without a per-track override.
    pub fn set_stall_behavior(&self, behavior: StallBehavior) {
        self.inner.lock().set_stall_behavior(behavior.into());
    }

    /// How far the fastest-advancing stream may run past a moment before it
    /// resolves without a silent track, in milliseconds of sender-clock time
    /// (not wall-clock). Defaults to `slack / fps`; `0` resolves immediately.
    pub fn set_max_lag_ms(&self, ms: u32) {
        self.inner.lock().set_max_lag_ms(ms);
    }

    /// Per-track override for `set_stall_behavior`.
    pub fn set_track_stall_behavior(&self, track: String, behavior: StallBehavior) {
        self.inner.lock().set_track_stall_behavior(track, behavior.into());
    }

    /// Per-track override for `set_max_lag_ms`.
    pub fn set_track_max_lag_ms(&self, track: String, ms: u32) {
        self.inner.lock().set_track_max_lag_ms(track, ms);
    }

    pub fn set_e2ee_key(&self, key: Vec<u8>) {
        self.inner.lock().set_e2ee_key(key);
    }

    /// Operator-side: which received actions reach `on_action` /
    /// `get_action`. No-op on the Robot side.
    pub fn set_action_subscription(&self, subscription: ActionSubscription) {
        self.inner.lock().set_action_subscription(subscription.into());
    }

    /// Names of every declared video track, in declaration order, whatever
    /// transport its codec selects.
    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.lock().video_track_names().map(String::from).collect()
    }

    /// Every declared video track with its codec and options, in declaration
    /// order. The full readback of what `add_video` was given.
    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.lock().video_tracks().iter().map(videotrackspec_from_core).collect()
    }

    /// The byte-stream subset (Raw / Png / Mjpeg) of `video_track_specs`.
    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.lock().frame_video_tracks().map(videotrackspec_from_core).collect()
    }

    /// Declared state schema, in declaration order.
    pub fn state_schema(&self) -> Vec<FieldSpec> {
        self.inner
            .lock()
            .state_schema()
            .iter()
            .map(|f| FieldSpec { name: f.name.clone(), dtype: f.dtype.into() })
            .collect()
    }

    /// Declared action schema, in declaration order.
    pub fn action_schema(&self) -> Vec<FieldSpec> {
        self.inner
            .lock()
            .action_schema()
            .iter()
            .map(|f| FieldSpec { name: f.name.clone(), dtype: f.dtype.into() })
            .collect()
    }

    /// Session name this config was built for.
    pub fn session(&self) -> String {
        self.inner.lock().session().to_string()
    }

    /// Role this config is pinned to.
    pub fn role(&self) -> Role {
        self.inner.lock().role().into()
    }

    /// Unified observation rate in Hz. Defaults to 30.
    pub fn fps(&self) -> u32 {
        self.inner.lock().fps()
    }

    /// Ticks of pipeline headroom. Defaults to 5.
    pub fn slack(&self) -> u32 {
        self.inner.lock().slack()
    }

    /// Frame-match window, in tick intervals at `fps`. Defaults to 1.5.
    pub fn tolerance(&self) -> f32 {
        self.inner.lock().tolerance()
    }

    /// Whether state packets are published on the reliable channel.
    pub fn state_reliable(&self) -> bool {
        self.inner.lock().state_reliable()
    }

    /// Whether action packets are published on the reliable channel.
    pub fn action_reliable(&self) -> bool {
        self.inner.lock().action_reliable()
    }

    /// Whether a state past its video match window reuses the last emitted
    /// frame instead of being dropped.
    #[allow(deprecated)]
    pub fn reuse_stale_frames(&self) -> bool {
        self.inner.lock().reuse_stale_frames()
    }

    /// Whether action subscription is enabled (operator-side opt-in).
    pub fn action_subscription(&self) -> ActionSubscription {
        self.inner.lock().action_subscription().into()
    }

    /// Whether a shared E2EE key has been set. The key bytes are not
    /// readable back — this only reports presence.
    pub fn has_e2ee_key(&self) -> bool {
        self.inner.lock().has_e2ee_key()
    }
}

// ---------------------------------------------------------------------------
// Portal
// ---------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct Portal {
    inner: core::Portal,
    // Held only to keep the foreign trait object alive for the lifetime of
    // the Portal — core::Portal's closures already own their own `Arc` clones.
    _callbacks: Arc<dyn PortalCallbacks>,
    state_fields: Vec<String>,
    action_fields: Vec<String>,
    video_tracks: Vec<String>,
    video_track_specs: Vec<VideoTrackSpec>,
}

#[uniffi::export(async_runtime = "tokio")]
impl Portal {
    /// Construct a Portal from a built config. Callbacks must be passed at
    /// construction — `livekit_portal::Portal` registers them internally and
    /// there's no re-register-later escape hatch on the core side.
    #[uniffi::constructor]
    pub fn new(config: Arc<PortalConfig>, callbacks: Arc<dyn PortalCallbacks>) -> Arc<Self> {
        let cfg = config.inner.lock().clone();
        let state_fields: Vec<String> = cfg.state_fields().map(String::from).collect();
        let action_fields: Vec<String> = cfg.action_fields().map(String::from).collect();
        let video_tracks: Vec<String> = cfg.video_track_names().map(String::from).collect();
        let video_track_specs: Vec<VideoTrackSpec> =
            cfg.video_tracks().iter().map(videotrackspec_from_core).collect();

        let inner = core::Portal::new(cfg);

        let cb = callbacks.clone();
        inner.on_action(move |action| {
            // Cross the FFI boundary with `raw_values` — the lossless f64
            // view. Foreign bindings (Python) re-cast to typed values in
            // their own record using the schema they mirror.
            cb.on_action(Action {
                values: action.raw_values.clone(),
                timestamp_us: action.timestamp_us,
                in_reply_to_ts_us: action.in_reply_to_ts_us,
                sender: action.sender.clone(),
                active: action.active,
            });
        });
        let cb = callbacks.clone();
        inner.on_state(move |state| {
            cb.on_state(State {
                values: state.raw_values.clone(),
                timestamp_us: state.timestamp_us,
            });
        });
        if inner.observation_sync() {
            let cb = callbacks.clone();
            inner
                .on_observation(move |obs| {
                    cb.on_observation(observation_from_core(obs));
                })
                .expect("observation sync is on");
        }
        let cb = callbacks.clone();
        inner.on_drop(move |dropped| {
            // Cross with raw f64 maps. Python wraps to typed on receipt.
            let raw: Vec<HashMap<String, f64>> = dropped
                .into_iter()
                .map(|m| m.into_iter().map(|(k, v)| (k, v.as_f64())).collect())
                .collect();
            cb.on_drop(raw);
        });
        // Register `on_video_frame` for every declared track regardless of
        // transport. Frame-video tracks share the same `VideoTrackSlots` map
        // with WebRTC tracks on the core side, so a single registration
        // surface works for both — the foreign side only sees one
        // `on_video_frame(track, frame)` event stream per Portal.
        for track in &video_tracks {
            let cb = callbacks.clone();
            let track_name = track.clone();
            inner.on_video_frame(track, move |_name, frame| {
                cb.on_video_frame(track_name.clone(), frame_from_core(frame));
            });
        }

        let cb = callbacks.clone();
        inner.on_operator_joined(move |id| {
            cb.on_operator_joined(id.to_string());
        });
        let cb = callbacks.clone();
        inner.on_operator_left(move |id| {
            cb.on_operator_left(id.to_string());
        });
        let cb = callbacks.clone();
        inner.on_active_operator_changed(move |id| {
            cb.on_active_operator_changed(id.map(|s| s.to_string()));
        });
        let cb = callbacks.clone();
        inner.on_time_synced(move || cb.on_time_synced());

        Arc::new(Self {
            inner,
            _callbacks: callbacks,
            state_fields,
            action_fields,
            video_tracks,
            video_track_specs,
        })
    }

    pub async fn connect(&self, url: String, token: String) -> PortalResult<()> {
        self.inner.connect(&url, &token).await.map_err(Into::into)
    }

    pub async fn disconnect(&self) -> PortalResult<()> {
        self.inner.disconnect().await.map_err(Into::into)
    }

    pub fn send_video_frame(
        &self,
        track_name: String,
        rgb_data: Vec<u8>,
        width: u32,
        height: u32,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        self.inner
            .send_video_frame(&track_name, &rgb_data, width, height, timestamp_us)
            .map_err(Into::into)
    }

    pub fn send_state(
        &self,
        values: HashMap<String, f64>,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        // Schema comes from the core Portal on every send so we don't
        // carry a duplicate snapshot. Lookup is a linear scan over a
        // small list — cheaper than cloning the Vec at construction.
        let typed = f64_to_typed(&values, self.inner.state_schema());
        self.inner.send_state(&typed, timestamp_us).map_err(Into::into)
    }

    pub fn send_action(
        &self,
        values: HashMap<String, f64>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        let typed = f64_to_typed(&values, self.inner.action_schema());
        self.inner.send_action(&typed, timestamp_us, in_reply_to_ts_us).map_err(Into::into)
    }

    pub fn get_observation(&self) -> PortalResult<Option<Observation>> {
        Ok(self.inner.get_observation()?.as_ref().map(observation_from_core))
    }

    pub fn observation_sync(&self) -> bool {
        self.inner.observation_sync()
    }

    pub fn get_action(&self) -> Option<Action> {
        self.inner.get_action().map(|a| Action {
            values: a.raw_values,
            timestamp_us: a.timestamp_us,
            in_reply_to_ts_us: a.in_reply_to_ts_us,
            sender: a.sender,
            active: a.active,
        })
    }

    pub fn get_state(&self) -> Option<State> {
        self.inner.get_state().map(|s| State { values: s.raw_values, timestamp_us: s.timestamp_us })
    }

    pub fn get_video_frame(&self, track_name: String) -> Option<VideoFrame> {
        self.inner.get_video_frame(&track_name).as_ref().map(frame_from_core)
    }

    pub fn metrics(&self) -> PortalMetrics {
        metrics_from_core(self.inner.metrics())
    }

    /// Now on the robot's clock, in microseconds.
    pub fn now_us(&self) -> u64 {
        self.inner.now_us()
    }

    pub fn reset_metrics(&self) {
        self.inner.reset_metrics();
    }

    pub fn state_fields(&self) -> Vec<String> {
        self.state_fields.clone()
    }

    pub fn action_fields(&self) -> Vec<String> {
        self.action_fields.clone()
    }

    /// Names of every declared video track, in declaration order, whatever
    /// transport its codec selects.
    pub fn video_tracks(&self) -> Vec<String> {
        self.video_tracks.clone()
    }

    /// Every declared video track with its codec and options, in declaration
    /// order.
    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.video_track_specs.clone()
    }

    /// The byte-stream subset (Raw / Png / Mjpeg) of `video_track_specs`.
    /// These ride a byte-stream channel rather than the WebRTC media path;
    /// the user-facing send/receive API is the same either way.
    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.video_track_specs
            .iter()
            .filter(|s| !core::Codec::from(s.codec).is_webrtc())
            .cloned()
            .collect()
    }

    // --- Multi-controller ---

    /// Own LiveKit identity once connected. `None` before `connect()`.
    pub fn local_identity(&self) -> Option<String> {
        self.inner.local_identity()
    }

    /// Identity of the operator the robot is currently listening to, or
    /// `None`. On Robot side, the local pointer. On Operator side, a mirror
    /// of the robot's `lk.portal.active_operator` attribute.
    pub fn active_operator(&self) -> Option<String> {
        self.inner.active_operator()
    }

    /// Set the active operator. Local + broadcast on Robot side. RPC to
    /// the robot on Operator side. Pass `None` to clear.
    pub async fn set_active_operator(&self, identity: Option<String>) -> PortalResult<()> {
        self.inner.set_active_operator(identity).await.map_err(Into::into)
    }

    /// Currently-connected operator identities (excluding self), sorted.
    pub fn operators(&self) -> Vec<String> {
        self.inner.operators()
    }

    pub fn observers(&self) -> Vec<String> {
        self.inner.observers()
    }

    /// Robot's identity if discovered, else `None`. Operator-side helper.
    pub fn robot_identity(&self) -> Option<String> {
        self.inner.robot_identity()
    }

    /// Register a method handler. Handlers may be registered before or
    /// after `connect()`; reconnects reapply the stored set.
    pub fn register_rpc_method(&self, method: String, handler: Arc<dyn RpcHandler>) {
        self.inner.register_rpc_method(&method, wrap_foreign_handler(handler));
    }

    pub fn unregister_rpc_method(&self, method: String) {
        self.inner.unregister_rpc_method(&method);
    }

    /// Invoke a method on the peer. When `destination` is `None`, the call
    /// is routed to the identified peer, falling back to the single remote
    /// participant in the room. Timeout defaults to the SDK's 15s if
    /// `response_timeout_ms` is `None`.
    pub async fn perform_rpc(
        &self,
        destination: Option<String>,
        method: String,
        payload: String,
        response_timeout_ms: Option<u64>,
    ) -> PortalResult<String> {
        let timeout = response_timeout_ms.map(std::time::Duration::from_millis);
        self.inner
            .perform_rpc(destination.as_deref(), &method, payload, timeout)
            .await
            .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Role-split surface
//
// `Robot` / `Operator` (and their configs) are thin wrappers over the unified
// `Portal` / `PortalConfig`. They exist so foreign bindings inherit the role
// split straight from UniFFI instead of hand-reimplementing it in every host
// language — which is what the Python layer did before this. Each type
// exposes only the methods that make sense for its role; the wrong-role
// methods are simply absent rather than runtime-erroring with `WrongRole`.
// The core crate stays unified: both roles drive the same `core::Portal`.
//
// Configs are distinct objects (`RobotConfig` / `OperatorConfig`) purely for
// the type safety — `Robot::new` takes an `Arc<RobotConfig>`, so an operator
// config can't be passed by mistake. Both pin the role internally, so callers
// never name `Role` themselves.
//
// Callbacks are still delivered through the uniform `PortalCallbacks` trait,
// passed at construction. A binding wires up only the events its role
// consumes (a Robot ignores `on_state` / `on_observation`; an Operator
// ignores `on_action` unless it opted into action subscription).
// ---------------------------------------------------------------------------

/// Robot-side session config. Same declarative surface as `OperatorConfig`;
/// the role is pinned to `Role::Robot` internally.
#[derive(uniffi::Object)]
pub struct RobotConfig {
    inner: Arc<PortalConfig>,
}

#[uniffi::export]
impl RobotConfig {
    #[uniffi::constructor]
    pub fn new(session: String) -> Arc<Self> {
        Arc::new(Self { inner: PortalConfig::new(session, Role::Robot) })
    }

    /// Build a `RobotConfig` from a YAML string. See
    /// `PortalConfig::from_yaml_str` for the schema and semantics.
    #[uniffi::constructor]
    pub fn from_yaml_str(yaml: String, session: String) -> Result<Arc<Self>, ConfigFileError> {
        Ok(Arc::new(Self { inner: PortalConfig::from_yaml_str(yaml, session, Role::Robot)? }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_video(
        &self,
        name: String,
        codec: VideoCodec,
        quality: u8,
        max_bitrate_kbps: Option<u32>,
        simulcast: Option<bool>,
        screencast: Option<bool>,
        stall_behavior: Option<StallBehavior>,
        max_lag_ms: Option<u32>,
    ) {
        self.inner.add_video(
            name,
            codec,
            quality,
            max_bitrate_kbps,
            simulcast,
            screencast,
            stall_behavior,
            max_lag_ms,
        );
    }

    pub fn add_state_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.add_state_typed(schema);
    }

    pub fn add_action_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.add_action_typed(schema);
    }

    pub fn set_fps(&self, fps: u32) {
        self.inner.set_fps(fps);
    }

    pub fn set_slack(&self, ticks: u32) {
        self.inner.set_slack(ticks);
    }

    pub fn set_tolerance(&self, ticks: f32) {
        self.inner.set_tolerance(ticks);
    }

    pub fn set_state_reliable(&self, reliable: bool) {
        self.inner.set_state_reliable(reliable);
    }

    pub fn set_action_reliable(&self, reliable: bool) {
        self.inner.set_action_reliable(reliable);
    }

    /// Test hook: shifts this peer's local clock.
    pub fn set_clock_skew_us(&self, skew_us: i64) {
        self.inner.set_clock_skew_us(skew_us);
    }

    pub fn set_time_sync_source(&self, source: TimeSyncSource) {
        self.inner.set_time_sync_source(source);
    }

    pub fn time_sync_source(&self) -> TimeSyncSource {
        self.inner.time_sync_source()
    }

    pub fn set_e2ee_key(&self, key: Vec<u8>) {
        self.inner.set_e2ee_key(key);
    }

    #[allow(deprecated)]
    pub fn set_reuse_stale_frames(&self, enable: bool) {
        self.inner.set_reuse_stale_frames(enable);
    }

    /// How a moment is resolved when a video track goes silent past its
    /// `max_lag`. Applies to tracks without a per-track override.
    pub fn set_stall_behavior(&self, behavior: StallBehavior) {
        self.inner.set_stall_behavior(behavior);
    }

    /// How far the fastest-advancing stream may run past a moment before it
    /// resolves without a silent track, in milliseconds of sender-clock time
    /// (not wall-clock). Defaults to `slack / fps`; `0` resolves immediately.
    pub fn set_max_lag_ms(&self, ms: u32) {
        self.inner.set_max_lag_ms(ms);
    }

    /// Per-track override for `set_stall_behavior`.
    pub fn set_track_stall_behavior(&self, track: String, behavior: StallBehavior) {
        self.inner.set_track_stall_behavior(track, behavior);
    }

    /// Per-track override for `set_max_lag_ms`.
    pub fn set_track_max_lag_ms(&self, track: String, ms: u32) {
        self.inner.set_track_max_lag_ms(track, ms);
    }

    /// No-op on the Robot side — the robot always processes actions. Kept on
    /// the surface so `RobotConfig` and `OperatorConfig` stay symmetrical.
    pub fn set_action_subscription(&self, subscription: ActionSubscription) {
        self.inner.set_action_subscription(subscription);
    }

    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.video_tracks()
    }

    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.video_track_specs()
    }

    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.frame_video_tracks()
    }

    pub fn state_schema(&self) -> Vec<FieldSpec> {
        self.inner.state_schema()
    }

    pub fn action_schema(&self) -> Vec<FieldSpec> {
        self.inner.action_schema()
    }

    pub fn session(&self) -> String {
        self.inner.session()
    }

    pub fn role(&self) -> Role {
        self.inner.role()
    }

    pub fn fps(&self) -> u32 {
        self.inner.fps()
    }

    pub fn slack(&self) -> u32 {
        self.inner.slack()
    }

    pub fn tolerance(&self) -> f32 {
        self.inner.tolerance()
    }

    pub fn state_reliable(&self) -> bool {
        self.inner.state_reliable()
    }

    pub fn action_reliable(&self) -> bool {
        self.inner.action_reliable()
    }

    #[allow(deprecated)]
    pub fn reuse_stale_frames(&self) -> bool {
        self.inner.reuse_stale_frames()
    }

    /// Always reports what was set, but the robot ignores the flag — it
    /// always processes actions. Kept for surface symmetry.
    pub fn action_subscription(&self) -> ActionSubscription {
        self.inner.action_subscription()
    }

    pub fn has_e2ee_key(&self) -> bool {
        self.inner.has_e2ee_key()
    }
}

/// Operator-side session config. Same declarative surface as `RobotConfig`;
/// the role is pinned to `Role::Operator` internally. Identity is set on the
/// LiveKit access token at mint time and read back via
/// `Operator::local_identity` after `connect()` — there is no config-level
/// identity field.
#[derive(uniffi::Object)]
pub struct OperatorConfig {
    inner: Arc<PortalConfig>,
}

#[uniffi::export]
impl OperatorConfig {
    #[uniffi::constructor]
    pub fn new(session: String) -> Arc<Self> {
        Arc::new(Self { inner: PortalConfig::new(session, Role::Operator) })
    }

    /// Build an `OperatorConfig` from a YAML string. See
    /// `PortalConfig::from_yaml_str` for the schema and semantics.
    #[uniffi::constructor]
    pub fn from_yaml_str(yaml: String, session: String) -> Result<Arc<Self>, ConfigFileError> {
        Ok(Arc::new(Self { inner: PortalConfig::from_yaml_str(yaml, session, Role::Operator)? }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_video(
        &self,
        name: String,
        codec: VideoCodec,
        quality: u8,
        max_bitrate_kbps: Option<u32>,
        simulcast: Option<bool>,
        screencast: Option<bool>,
        stall_behavior: Option<StallBehavior>,
        max_lag_ms: Option<u32>,
    ) {
        self.inner.add_video(
            name,
            codec,
            quality,
            max_bitrate_kbps,
            simulcast,
            screencast,
            stall_behavior,
            max_lag_ms,
        );
    }

    pub fn add_state_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.add_state_typed(schema);
    }

    pub fn add_action_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.add_action_typed(schema);
    }

    pub fn set_fps(&self, fps: u32) {
        self.inner.set_fps(fps);
    }

    pub fn set_slack(&self, ticks: u32) {
        self.inner.set_slack(ticks);
    }

    pub fn set_tolerance(&self, ticks: f32) {
        self.inner.set_tolerance(ticks);
    }

    pub fn set_state_reliable(&self, reliable: bool) {
        self.inner.set_state_reliable(reliable);
    }

    pub fn set_action_reliable(&self, reliable: bool) {
        self.inner.set_action_reliable(reliable);
    }

    /// Test hook: shifts this peer's local clock.
    pub fn set_clock_skew_us(&self, skew_us: i64) {
        self.inner.set_clock_skew_us(skew_us);
    }

    pub fn set_time_sync_source(&self, source: TimeSyncSource) {
        self.inner.set_time_sync_source(source);
    }

    pub fn time_sync_source(&self) -> TimeSyncSource {
        self.inner.time_sync_source()
    }

    pub fn set_observation_sync(&self, enable: bool) {
        self.inner.set_observation_sync(enable);
    }

    pub fn observation_sync(&self) -> bool {
        self.inner.observation_sync()
    }

    pub fn set_e2ee_key(&self, key: Vec<u8>) {
        self.inner.set_e2ee_key(key);
    }

    #[allow(deprecated)]
    pub fn set_reuse_stale_frames(&self, enable: bool) {
        self.inner.set_reuse_stale_frames(enable);
    }

    /// How a moment is resolved when a video track goes silent past its
    /// `max_lag`. Applies to tracks without a per-track override.
    pub fn set_stall_behavior(&self, behavior: StallBehavior) {
        self.inner.set_stall_behavior(behavior);
    }

    /// How far the fastest-advancing stream may run past a moment before it
    /// resolves without a silent track, in milliseconds of sender-clock time
    /// (not wall-clock). Defaults to `slack / fps`; `0` resolves immediately.
    pub fn set_max_lag_ms(&self, ms: u32) {
        self.inner.set_max_lag_ms(ms);
    }

    /// Per-track override for `set_stall_behavior`.
    pub fn set_track_stall_behavior(&self, track: String, behavior: StallBehavior) {
        self.inner.set_track_stall_behavior(track, behavior);
    }

    /// Per-track override for `set_max_lag_ms`.
    pub fn set_track_max_lag_ms(&self, track: String, ms: u32) {
        self.inner.set_track_max_lag_ms(track, ms);
    }

    /// Which received actions reach `on_action`. See
    /// `PortalConfig::set_action_subscription`.
    pub fn set_action_subscription(&self, subscription: ActionSubscription) {
        self.inner.set_action_subscription(subscription);
    }

    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.video_tracks()
    }

    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.video_track_specs()
    }

    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.frame_video_tracks()
    }

    pub fn state_schema(&self) -> Vec<FieldSpec> {
        self.inner.state_schema()
    }

    pub fn action_schema(&self) -> Vec<FieldSpec> {
        self.inner.action_schema()
    }

    pub fn session(&self) -> String {
        self.inner.session()
    }

    pub fn role(&self) -> Role {
        self.inner.role()
    }

    pub fn fps(&self) -> u32 {
        self.inner.fps()
    }

    pub fn slack(&self) -> u32 {
        self.inner.slack()
    }

    pub fn tolerance(&self) -> f32 {
        self.inner.tolerance()
    }

    pub fn state_reliable(&self) -> bool {
        self.inner.state_reliable()
    }

    pub fn action_reliable(&self) -> bool {
        self.inner.action_reliable()
    }

    #[allow(deprecated)]
    pub fn reuse_stale_frames(&self) -> bool {
        self.inner.reuse_stale_frames()
    }

    pub fn action_subscription(&self) -> ActionSubscription {
        self.inner.action_subscription()
    }

    pub fn has_e2ee_key(&self) -> bool {
        self.inner.has_e2ee_key()
    }
}

/// Robot-side Portal facade. Exposes publish-state/video, receive
/// actions, and the shared control plane. Wrong-role methods
/// (`send_action`, observation getters) are absent by construction.
#[derive(uniffi::Object)]
pub struct Robot {
    inner: Arc<Portal>,
}

#[uniffi::export(async_runtime = "tokio")]
impl Robot {
    #[uniffi::constructor]
    pub fn new(config: Arc<RobotConfig>, callbacks: Arc<dyn PortalCallbacks>) -> Arc<Self> {
        Arc::new(Self { inner: Portal::new(config.inner.clone(), callbacks) })
    }

    pub async fn connect(&self, url: String, token: String) -> PortalResult<()> {
        self.inner.connect(url, token).await
    }

    pub async fn disconnect(&self) -> PortalResult<()> {
        self.inner.disconnect().await
    }

    // -- publish (robot-side) ------------------------------------------------

    pub fn send_video_frame(
        &self,
        track_name: String,
        rgb_data: Vec<u8>,
        width: u32,
        height: u32,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        self.inner.send_video_frame(track_name, rgb_data, width, height, timestamp_us)
    }

    pub fn send_state(
        &self,
        values: HashMap<String, f64>,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        self.inner.send_state(values, timestamp_us)
    }

    // -- receive (robot-side) ------------------------------------------------

    pub fn get_action(&self) -> Option<Action> {
        self.inner.get_action()
    }

    // -- introspection (shared) ----------------------------------------------

    pub fn state_fields(&self) -> Vec<String> {
        self.inner.state_fields()
    }

    pub fn action_fields(&self) -> Vec<String> {
        self.inner.action_fields()
    }

    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.video_tracks()
    }

    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.video_track_specs()
    }

    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.frame_video_tracks()
    }

    // -- multi-controller + rpc + metrics (shared) ---------------------------

    pub fn local_identity(&self) -> Option<String> {
        self.inner.local_identity()
    }

    pub fn active_operator(&self) -> Option<String> {
        self.inner.active_operator()
    }

    pub async fn set_active_operator(&self, identity: Option<String>) -> PortalResult<()> {
        self.inner.set_active_operator(identity).await
    }

    pub fn operators(&self) -> Vec<String> {
        self.inner.operators()
    }

    pub fn observers(&self) -> Vec<String> {
        self.inner.observers()
    }

    pub fn register_rpc_method(&self, method: String, handler: Arc<dyn RpcHandler>) {
        self.inner.register_rpc_method(method, handler);
    }

    pub fn unregister_rpc_method(&self, method: String) {
        self.inner.unregister_rpc_method(method);
    }

    pub async fn perform_rpc(
        &self,
        destination: Option<String>,
        method: String,
        payload: String,
        response_timeout_ms: Option<u64>,
    ) -> PortalResult<String> {
        self.inner.perform_rpc(destination, method, payload, response_timeout_ms).await
    }

    pub fn metrics(&self) -> PortalMetrics {
        self.inner.metrics()
    }

    /// Now on the robot's clock, in microseconds.
    pub fn now_us(&self) -> u64 {
        self.inner.now_us()
    }

    pub fn reset_metrics(&self) {
        self.inner.reset_metrics();
    }
}

/// Operator-side Portal facade. Exposes publish-action, receive
/// observations/state/video, and the shared control plane. Wrong-role methods
/// (`send_state`, `send_video_frame`) are absent by construction. The
/// action-subscription getter (`get_action`) is
/// present but only yield values when `OperatorConfig::set_action_subscription`
/// was enabled.
#[derive(uniffi::Object)]
pub struct Operator {
    inner: Arc<Portal>,
}

#[uniffi::export(async_runtime = "tokio")]
impl Operator {
    #[uniffi::constructor]
    pub fn new(config: Arc<OperatorConfig>, callbacks: Arc<dyn PortalCallbacks>) -> Arc<Self> {
        Arc::new(Self { inner: Portal::new(config.inner.clone(), callbacks) })
    }

    pub async fn connect(&self, url: String, token: String) -> PortalResult<()> {
        self.inner.connect(url, token).await
    }

    pub async fn disconnect(&self) -> PortalResult<()> {
        self.inner.disconnect().await
    }

    // -- publish (operator-side) ---------------------------------------------

    pub fn send_action(
        &self,
        values: HashMap<String, f64>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        self.inner.send_action(values, timestamp_us, in_reply_to_ts_us)
    }

    // -- receive (operator-side) ---------------------------------------------

    pub fn get_state(&self) -> Option<State> {
        self.inner.get_state()
    }

    pub fn get_observation(&self) -> PortalResult<Option<Observation>> {
        self.inner.get_observation()
    }

    pub fn observation_sync(&self) -> bool {
        self.inner.observation_sync()
    }

    pub fn get_video_frame(&self, track_name: String) -> Option<VideoFrame> {
        self.inner.get_video_frame(track_name)
    }

    /// Latest executed action, or `None`. Requires
    /// an `OperatorConfig::set_action_subscription` other than `NONE` for any value to land.
    pub fn get_action(&self) -> Option<Action> {
        self.inner.get_action()
    }

    // -- introspection (shared) ----------------------------------------------

    pub fn state_fields(&self) -> Vec<String> {
        self.inner.state_fields()
    }

    pub fn action_fields(&self) -> Vec<String> {
        self.inner.action_fields()
    }

    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.video_tracks()
    }

    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.video_track_specs()
    }

    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.frame_video_tracks()
    }

    // -- multi-controller + rpc + metrics (shared) ---------------------------

    pub fn local_identity(&self) -> Option<String> {
        self.inner.local_identity()
    }

    pub fn active_operator(&self) -> Option<String> {
        self.inner.active_operator()
    }

    pub async fn set_active_operator(&self, identity: Option<String>) -> PortalResult<()> {
        self.inner.set_active_operator(identity).await
    }

    pub fn operators(&self) -> Vec<String> {
        self.inner.operators()
    }

    pub fn observers(&self) -> Vec<String> {
        self.inner.observers()
    }

    pub fn robot_identity(&self) -> Option<String> {
        self.inner.robot_identity()
    }

    pub fn register_rpc_method(&self, method: String, handler: Arc<dyn RpcHandler>) {
        self.inner.register_rpc_method(method, handler);
    }

    pub fn unregister_rpc_method(&self, method: String) {
        self.inner.unregister_rpc_method(method);
    }

    pub async fn perform_rpc(
        &self,
        destination: Option<String>,
        method: String,
        payload: String,
        response_timeout_ms: Option<u64>,
    ) -> PortalResult<String> {
        self.inner.perform_rpc(destination, method, payload, response_timeout_ms).await
    }

    pub fn metrics(&self) -> PortalMetrics {
        self.inner.metrics()
    }

    /// Now on the robot's clock, in microseconds.
    pub fn now_us(&self) -> u64 {
        self.inner.now_us()
    }

    pub fn reset_metrics(&self) {
        self.inner.reset_metrics();
    }
}

/// Observer-side session config. Same declarative surface as
/// `OperatorConfig`; the role is pinned to `Role::Observer` internally, and
/// it defaults to `ActionSubscription::Active` with observation sync off. Identity is set on the
/// LiveKit access token at mint time and read back via
/// `Observer::local_identity` after `connect()` — there is no config-level
/// identity field.
#[derive(uniffi::Object)]
pub struct ObserverConfig {
    inner: Arc<PortalConfig>,
}

#[uniffi::export]
impl ObserverConfig {
    #[uniffi::constructor]
    pub fn new(session: String) -> Arc<Self> {
        Arc::new(Self { inner: PortalConfig::new(session, Role::Observer) })
    }

    /// Build an `ObserverConfig` from a YAML string. See
    /// `PortalConfig::from_yaml_str` for the schema and semantics.
    #[uniffi::constructor]
    pub fn from_yaml_str(yaml: String, session: String) -> Result<Arc<Self>, ConfigFileError> {
        Ok(Arc::new(Self { inner: PortalConfig::from_yaml_str(yaml, session, Role::Observer)? }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_video(
        &self,
        name: String,
        codec: VideoCodec,
        quality: u8,
        max_bitrate_kbps: Option<u32>,
        simulcast: Option<bool>,
        screencast: Option<bool>,
        stall_behavior: Option<StallBehavior>,
        max_lag_ms: Option<u32>,
    ) {
        self.inner.add_video(
            name,
            codec,
            quality,
            max_bitrate_kbps,
            simulcast,
            screencast,
            stall_behavior,
            max_lag_ms,
        );
    }

    pub fn add_state_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.add_state_typed(schema);
    }

    pub fn add_action_typed(&self, schema: Vec<FieldSpec>) {
        self.inner.add_action_typed(schema);
    }

    pub fn set_fps(&self, fps: u32) {
        self.inner.set_fps(fps);
    }

    pub fn set_slack(&self, ticks: u32) {
        self.inner.set_slack(ticks);
    }

    pub fn set_tolerance(&self, ticks: f32) {
        self.inner.set_tolerance(ticks);
    }

    pub fn set_state_reliable(&self, reliable: bool) {
        self.inner.set_state_reliable(reliable);
    }

    pub fn set_action_reliable(&self, reliable: bool) {
        self.inner.set_action_reliable(reliable);
    }

    /// Test hook: shifts this peer's local clock.
    pub fn set_clock_skew_us(&self, skew_us: i64) {
        self.inner.set_clock_skew_us(skew_us);
    }

    pub fn set_time_sync_source(&self, source: TimeSyncSource) {
        self.inner.set_time_sync_source(source);
    }

    pub fn time_sync_source(&self) -> TimeSyncSource {
        self.inner.time_sync_source()
    }

    pub fn set_observation_sync(&self, enable: bool) {
        self.inner.set_observation_sync(enable);
    }

    pub fn observation_sync(&self) -> bool {
        self.inner.observation_sync()
    }

    pub fn set_e2ee_key(&self, key: Vec<u8>) {
        self.inner.set_e2ee_key(key);
    }

    #[allow(deprecated)]
    pub fn set_reuse_stale_frames(&self, enable: bool) {
        self.inner.set_reuse_stale_frames(enable);
    }

    /// How a moment is resolved when a video track goes silent past its
    /// `max_lag`. Applies to tracks without a per-track override.
    pub fn set_stall_behavior(&self, behavior: StallBehavior) {
        self.inner.set_stall_behavior(behavior);
    }

    /// How far the fastest-advancing stream may run past a moment before it
    /// resolves without a silent track, in milliseconds of sender-clock time
    /// (not wall-clock). Defaults to `slack / fps`; `0` resolves immediately.
    pub fn set_max_lag_ms(&self, ms: u32) {
        self.inner.set_max_lag_ms(ms);
    }

    /// Per-track override for `set_stall_behavior`.
    pub fn set_track_stall_behavior(&self, track: String, behavior: StallBehavior) {
        self.inner.set_track_stall_behavior(track, behavior);
    }

    /// Per-track override for `set_max_lag_ms`.
    pub fn set_track_max_lag_ms(&self, track: String, ms: u32) {
        self.inner.set_track_max_lag_ms(track, ms);
    }

    /// Which received actions reach `on_action`. See
    /// `PortalConfig::set_action_subscription`.
    pub fn set_action_subscription(&self, subscription: ActionSubscription) {
        self.inner.set_action_subscription(subscription);
    }

    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.video_tracks()
    }

    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.video_track_specs()
    }

    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.frame_video_tracks()
    }

    pub fn state_schema(&self) -> Vec<FieldSpec> {
        self.inner.state_schema()
    }

    pub fn action_schema(&self) -> Vec<FieldSpec> {
        self.inner.action_schema()
    }

    pub fn session(&self) -> String {
        self.inner.session()
    }

    pub fn role(&self) -> Role {
        self.inner.role()
    }

    pub fn fps(&self) -> u32 {
        self.inner.fps()
    }

    pub fn slack(&self) -> u32 {
        self.inner.slack()
    }

    pub fn tolerance(&self) -> f32 {
        self.inner.tolerance()
    }

    pub fn state_reliable(&self) -> bool {
        self.inner.state_reliable()
    }

    pub fn action_reliable(&self) -> bool {
        self.inner.action_reliable()
    }

    #[allow(deprecated)]
    pub fn reuse_stale_frames(&self) -> bool {
        self.inner.reuse_stale_frames()
    }

    pub fn action_subscription(&self) -> ActionSubscription {
        self.inner.action_subscription()
    }

    pub fn has_e2ee_key(&self) -> bool {
        self.inner.has_e2ee_key()
    }
}

/// Observer-side Portal facade: receives everything in the room and can
/// hand control between operators, but has no send methods for state or
/// actions.
#[derive(uniffi::Object)]
pub struct Observer {
    inner: Arc<Portal>,
}

#[uniffi::export(async_runtime = "tokio")]
impl Observer {
    #[uniffi::constructor]
    pub fn new(config: Arc<ObserverConfig>, callbacks: Arc<dyn PortalCallbacks>) -> Arc<Self> {
        Arc::new(Self { inner: Portal::new(config.inner.clone(), callbacks) })
    }

    pub async fn connect(&self, url: String, token: String) -> PortalResult<()> {
        self.inner.connect(url, token).await
    }

    pub async fn disconnect(&self) -> PortalResult<()> {
        self.inner.disconnect().await
    }

    // -- receive ---------------------------------------------

    pub fn get_state(&self) -> Option<State> {
        self.inner.get_state()
    }

    pub fn get_observation(&self) -> PortalResult<Option<Observation>> {
        self.inner.get_observation()
    }

    pub fn observation_sync(&self) -> bool {
        self.inner.observation_sync()
    }

    pub fn get_video_frame(&self, track_name: String) -> Option<VideoFrame> {
        self.inner.get_video_frame(track_name)
    }

    /// Latest executed action, or `None`. Requires
    /// an `ObserverConfig::set_action_subscription` other than `NONE` for any value to land.
    pub fn get_action(&self) -> Option<Action> {
        self.inner.get_action()
    }

    // -- introspection (shared) ----------------------------------------------

    pub fn state_fields(&self) -> Vec<String> {
        self.inner.state_fields()
    }

    pub fn action_fields(&self) -> Vec<String> {
        self.inner.action_fields()
    }

    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.video_tracks()
    }

    pub fn video_track_specs(&self) -> Vec<VideoTrackSpec> {
        self.inner.video_track_specs()
    }

    pub fn frame_video_tracks(&self) -> Vec<VideoTrackSpec> {
        self.inner.frame_video_tracks()
    }

    // -- multi-controller + rpc + metrics (shared) ---------------------------

    pub fn local_identity(&self) -> Option<String> {
        self.inner.local_identity()
    }

    pub fn active_operator(&self) -> Option<String> {
        self.inner.active_operator()
    }

    pub async fn set_active_operator(&self, identity: Option<String>) -> PortalResult<()> {
        self.inner.set_active_operator(identity).await
    }

    pub fn operators(&self) -> Vec<String> {
        self.inner.operators()
    }

    pub fn observers(&self) -> Vec<String> {
        self.inner.observers()
    }

    pub fn robot_identity(&self) -> Option<String> {
        self.inner.robot_identity()
    }

    pub fn register_rpc_method(&self, method: String, handler: Arc<dyn RpcHandler>) {
        self.inner.register_rpc_method(method, handler);
    }

    pub fn unregister_rpc_method(&self, method: String) {
        self.inner.unregister_rpc_method(method);
    }

    pub async fn perform_rpc(
        &self,
        destination: Option<String>,
        method: String,
        payload: String,
        response_timeout_ms: Option<u64>,
    ) -> PortalResult<String> {
        self.inner.perform_rpc(destination, method, payload, response_timeout_ms).await
    }

    pub fn metrics(&self) -> PortalMetrics {
        self.inner.metrics()
    }

    /// Now on the robot's clock, in microseconds.
    pub fn now_us(&self) -> u64 {
        self.inner.now_us()
    }

    pub fn reset_metrics(&self) {
        self.inner.reset_metrics();
    }
}

// ---------------------------------------------------------------------------
// Conversions from core types. Records own their data, so we copy frame
// bytes out of the core's `Arc<[u8]>` into `Vec<u8>` at the boundary.
// ---------------------------------------------------------------------------

fn frame_from_core(f: &core::VideoFrameData) -> VideoFrame {
    VideoFrame {
        width: f.width,
        height: f.height,
        data: f.data.to_vec(),
        timestamp_us: f.timestamp_us,
        source: f.source.into(),
    }
}

fn observation_from_core(o: &core::Observation) -> Observation {
    // FFI carries the raw f64 state map across the boundary; foreign
    // bindings (Python) re-cast to typed values in their own record.
    Observation {
        timestamp_us: o.timestamp_us,
        state: o.raw_state.clone(),
        frames: o.frames.iter().map(|(k, v)| (k.clone(), frame_from_core(v))).collect(),
    }
}

/// Convert the foreign `HashMap<String, f64>` (what UniFFI accepts for
/// Python dicts) into the core's `HashMap<String, TypedValue>` using the
/// declared schema. Keys absent from the schema are passed through as
/// `F64` so the core's unknown-key warn path still fires.
fn f64_to_typed(
    values: &HashMap<String, f64>,
    schema: &[core::FieldSpec],
) -> HashMap<String, core::TypedValue> {
    values
        .iter()
        .map(|(name, &v)| {
            let dtype = schema
                .iter()
                .find(|f| &f.name == name)
                .map(|f| f.dtype)
                .unwrap_or(core::DType::F64);
            (name.clone(), core::TypedValue::from_f64(v, dtype))
        })
        .collect()
}

/// Adapt a foreign `RpcHandler` trait object to the core handler type.
/// The outer `Fn` closure is invoked once per incoming RPC; the Arc clone
/// moves an owned handle into the returned future so the closure can be
/// called again without consuming its capture.
fn wrap_foreign_handler(handler: Arc<dyn RpcHandler>) -> core::RpcHandler {
    Arc::new(move |data: core::RpcInvocationData| {
        let handler = handler.clone();
        Box::pin(async move {
            let ffi_data = RpcInvocationData::from(data);
            handler.handle(ffi_data).await.map_err(Into::into)
        })
    })
}

fn metrics_from_core(m: core::PortalMetrics) -> PortalMetrics {
    PortalMetrics {
        sync: SyncMetrics {
            observations_emitted: m.sync.observations_emitted,
            stale_observations_emitted: m.sync.stale_observations_emitted,
            states_dropped: m.sync.states_dropped,
            frames_omitted: m.sync.frames_omitted.clone(),
            match_delta_us_p50: m.sync.match_delta_us_p50,
            match_delta_us_p95: m.sync.match_delta_us_p95,
            last_blocker_track: m.sync.last_blocker_track,
        },
        transport: TransportMetrics {
            frames_sent: m.transport.frames_sent,
            frames_received: m.transport.frames_received,
            frames_dropped_publisher_full: m.transport.frames_dropped_publisher_full,
            bytes_sent: m.transport.bytes_sent,
            bytes_received: m.transport.bytes_received,
            states_sent: m.transport.states_sent,
            states_received: m.transport.states_received,
            actions_sent: m.transport.actions_sent,
            actions_received: m.transport.actions_received,
            frame_jitter_us: m.transport.frame_jitter_us,
            state_jitter_us: m.transport.state_jitter_us,
            action_jitter_us: m.transport.action_jitter_us,
        },
        buffers: BufferMetrics {
            video_fill: m.buffers.video_fill.into_iter().map(|(k, v)| (k, v as u64)).collect(),
            state_fill: m.buffers.state_fill as u64,
            evictions: m.buffers.evictions,
        },
        rtt: RttMetrics {
            rtt_us_last: m.rtt.rtt_us_last,
            rtt_us_mean: m.rtt.rtt_us_mean,
            rtt_us_p95: m.rtt.rtt_us_p95,
            pings_sent: m.rtt.pings_sent,
            pongs_received: m.rtt.pongs_received,
        },
        time_sync: TimeSyncMetrics {
            source: m.time_sync.source.into(),
            synced: m.time_sync.synced,
            offset_us: m.time_sync.offset_us,
            uncertainty_us: m.time_sync.uncertainty_us,
            measured_offset_us: m.time_sync.measured_offset_us,
            resyncs: m.time_sync.resyncs,
            samples_rejected: m.time_sync.samples_rejected,
        },
        policy: PolicyMetrics {
            e2e_us_p50: m.policy.e2e_us_p50,
            e2e_us_p95: m.policy.e2e_us_p95,
            correlated_received: m.policy.correlated_received,
        },
    }
}
