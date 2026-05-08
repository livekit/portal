use std::collections::HashMap;

use bytes::Bytes;

use crate::config::FieldSpec;
use crate::dtype::DType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Robot,
    Operator,
}

/// A value received on the wire, reconstructed to its declared dtype.
///
/// The core pipeline widens every value to `f64` for carry-forward and
/// buffering — every supported integer dtype fits in `f64`'s 53-bit
/// mantissa, so that widening is lossless. `TypedValue` is the
/// presentation form handed to user code: the dtype is preserved, so a
/// `BOOL` field arrives as `Bool(true)`, not `F64(1.0)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TypedValue {
    F64(f64),
    F32(f32),
    I32(i32),
    I16(i16),
    I8(i8),
    U32(u32),
    U16(u16),
    U8(u8),
    Bool(bool),
}

impl TypedValue {
    /// Construct from an `f64` per the declared dtype. The pipeline hands
    /// every value to this method at delivery; by that point the value
    /// has already been round-tripped through `DType::encode`/`decode`
    /// and lies in range for the dtype, so this is a straight cast.
    ///
    /// Rust's `as` cast from `f64` to an integer is saturating (Rust
    /// 1.45+): out-of-range values clamp to the integer's bounds and
    /// `NaN` becomes `0`.
    ///
    /// Exposed publicly so language bindings that receive an `f64` map
    /// across the FFI (e.g. UniFFI) can adapt it back into `TypedValue`
    /// for typed on-receive paths.
    pub fn from_f64(v: f64, dtype: DType) -> Self {
        match dtype {
            DType::F64 => TypedValue::F64(v),
            DType::F32 => TypedValue::F32(v as f32),
            DType::I32 => TypedValue::I32(v as i32),
            DType::I16 => TypedValue::I16(v as i16),
            DType::I8 => TypedValue::I8(v as i8),
            DType::U32 => TypedValue::U32(v as u32),
            DType::U16 => TypedValue::U16(v as u16),
            DType::U8 => TypedValue::U8(v as u8),
            DType::Bool => TypedValue::Bool(v != 0.0 && !v.is_nan()),
        }
    }

    /// The `DType` tag matching this variant — lets callers check a
    /// typed value against a declared schema.
    pub fn dtype(self) -> DType {
        match self {
            TypedValue::F64(_) => DType::F64,
            TypedValue::F32(_) => DType::F32,
            TypedValue::I32(_) => DType::I32,
            TypedValue::I16(_) => DType::I16,
            TypedValue::I8(_) => DType::I8,
            TypedValue::U32(_) => DType::U32,
            TypedValue::U16(_) => DType::U16,
            TypedValue::U8(_) => DType::U8,
            TypedValue::Bool(_) => DType::Bool,
        }
    }

    /// Static name of the variant, for error messages.
    pub fn variant_name(self) -> &'static str {
        match self {
            TypedValue::F64(_) => "F64",
            TypedValue::F32(_) => "F32",
            TypedValue::I32(_) => "I32",
            TypedValue::I16(_) => "I16",
            TypedValue::I8(_) => "I8",
            TypedValue::U32(_) => "U32",
            TypedValue::U16(_) => "U16",
            TypedValue::U8(_) => "U8",
            TypedValue::Bool(_) => "Bool",
        }
    }

    /// Lossless widening back to `f64`. Useful when a consumer wants to
    /// treat every field uniformly (e.g. writing into an `ndarray`).
    pub fn as_f64(self) -> f64 {
        match self {
            TypedValue::F64(v) => v,
            TypedValue::F32(v) => v as f64,
            TypedValue::I32(v) => v as f64,
            TypedValue::I16(v) => v as f64,
            TypedValue::I8(v) => v as f64,
            TypedValue::U32(v) => v as f64,
            TypedValue::U16(v) => v as f64,
            TypedValue::U8(v) => v as f64,
            TypedValue::Bool(v) => if v { 1.0 } else { 0.0 },
        }
    }
}

impl From<TypedValue> for f64 {
    fn from(v: TypedValue) -> Self {
        v.as_f64()
    }
}

/// One action received from the operator. Surfaces in `on_action` and
/// `Portal::get_action`.
#[derive(Debug, Clone)]
pub struct Action {
    /// Field name to typed value per the declared action schema.
    pub values: HashMap<String, TypedValue>,
    /// The same payload widened to `f64` — every dtype's lossless
    /// representation on the pipeline. Useful when you want to write into
    /// a numeric buffer without matching on each variant.
    pub raw_values: HashMap<String, f64>,
    pub timestamp_us: u64,
    /// Sender-side observation timestamp this action was produced from,
    /// when the operator passed one to `send_action`. `None` means the
    /// action was published unsolicited (no observation it answers to).
    /// Used to derive end-to-end policy latency (`metrics.policy.e2e_us_*`).
    pub in_reply_to_ts_us: Option<u64>,
    /// Identity of the operator that produced this action, captured at the
    /// moment the multi-controller gate accepted the packet. `None` on
    /// paths that bypass the gate (v0.1 unified `Portal` with
    /// `multi_controller` off, or operator-side echo before the
    /// active-operator pointer is set). Recording / shadow-eval code
    /// should use this field rather than `Portal::active_operator()` to
    /// label rows so the label cannot race with a handoff.
    pub sender: Option<String>,
}

