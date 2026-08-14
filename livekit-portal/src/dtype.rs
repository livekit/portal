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

use crate::error::{PortalError, PortalResult};

/// Declared type for a single state or action field.
///
/// State and action schemas carry one `DType` per field. The declared type
/// drives wire-format width and value reconstruction. Values are carried as
/// `f64` through the Rust core and cast to the declared dtype only at the
/// serialization boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl DType {
    /// Stable variant name, for error messages. Matches the `TypedValue`
    /// variant that carries this dtype so `DtypeMismatch` reads the same
    /// whether it came from a scalar value or a chunk column.
    pub fn variant_name(self) -> &'static str {
        match self {
            DType::F64 => "F64",
            DType::F32 => "F32",
            DType::I32 => "I32",
            DType::I16 => "I16",
            DType::I8 => "I8",
            DType::U32 => "U32",
            DType::U16 => "U16",
            DType::U8 => "U8",
            DType::Bool => "Bool",
        }
    }

    /// Byte width on the wire.
    pub fn size_bytes(self) -> usize {
        match self {
            DType::F64 => 8,
            DType::F32 | DType::I32 | DType::U32 => 4,
            DType::I16 | DType::U16 => 2,
            DType::I8 | DType::U8 | DType::Bool => 1,
        }
    }

    /// Encode an `f64` into `out` at the declared width. Returns `true` when
    /// the value was lossy beyond ordinary float rounding: an integer dtype
    /// saturated, or `Bool` received `NaN`. `F64` never reports lossy. `F32`
    /// reports lossy only if the value is finite and outside `f32` range.
    /// Caller uses the flag to emit a rate-limited warning.
    pub(crate) fn encode(self, v: f64, out: &mut Vec<u8>) -> bool {
        match self {
            DType::F64 => {
                out.extend_from_slice(&v.to_le_bytes());
                false
            }
            DType::F32 => {
                let f = v as f32;
                out.extend_from_slice(&f.to_le_bytes());
                v.is_finite() && !f.is_finite()
            }
            DType::I32 => {
                let (x, sat) = saturate_i32(v);
                out.extend_from_slice(&x.to_le_bytes());
                sat
            }
            DType::I16 => {
                let (x, sat) = saturate_i16(v);
                out.extend_from_slice(&x.to_le_bytes());
                sat
            }
            DType::I8 => {
                let (x, sat) = saturate_i8(v);
                out.extend_from_slice(&x.to_le_bytes());
                sat
            }
            DType::U32 => {
                let (x, sat) = saturate_u32(v);
                out.extend_from_slice(&x.to_le_bytes());
                sat
            }
            DType::U16 => {
                let (x, sat) = saturate_u16(v);
                out.extend_from_slice(&x.to_le_bytes());
                sat
            }
            DType::U8 => {
                let (x, sat) = saturate_u8(v);
                out.extend_from_slice(&x.to_le_bytes());
                sat
            }
            DType::Bool => {
                let saturated = v.is_nan();
                out.push(if v != 0.0 && !v.is_nan() { 1 } else { 0 });
                saturated
            }
        }
    }

    /// Decode `buf[..self.size_bytes()]` into an `f64`.
    pub(crate) fn decode(self, buf: &[u8]) -> PortalResult<f64> {
        let need = self.size_bytes();
        if buf.len() < need {
            return Err(PortalError::Deserialization(format!(
                "dtype {self:?} needs {need} bytes, got {}",
                buf.len()
            )));
        }
        Ok(match self {
            DType::F64 => f64::from_le_bytes(buf[..8].try_into().unwrap()),
            DType::F32 => f32::from_le_bytes(buf[..4].try_into().unwrap()) as f64,
            DType::I32 => i32::from_le_bytes(buf[..4].try_into().unwrap()) as f64,
            DType::I16 => i16::from_le_bytes(buf[..2].try_into().unwrap()) as f64,
            DType::I8 => i8::from_le_bytes([buf[0]]) as f64,
            DType::U32 => u32::from_le_bytes(buf[..4].try_into().unwrap()) as f64,
            DType::U16 => u16::from_le_bytes(buf[..2].try_into().unwrap()) as f64,
            DType::U8 => buf[0] as f64,
            DType::Bool => {
                if buf[0] == 0 {
                    0.0
                } else {
                    1.0
                }
            }
        })
    }
}

