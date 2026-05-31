//! UniFFI wrapper around `livekit-portal`.
//!
//! The core `livekit_portal::Portal` stays free of binding concerns; this
//! crate re-exposes it as a proc-macro-annotated UniFFI surface that
//! generates Python (and, later, Swift/Kotlin) bindings directly from Rust.
//!
//! Shape:
//!   * `PortalConfig` and `Portal` are `#[uniffi::Object]`s. Constructors and
//!     methods run through UniFFI's Arc-based lifecycle.
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
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
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
}

impl From<Role> for core::Role {
    fn from(r: Role) -> Self {
        match r {
            Role::Robot => core::Role::Robot,
            Role::Operator => core::Role::Operator,
        }
    }
}

impl From<core::Role> for Role {
    fn from(r: core::Role) -> Self {
        match r {
            core::Role::Robot => Role::Robot,
            core::Role::Operator => Role::Operator,
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

/// One declared byte-stream video track: name, codec, and per-codec
/// quality. Crosses the FFI boundary so bindings can introspect tracks
/// declared via `PortalConfig.add_video` with a non-`H264` codec.
/// `quality` is meaningful for `VideoCodec.Mjpeg` (1..=100) and ignored for
/// `Raw` / `Png`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FrameVideoSpec {
    pub name: String,
    pub codec: VideoCodec,
    pub quality: u8,
}

/// A declared action chunk: name, fixed horizon, ordered field list. The
/// chunk's payload travels as a LiveKit byte stream (not a data packet) so
/// it isn't bounded by the 15 KB packet limit.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChunkSpec {
    pub name: String,
    pub horizon: u32,
    pub fields: Vec<FieldSpec>,
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
}

/// A received action chunk. `data` is `field -> column of length horizon`,
/// each column widened to `f64` (lossless for every supported dtype).
/// Bindings may re-cast columns into typed numpy arrays per the declared
/// chunk schema.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionChunk {
    pub name: String,
    pub horizon: u32,
    pub data: HashMap<String, Vec<f64>>,
    pub timestamp_us: u64,
    pub in_reply_to_ts_us: Option<u64>,
    /// Identity of the operator that produced this chunk; same semantics
    /// as `Action::sender`.
    pub sender: String,
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
    pub action_chunks_sent: u64,
    pub action_chunks_received: u64,
    pub frame_jitter_us: HashMap<String, u64>,
    pub state_jitter_us: u64,
    pub action_jitter_us: u64,
    pub action_chunk_jitter_us: u64,
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

#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct PortalMetrics {
    pub sync: SyncMetrics,
    pub transport: TransportMetrics,
    pub buffers: BufferMetrics,
    pub rtt: RttMetrics,
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

    #[error("unknown action chunk: {0}")]
    UnknownChunk(String),

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
            core::PortalError::UnknownChunk { name } => PortalError::UnknownChunk(name),
            core::PortalError::WrongFrameSize { expected, got } => {
                PortalError::WrongFrameSize { expected: expected as u64, got: got as u64 }
            }
            core::PortalError::InvalidFrameDimensions { width, height } => {
                PortalError::InvalidFrameDimensions { width, height }
            }
            core::PortalError::Deserialization(s) => PortalError::Deserialization(s),
            core::PortalError::Codec(s) => PortalError::Codec(s),
            core::PortalError::WrongRole(r) => PortalError::WrongRole(r.into()),
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
    /// Fires for every chunk received. Bindings dispatch by `chunk.name`
    /// to per-chunk user callbacks if needed.
    fn on_action_chunk(&self, chunk: ActionChunk);
    /// Fires when an operator joins the room (post role-attribute discovery).
    fn on_operator_joined(&self, identity: String);
    /// Fires when an operator leaves the room. The robot's
    /// `active_operator` pointer is **not** auto-cleared on disconnect.
    fn on_operator_left(&self, identity: String);
    /// Fires when the robot's `active_operator` attribute changes (or, on
    /// the Robot side, when the local pointer is updated). Empty string
    /// means the pointer was cleared.
    fn on_active_operator_changed(&self, identity: Option<String>);
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
    pub fn add_video(
        &self,
        name: String,
        codec: VideoCodec,
        quality: u8,
        max_bitrate_kbps: Option<u32>,
    ) {
        self.inner.lock().add_video(name, codec.into(), quality, max_bitrate_kbps);
    }