/// One action chunk received from the operator. Surfaces in
/// `on_action_chunk` / `Portal::get_action_chunk`.
///
/// The shape is `[horizon, fields]` row-major: timestep `t` of field `f` is at
/// `data[&f][t as usize]`. Each per-field column has length `horizon`. Fields
/// keep their declared dtype on the wire and are widened to `f64` here for
/// uniformity — bindings re-cast at egress.
///
/// **Why a chunk type, not just an Action?** VLA policies emit a horizon of
/// future actions per inference step. Packing them as scalars would either
/// require many `send_action` calls (one per timestep) or hand-rolled
/// side-channel binary, defeating the schema. A first-class chunk lets the
/// schema describe the tensor and lets the wire ship it as one packet.
#[derive(Debug, Clone)]
pub struct ActionChunk {
    /// Chunk name as declared in `add_action_chunk`.
    pub name: String,
    /// Number of timesteps (length of every per-field column).
    pub horizon: u32,
    /// Per-field column, length `horizon`, dtype widened to `f64`.
    pub data: HashMap<String, Vec<f64>>,
    pub timestamp_us: u64,
    /// Sender-side observation timestamp this chunk was produced from, when
    /// the operator passed one to `send_action_chunk`. `None` means the
    /// chunk was published unsolicited.
    pub in_reply_to_ts_us: Option<u64>,
    /// Identity of the operator that produced this chunk, captured at the
    /// moment the multi-controller gate accepted the byte stream. See the
    /// note on `Action::sender` — same semantics.
    pub sender: Option<String>,
}

/// One state sample received from the robot. Surfaces in `on_state` and
/// `Portal::get_state`.
#[derive(Debug, Clone)]
pub struct State {
    pub values: HashMap<String, TypedValue>,
    pub raw_values: HashMap<String, f64>,
    pub timestamp_us: u64,
}

/// A synchronized observation: one state matched with one frame from every
/// registered video track.
#[derive(Debug, Clone)]
pub struct Observation {
    /// Typed per the declared state schema.
    pub state: HashMap<String, TypedValue>,
    /// Same payload as `state`, widened to `f64` (lossless).
    pub raw_state: HashMap<String, f64>,
    pub frames: HashMap<String, VideoFrameData>,
    pub timestamp_us: u64,
}

/// Decoded video frame. `data` is packed RGB24 (R,G,B byte order, `W*H*3`
/// bytes) regardless of transport — WebRTC frames are color-converted from
/// I420 on receive, frame-video frames are decoded back to RGB by the
/// codec.
///
/// `data` is `bytes::Bytes` rather than `Arc<[u8]>` so that frame-video
/// receive can carry a zero-copy view into the byte-stream payload (Raw
/// codec — `Bytes::slice` is a refcount bump, not a memcpy). Cloning a
/// `Bytes` is the same single-atomic refcount bump `Arc<[u8]>` would do.
#[derive(Debug, Clone)]
pub struct VideoFrameData {
    pub width: u32,
    pub height: u32,
    pub data: Bytes,
    pub timestamp_us: u64,
}

/// Internal sync configuration, derived from `PortalConfig` knobs.
#[derive(Debug, Clone, Copy)]
pub struct SyncConfig {
    pub video_buffer_size: u32,
    pub state_buffer_size: u32,
    pub search_range_us: u64,
    /// When true, a state whose video match window has elapsed reuses the
    /// most recently emitted frame on that track instead of being dropped.
    /// Video effectively "freezes" during frame loss while state keeps
    /// flowing — every state becomes an observation once every track has
    /// emitted at least once. Default is `false`, preserving the strict
    /// drop-on-horizon behavior.
    pub reuse_stale_frames: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            video_buffer_size: 5,    // ~83ms at 60fps
            state_buffer_size: 5,    // ~83ms at 60fps
            search_range_us: 10_000, // 10ms — half a frame interval at 60fps
            reuse_stale_frames: false,
        }
    }
}

/// Build `(typed, raw)` maps from an ordered schema and its values. Both
/// maps are returned so delivery records can carry typed *and* raw views
/// without rebuilding either on access.
pub(crate) fn to_value_maps(
    schema: &[FieldSpec],
    values: &[f64],
) -> (HashMap<String, TypedValue>, HashMap<String, f64>) {
    let mut typed = HashMap::with_capacity(schema.len());
    let mut raw = HashMap::with_capacity(schema.len());
    for (f, v) in schema.iter().zip(values.iter()) {
        typed.insert(f.name.clone(), TypedValue::from_f64(*v, f.dtype));
        raw.insert(f.name.clone(), *v);
    }
    (typed, raw)
}

// `From<primitive> for TypedValue` impls so callers can build typed maps
// ergonomically with `.into()` rather than spelling the variant:
//     let mut m: HashMap<String, TypedValue> = HashMap::new();
//     m.insert("gripper".into(), true.into());
//     m.insert("shoulder".into(), 0.5f32.into());

impl From<f64> for TypedValue {
    fn from(v: f64) -> Self { TypedValue::F64(v) }
}
impl From<f32> for TypedValue {
    fn from(v: f32) -> Self { TypedValue::F32(v) }
}
impl From<i32> for TypedValue {
    fn from(v: i32) -> Self { TypedValue::I32(v) }
}
impl From<i16> for TypedValue {
    fn from(v: i16) -> Self { TypedValue::I16(v) }
}
impl From<i8> for TypedValue {
    fn from(v: i8) -> Self { TypedValue::I8(v) }
}
impl From<u32> for TypedValue {
    fn from(v: u32) -> Self { TypedValue::U32(v) }
}
impl From<u16> for TypedValue {
    fn from(v: u16) -> Self { TypedValue::U16(v) }
}
impl From<u8> for TypedValue {
    fn from(v: u8) -> Self { TypedValue::U8(v) }
}
impl From<bool> for TypedValue {
    fn from(v: bool) -> Self { TypedValue::Bool(v) }
}