/// Saturating cast `f64 -> signed-int T`. Returns `(value, saturated?)`.
/// `NaN` maps to 0 and is reported as saturated. Values exactly equal to
/// `T::MIN` or `T::MAX` are representable and *not* reported.
macro_rules! saturate_signed {
    ($name:ident, $t:ty) => {
        fn $name(v: f64) -> ($t, bool) {
            if v.is_nan() {
                (0, true)
            } else if v > <$t>::MAX as f64 {
                (<$t>::MAX, true)
            } else if v < <$t>::MIN as f64 {
                (<$t>::MIN, true)
            } else {
                (v as $t, false)
            }
        }
    };
}

/// Saturating cast `f64 -> unsigned-int T`. Negative and `NaN` inputs clamp
/// to 0 and are reported as saturated. Zero and exact `T::MAX` are
/// representable and not reported.
macro_rules! saturate_unsigned {
    ($name:ident, $t:ty) => {
        fn $name(v: f64) -> ($t, bool) {
            if v.is_nan() {
                (0, true)
            } else if v < 0.0 {
                (0, true)
            } else if v > <$t>::MAX as f64 {
                (<$t>::MAX, true)
            } else {
                (v as $t, false)
            }
        }
    };
}

saturate_signed!(saturate_i32, i32);
saturate_signed!(saturate_i16, i16);
saturate_signed!(saturate_i8, i8);
saturate_unsigned!(saturate_u32, u32);
saturate_unsigned!(saturate_u16, u16);
saturate_unsigned!(saturate_u8, u8);

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_decode(dtype: DType, v: f64) -> (f64, bool) {
        let mut buf = Vec::new();
        let saturated = dtype.encode(v, &mut buf);
        assert_eq!(buf.len(), dtype.size_bytes());
        (dtype.decode(&buf).unwrap(), saturated)
    }

    #[test]
    fn f64_roundtrip() {
        let (v, sat) = encode_decode(DType::F64, 3.14159265358979);
        assert_eq!(v, 3.14159265358979);
        assert!(!sat);
    }

    #[test]
    fn f32_reports_overflow() {
        let (v, sat) = encode_decode(DType::F32, 1.5);
        assert_eq!(v, 1.5);
        assert!(!sat);
        let (v, sat) = encode_decode(DType::F32, 1e40);
        assert!(v.is_infinite());
        assert!(sat, "f32 overflow must be reported as saturation");
    }

    #[test]
    fn int_saturates_on_overflow() {
        assert_eq!(encode_decode(DType::I8, 500.0), (127.0, true));
        assert_eq!(encode_decode(DType::I8, -500.0), (-128.0, true));
        assert_eq!(encode_decode(DType::U8, -5.0), (0.0, true));
        assert_eq!(encode_decode(DType::U8, 1000.0), (255.0, true));
        assert_eq!(encode_decode(DType::I8, 42.0), (42.0, false));
    }

    #[test]
    fn int_boundary_values_are_not_reported() {
        // Exact MIN/MAX are representable and should not flag.
        assert_eq!(encode_decode(DType::I8, 127.0), (127.0, false));
        assert_eq!(encode_decode(DType::I8, -128.0), (-128.0, false));
        assert_eq!(encode_decode(DType::U16, 65535.0), (65535.0, false));
        assert_eq!(encode_decode(DType::U8, 0.0), (0.0, false));
    }

    #[test]
    fn bool_maps_zero_and_nonzero() {
        assert_eq!(encode_decode(DType::Bool, 0.0), (0.0, false));
        assert_eq!(encode_decode(DType::Bool, 1.0), (1.0, false));
        assert_eq!(encode_decode(DType::Bool, -3.2), (1.0, false));
        let (v, sat) = encode_decode(DType::Bool, f64::NAN);
        assert_eq!(v, 0.0);
        assert!(sat, "NaN into Bool must be reported as saturation");
    }

    #[test]
    fn decode_buffer_too_short() {
        let buf = [0u8; 3];
        assert!(DType::F64.decode(&buf).is_err());
    }
}
