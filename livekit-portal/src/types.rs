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
        self.dtype().variant_name()
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
            TypedValue::Bool(v) => {
                if v {
                    1.0
                } else {
                    0.0
                }
            }
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
    /// Identity of the operator that produced this action, captured at
    /// the moment the active-operator gate accepted the packet (or, for
    /// the local echo path, the publisher's own identity). Recording and
    /// shadow-eval code should use this field rather than
    /// `Portal::active_operator()` to label rows so the label cannot
    /// race with a handoff.
    pub sender: String,
}

/// One column of an outgoing action chunk: `horizon` values widened to
/// `f64`, plus the dtype the caller claims they are.
///
/// **Why the claim.** A scalar action crosses `Portal::send_action` as a
/// `TypedValue`, whose variant states the caller's intent, and
/// `check_dtypes` compares that against the declared schema. Chunk columns
/// are `Vec<f64>` because a horizon of rows wants to stay a flat numeric
/// buffer, so the variant tag has nowhere to live. `dtype` is that tag,
/// hoisted to the column. It gives chunks the same send-time dtype
/// rejection scalar actions already get.
///
/// Note that the check compares declarations, not values, exactly as
/// `check_dtypes` does. Nothing here inspects the `f64`s. Out-of-range
/// values still saturate at encode and warn once per `(t, field)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkColumn {
    /// The column, length `horizon`. Short columns zero-pad and long ones
    /// truncate, both with a warn-once.
    pub values: Vec<f64>,
    /// The dtype the caller claims this column holds. `Some(d)` is checked
    /// against the field's declared dtype and a mismatch is rejected with
    /// `PortalError::DtypeMismatch`. `None` waives the check and coerces to
    /// the declared dtype, which is what a uniform policy tensor needs: a
    /// single `f32` array fanned out across a mixed schema cannot honestly
    /// claim `Bool` for the gripper column.
    pub dtype: Option<DType>,
}

impl ChunkColumn {
    /// A column that claims a dtype. Mismatches against the declared field
    /// are rejected at send.
    pub fn typed(dtype: DType, values: Vec<f64>) -> Self {
        Self { values, dtype: Some(dtype) }
    }

    /// A column that claims nothing and coerces to the declared dtype.
    pub fn untyped(values: Vec<f64>) -> Self {
        Self { values, dtype: None }
    }
}

impl From<Vec<f64>> for ChunkColumn {
    /// Bare columns coerce, matching the pre-`ChunkColumn` behaviour.
    fn from(values: Vec<f64>) -> Self {
        Self::untyped(values)
    }
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
    /// Identity of the operator that produced this chunk, captured at
    /// the moment the active-operator gate accepted the byte stream. See
    /// the note on `Action::sender` — same semantics.
    pub sender: String,
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

/// Where the pixels in a delivered frame came from. Frames arriving on the
/// raw video callbacks are always `Live`; the other two variants only ever
/// appear on frames inside an [`Observation`], where the sync buffer had to
/// resolve a track that could not be matched within its `max_lag` (see
/// `stall_behavior` in [`PortalConfig`](crate::PortalConfig)).
///
/// Check this before feeding an observation to a policy or writing it to a
/// dataset: `Stale` and `Omitted` frames are not measurements of the moment
/// they are attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// A real frame matched to this state within the tolerance window.
    Live,
    /// A real frame, but from an earlier moment — the track's last good
    /// frame, reused because nothing in range arrived (`stall_behavior: freeze`).
    /// `timestamp_us` is the frame's own, so the age is
    /// `observation.timestamp_us - frame.timestamp_us`.
    Stale,
    /// Not a camera frame at all: a synthesized placeholder standing in for
    /// a track that went silent (`stall_behavior: omit`). The key is still present
    /// so `frames[name]` never fails; the pixels carry a visible pattern.
    Omitted,
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
    /// Whether these pixels are a live match, a reused earlier frame, or a
    /// synthesized placeholder. Always `Live` outside of `Observation`.
    pub source: FrameSource,
}

/// What to do with a moment whose video track has gone silent past its
/// `max_lag`. Set per track via
/// [`set_stall_behavior`](crate::PortalConfig::set_stall_behavior).
///
/// All three are terminal: they describe how the moment is resolved once
/// the wait is over, not something that happens during it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StallBehavior {
    /// Emit no observation. The state is still delivered, on the drop
    /// callback. Nothing is fabricated — but the healthy tracks in that
    /// moment are discarded along with the silent one.
    #[default]
    Drop,
    /// Emit with the track's last good frame, tagged
    /// [`FrameSource::Stale`]. Video freezes on that track while state
    /// keeps flowing. Falls back to `Drop` before the track's first frame,
    /// when there is nothing to reuse.
    Freeze,
    /// Emit with a synthesized placeholder for the silent track, tagged
    /// [`FrameSource::Omitted`]. The map key is still present, so
    /// `frames[name]` never fails. Falls back to `Drop` before the track's
    /// first frame, when its frame geometry is not yet known.
    Omit,
}

/// Per-track stall handling: how long to wait for a silent track, and how
/// to resolve the moment when the wait is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StallConfig {
    /// How far the fastest-advancing stream may run past a moment — in
    /// sender-clock microseconds, never wall-clock — before that moment is
    /// resolved without this track.
    ///
    /// Evaluated when a packet arrives, not on a timer: if every stream
    /// goes silent nothing fires, because nothing is being emitted anyway.
    /// `None` (the default here) keeps the historical behavior of waiting
    /// until state-buffer capacity evicts the moment;
    /// [`PortalConfig`](crate::PortalConfig) derives a concrete value from
    /// `slack` and `fps` instead. `Some(0)` resolves immediately, without
    /// ever waiting.
    pub max_lag_us: Option<u64>,
    pub behavior: StallBehavior,
}

/// Internal sync configuration, derived from `PortalConfig` knobs.
#[derive(Debug, Clone, Copy)]
pub struct SyncConfig {
    pub video_buffer_size: u32,
    pub state_buffer_size: u32,
    pub search_range_us: u64,
    /// Stall handling for tracks with no per-track override. Defaults to
    /// the historical strict behavior: wait for capacity, then drop.
    pub default_stall: StallConfig,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            video_buffer_size: 5,    // ~83ms at 60fps
            state_buffer_size: 5,    // ~83ms at 60fps
            search_range_us: 10_000, // 10ms — half a frame interval at 60fps
            default_stall: StallConfig { max_lag_us: None, behavior: StallBehavior::Drop },
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
    fn from(v: f64) -> Self {
        TypedValue::F64(v)
    }
}
impl From<f32> for TypedValue {
    fn from(v: f32) -> Self {
        TypedValue::F32(v)
    }
}
impl From<i32> for TypedValue {
    fn from(v: i32) -> Self {
        TypedValue::I32(v)
    }
}
impl From<i16> for TypedValue {
    fn from(v: i16) -> Self {
        TypedValue::I16(v)
    }
}
impl From<i8> for TypedValue {
    fn from(v: i8) -> Self {
        TypedValue::I8(v)
    }
}
impl From<u32> for TypedValue {
    fn from(v: u32) -> Self {
        TypedValue::U32(v)
    }
}
impl From<u16> for TypedValue {
    fn from(v: u16) -> Self {
        TypedValue::U16(v)
    }
}
impl From<u8> for TypedValue {
    fn from(v: u8) -> Self {
        TypedValue::U8(v)
    }
}
impl From<bool> for TypedValue {
    fn from(v: bool) -> Self {
        TypedValue::Bool(v)
    }
}