    pub fn add_state_typed(&self, schema: Vec<FieldSpec>) {
        self.inner
            .lock()
            .add_state_typed(schema.into_iter().map(|f| (f.name, f.dtype.into())));
    }

    pub fn add_action_typed(&self, schema: Vec<FieldSpec>) {
        self.inner
            .lock()
            .add_action_typed(schema.into_iter().map(|f| (f.name, f.dtype.into())));
    }

    /// Declare a named action chunk: a fixed-horizon batch of typed
    /// per-field values published as one byte stream. Use this for VLA
    /// policies that emit a horizon of future actions per inference step.
    pub fn add_action_chunk(&self, name: String, horizon: u32, fields: Vec<FieldSpec>) {
        self.inner.lock().add_action_chunk(
            name,
            horizon,
            fields.into_iter().map(|f| (f.name, f.dtype.into())),
        );
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

    pub fn set_ping_ms(&self, ms: u64) {
        self.inner.lock().set_ping_ms(ms);
    }

    pub fn set_reuse_stale_frames(&self, enable: bool) {
        self.inner.lock().set_reuse_stale_frames(enable);
    }

    pub fn set_e2ee_key(&self, key: Vec<u8>) {
        self.inner.lock().set_e2ee_key(key);
    }

    /// Operator-side opt-in to receiving executed actions ("HITL
    /// recording"). Off by default. When on, `on_action` / `on_action_chunk`
    /// / `get_action` / `get_action_chunk` fire on the operator for actions
    /// the active operator sends, plus a local echo when self == active.
    /// No-op on the Robot side — the robot always processes actions.
    pub fn set_action_subscription(&self, enable: bool) {
        self.inner.lock().set_action_subscription(enable);
    }

    /// Declared WebRTC video track names (H264).
    pub fn video_tracks(&self) -> Vec<String> {
        self.inner.lock().video_track_names().map(String::from).collect()
    }

    /// Declared byte-stream video tracks (Raw / Png / Mjpeg) with codec
    /// and per-codec quality.
    pub fn frame_video_tracks(&self) -> Vec<FrameVideoSpec> {
        self.inner
            .lock()
            .frame_video_tracks()
            .iter()
            .map(|s| FrameVideoSpec {
                name: s.name.clone(),
                codec: s.codec.into(),
                quality: s.quality,
            })
            .collect()
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

    /// Declared action chunks. Used by bindings that load a config from
    /// YAML and need to mirror chunk schemas into their own typed-state
    /// tracking after construction.
    pub fn action_chunks(&self) -> Vec<ChunkSpec> {
        self.inner.lock().action_chunks().iter().map(chunkspec_from_core).collect()
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
    frame_video_tracks: Vec<FrameVideoSpec>,
    action_chunks: Vec<ChunkSpec>,
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
        let frame_video_tracks: Vec<FrameVideoSpec> = cfg
            .frame_video_tracks()
            .iter()
            .map(|s| FrameVideoSpec {
                name: s.name.clone(),
                codec: s.codec.into(),
                quality: s.quality,
            })
            .collect();
        let action_chunks: Vec<ChunkSpec> = cfg
            .action_chunks()
            .iter()
            .map(chunkspec_from_core)
            .collect();

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
            });
        });
        let cb = callbacks.clone();
        inner.on_state(move |state| {
            cb.on_state(State {
                values: state.raw_values.clone(),
                timestamp_us: state.timestamp_us,
            });
        });
        let cb = callbacks.clone();
        inner.on_observation(move |obs| {
            cb.on_observation(observation_from_core(obs));
        });
        let cb = callbacks.clone();
        inner.on_drop(move |dropped| {
            // Cross with raw f64 maps. Python wraps to typed on receipt.
            let raw: Vec<HashMap<String, f64>> = dropped
                .into_iter()
                .map(|m| {
                    m.into_iter().map(|(k, v)| (k, v.as_f64())).collect()
                })
                .collect();
            cb.on_drop(raw);
        });
        // Register `on_video_frame` for every declared track regardless of
        // transport. Frame-video tracks share the same `VideoTrackSlots` map
        // with WebRTC tracks on the core side, so a single registration
        // surface works for both — the foreign side only sees one
        // `on_video_frame(track, frame)` event stream per Portal.
        for track in video_tracks.iter().chain(frame_video_tracks.iter().map(|s| &s.name)) {
            let cb = callbacks.clone();
            let track_name = track.clone();
            inner.on_video_frame(track, move |_name, frame| {
                cb.on_video_frame(track_name.clone(), frame_from_core(frame));
            });
        }

        for chunk_spec in &action_chunks {
            let cb = callbacks.clone();
            inner.on_action_chunk(&chunk_spec.name, move |chunk| {
                cb.on_action_chunk(actionchunk_from_core(chunk));
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

        Arc::new(Self {
            inner,
            _callbacks: callbacks,
            state_fields,
            action_fields,
            video_tracks,
            frame_video_tracks,
            action_chunks,
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
        self.inner
            .send_action(&typed, timestamp_us, in_reply_to_ts_us)
            .map_err(Into::into)
    }

    /// Publish an action chunk on the named declaration. `data` is
    /// `field -> column of length horizon` widened to `f64`.
    pub fn send_action_chunk(
        &self,
        chunk_name: String,
        data: HashMap<String, Vec<f64>>,
        timestamp_us: Option<u64>,
        in_reply_to_ts_us: Option<u64>,
    ) -> PortalResult<()> {
        self.inner
            .send_action_chunk(&chunk_name, &data, timestamp_us, in_reply_to_ts_us)
            .map_err(Into::into)
    }

    pub fn get_observation(&self) -> Option<Observation> {
        self.inner.get_observation().as_ref().map(observation_from_core)
    }

    pub fn get_action(&self) -> Option<Action> {
        self.inner.get_action().map(|a| Action {
            values: a.raw_values,
            timestamp_us: a.timestamp_us,
            in_reply_to_ts_us: a.in_reply_to_ts_us,
            sender: a.sender,
        })
    }

    pub fn get_action_chunk(&self, chunk_name: String) -> Option<ActionChunk> {
        self.inner.get_action_chunk(&chunk_name).map(|c| actionchunk_from_core(&c))
    }

    pub fn get_state(&self) -> Option<State> {
        self.inner.get_state().map(|s| State {
            values: s.raw_values,
            timestamp_us: s.timestamp_us,
        })
    }

    pub fn get_video_frame(&self, track_name: String) -> Option<VideoFrame> {
        self.inner.get_video_frame(&track_name).as_ref().map(frame_from_core)
    }

    pub fn metrics(&self) -> PortalMetrics {
        metrics_from_core(self.inner.metrics())
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

    pub fn video_tracks(&self) -> Vec<String> {
        self.video_tracks.clone()
    }

    /// Declared frame-video tracks (name + codec + quality), in declaration
    /// order. Frame-video tracks ride a byte-stream channel rather than the
    /// WebRTC media path; the user-facing send/receive API is the same.
    pub fn frame_video_tracks(&self) -> Vec<FrameVideoSpec> {
        self.frame_video_tracks.clone()
    }

    pub fn action_chunks(&self) -> Vec<ChunkSpec> {
        self.action_chunks.clone()
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
// Conversions from core types. Records own their data, so we copy frame
// bytes out of the core's `Arc<[u8]>` into `Vec<u8>` at the boundary.
// ---------------------------------------------------------------------------

fn frame_from_core(f: &core::VideoFrameData) -> VideoFrame {
    VideoFrame {
        width: f.width,
        height: f.height,
        data: f.data.to_vec(),
        timestamp_us: f.timestamp_us,
    }
}

fn chunkspec_from_core(c: &core::ChunkSpec) -> ChunkSpec {
    ChunkSpec {
        name: c.name.clone(),
        horizon: c.horizon,
        fields: c
            .fields
            .iter()
            .map(|f| FieldSpec { name: f.name.clone(), dtype: f.dtype.into() })
            .collect(),
    }
}

fn actionchunk_from_core(c: &core::ActionChunk) -> ActionChunk {
    ActionChunk {
        name: c.name.clone(),
        horizon: c.horizon,
        data: c.data.clone(),
        timestamp_us: c.timestamp_us,
        in_reply_to_ts_us: c.in_reply_to_ts_us,
        sender: c.sender.clone(),
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
            action_chunks_sent: m.transport.action_chunks_sent,
            action_chunks_received: m.transport.action_chunks_received,
            frame_jitter_us: m.transport.frame_jitter_us,
            state_jitter_us: m.transport.state_jitter_us,
            action_jitter_us: m.transport.action_jitter_us,
            action_chunk_jitter_us: m.transport.action_chunk_jitter_us,
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
        policy: PolicyMetrics {
            e2e_us_p50: m.policy.e2e_us_p50,
            e2e_us_p95: m.policy.e2e_us_p95,
            correlated_received: m.policy.correlated_received,
        },
    }
}
