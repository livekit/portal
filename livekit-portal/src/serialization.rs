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

use crate::config::FieldSpec;
use crate::dtype::DType;
use crate::error::PortalError;

/// Plain-stream wire prefix: 4-byte schema fingerprint + 8-byte
/// `timestamp_us`. Used by `portal_state`.
const HEADER_LEN: usize = 4 + 8;

/// Correlated-stream wire prefix: plain header plus an 8-byte
/// `in_reply_to_ts_us` slot. `0` = no correlation. Used by `portal_action`.
const CORRELATED_HEADER_LEN: usize = HEADER_LEN + 8;

/// Mixed into the action stream's fingerprint so a v2 peer rejects a v1
/// peer's wire (which lacks the `in_reply_to_ts_us` slot) instead of
/// silently mis-parsing 8 bytes of payload as timing metadata. Symmetric:
/// both sides apply the same xor, so v2-to-v2 still agrees.
const ACTION_STREAM_TAG: u32 = 0xa1c0_b001;

/// Stable 32-bit fingerprint of a state/action schema (ordered names +
/// dtype tags). Used at runtime to detect peers whose schemas disagree.
///
/// FNV-1a over `name_bytes, 0xff, dtype_tag, 0xff` per field. Not
/// cryptographic; collision odds at ~4e9 inputs are negligible for this
/// use.
pub(crate) fn schema_fingerprint(schema: &[FieldSpec]) -> u32 {
    const FNV_OFFSET: u32 = 0x811c9dc5;
    const FNV_PRIME: u32 = 0x01000193;
    let mut h = FNV_OFFSET;
    for f in schema {
        for byte in f.name.as_bytes() {
            h ^= *byte as u32;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h ^= 0xff;
        h = h.wrapping_mul(FNV_PRIME);
        h ^= dtype_tag(f.dtype) as u32;
        h = h.wrapping_mul(FNV_PRIME);
        h ^= 0xff;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Action stream's fingerprint: schema fingerprint xor'd with the action
/// stream tag. Locks both peers to the same wire format version (header
/// includes `in_reply_to_ts_us`) — a peer running an older Portal where
/// actions had no correlation slot would xor a different tag (or none),
/// fingerprints disagree, and the receive path drops cleanly.
pub(crate) fn action_fingerprint(schema: &[FieldSpec]) -> u32 {
    schema_fingerprint(schema) ^ ACTION_STREAM_TAG
}

/// Stable on-wire/hash tag for a dtype. Never renumber — changes break
/// cross-peer fingerprint agreement.
fn dtype_tag(d: DType) -> u8 {
    match d {
        DType::F64 => 1,
        DType::F32 => 2,
        DType::I32 => 3,
        DType::I16 => 4,
        DType::I8 => 5,
        DType::U32 => 6,
        DType::U16 => 7,
        DType::U8 => 8,
        DType::Bool => 9,
    }
}

/// Outcome of an encode pass: the packet bytes plus the names of fields
/// whose value saturated at the dtype boundary. Caller logs the latter.
pub(crate) struct EncodeResult {
    pub payload: Vec<u8>,
    pub saturated_indices: Vec<usize>,
}

/// Serialize state/action values with a timestamp against a dtype schema.
///
/// Wire format: `[u32 fingerprint][u64 timestamp_us][field0 bytes]...`,
/// all little-endian. Each field's byte width is the declared `DType`'s
/// `size_bytes()`. Caller must pass `values.len() == schema.len()`.
pub(crate) fn serialize_values(
    fingerprint: u32,
    timestamp_us: u64,
    values: &[f64],
    schema: &[FieldSpec],
) -> EncodeResult {
    debug_assert_eq!(values.len(), schema.len());
    let payload_bytes: usize = schema.iter().map(|f| f.dtype.size_bytes()).sum();
    let mut buf = Vec::with_capacity(HEADER_LEN + payload_bytes);
    buf.extend_from_slice(&fingerprint.to_le_bytes());
    buf.extend_from_slice(&timestamp_us.to_le_bytes());
    let mut saturated_indices = Vec::new();
    for (i, (v, field)) in values.iter().zip(schema.iter()).enumerate() {
        if field.dtype.encode(*v, &mut buf) {
            saturated_indices.push(i);
        }
    }
    EncodeResult { payload: buf, saturated_indices }
}

/// Serialize an action with its optional `in_reply_to_ts_us` correlation,
/// using the action stream fingerprint and 20-byte header. `None` is
/// encoded as `0_u64` on the wire — bindings turn it back into `None` on
/// receive. The zero sentinel is fine because timestamps are µs since the
/// Unix epoch and never zero in practice.
pub(crate) fn serialize_action(
    fingerprint: u32,
    timestamp_us: u64,
    in_reply_to_ts_us: Option<u64>,
    values: &[f64],
    schema: &[FieldSpec],
) -> EncodeResult {
    debug_assert_eq!(values.len(), schema.len());
    let payload_bytes: usize = schema.iter().map(|f| f.dtype.size_bytes()).sum();
    let mut buf = Vec::with_capacity(CORRELATED_HEADER_LEN + payload_bytes);
    buf.extend_from_slice(&fingerprint.to_le_bytes());
    buf.extend_from_slice(&timestamp_us.to_le_bytes());
    buf.extend_from_slice(&in_reply_to_ts_us.unwrap_or(0).to_le_bytes());
    let mut saturated_indices = Vec::new();
    for (i, (v, field)) in values.iter().zip(schema.iter()).enumerate() {
        if field.dtype.encode(*v, &mut buf) {
            saturated_indices.push(i);
        }
    }
    EncodeResult { payload: buf, saturated_indices }
}

/// Deserialize an action wire packet. Returns the timestamp, the optional
/// correlation, and the ordered values.
#[allow(clippy::type_complexity)]
pub(crate) fn deserialize_action(
    data: &[u8],
    fingerprint: u32,
    schema: &[FieldSpec],
) -> Result<(u64, Option<u64>, Vec<f64>), DecodeError> {
    if data.len() < CORRELATED_HEADER_LEN {
        return Err(DecodeError::Malformed(PortalError::Deserialization(format!(
            "action packet shorter than {CORRELATED_HEADER_LEN}-byte header: got {}",
            data.len()
        ))));
    }
    let fp_got = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if fp_got != fingerprint {
        return Err(DecodeError::SchemaMismatch { expected: fingerprint, got: fp_got });
    }
    let payload_bytes: usize = schema.iter().map(|f| f.dtype.size_bytes()).sum();
    let expected_len = CORRELATED_HEADER_LEN + payload_bytes;
    if data.len() != expected_len {
        return Err(DecodeError::Malformed(PortalError::Deserialization(format!(
            "expected {} bytes, got {}",
            expected_len,
            data.len()
        ))));
    }
    let timestamp_us = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let reply_raw = u64::from_le_bytes(data[12..20].try_into().unwrap());
    let in_reply_to_ts_us = (reply_raw != 0).then_some(reply_raw);
    let mut values = Vec::with_capacity(schema.len());
    let mut offset = CORRELATED_HEADER_LEN;
    for f in schema.iter() {
        let width = f.dtype.size_bytes();
        values.push(f.dtype.decode(&data[offset..offset + width])?);
        offset += width;
    }
    Ok((timestamp_us, in_reply_to_ts_us, values))
}

/// Reasons a receive-side deserialize can fail. Split so the caller can
/// tell a schema-mismatch (worth a rate-limited warn) apart from a corrupt
/// packet (worth dropping silently or noisily).
#[derive(Debug)]
pub(crate) enum DecodeError {
    /// Packet's schema fingerprint does not match the local schema. Peers
    /// are out of sync.
    SchemaMismatch { expected: u32, got: u32 },
    /// Packet is shorter than the header or the schema's declared size.
    Malformed(PortalError),
}

impl From<PortalError> for DecodeError {
    fn from(e: PortalError) -> Self {
        DecodeError::Malformed(e)
    }
}

/// Deserialize bytes back to a timestamp and ordered values. Returns
/// `SchemaMismatch` when the embedded fingerprint disagrees with
/// `fingerprint`; the caller decides whether to warn, count, or drop.
pub(crate) fn deserialize_values(
    data: &[u8],
    fingerprint: u32,
    schema: &[FieldSpec],
) -> Result<(u64, Vec<f64>), DecodeError> {
    if data.len() < HEADER_LEN {
        return Err(DecodeError::Malformed(PortalError::Deserialization(format!(
            "packet shorter than {HEADER_LEN}-byte header: got {}",
            data.len()
        ))));
    }
    let fp_got = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if fp_got != fingerprint {
        return Err(DecodeError::SchemaMismatch { expected: fingerprint, got: fp_got });
    }
    let payload_bytes: usize = schema.iter().map(|f| f.dtype.size_bytes()).sum();
    let expected_len = HEADER_LEN + payload_bytes;
    if data.len() != expected_len {
        return Err(DecodeError::Malformed(PortalError::Deserialization(format!(
            "expected {} bytes, got {}",
            expected_len,
            data.len()
        ))));
    }
    let timestamp_us = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let mut values = Vec::with_capacity(schema.len());
    let mut offset = HEADER_LEN;
    for f in schema.iter() {
        let width = f.dtype.size_bytes();
        values.push(f.dtype.decode(&data[offset..offset + width])?);
        offset += width;
    }
    Ok((timestamp_us, values))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(pairs: &[(&str, DType)]) -> Vec<FieldSpec> {
        pairs.iter().map(|(n, d)| FieldSpec::new(*n, *d)).collect()
    }

    #[test]
    fn f64_roundtrip() {
        let s = schema(&[("a", DType::F64), ("b", DType::F64), ("c", DType::F64)]);
        let fp = schema_fingerprint(&s);
        let values = vec![1.0, 2.5, -3.25];
        let out = serialize_values(fp, 1_713_300_000_000, &values, &s);
        assert!(out.saturated_indices.is_empty());
        let (ts2, values2) = deserialize_values(&out.payload, fp, &s).unwrap();
        assert_eq!(ts2, 1_713_300_000_000);
        assert_eq!(values, values2);
    }

    #[test]
    fn mixed_dtype_roundtrip() {
        let s =
            schema(&[("f", DType::F32), ("i", DType::I8), ("b", DType::Bool), ("u", DType::U16)]);
        let fp = schema_fingerprint(&s);
        let values = vec![1.5, 127.0, 1.0, 65535.0];
        let out = serialize_values(fp, 42, &values, &s);
        assert_eq!(out.payload.len(), HEADER_LEN + 4 + 1 + 1 + 2);
        assert!(out.saturated_indices.is_empty());
        let (ts2, values2) = deserialize_values(&out.payload, fp, &s).unwrap();
        assert_eq!(ts2, 42);
        assert_eq!(values2, vec![1.5, 127.0, 1.0, 65535.0]);
    }

    #[test]
    fn empty_schema() {
        let s: Vec<FieldSpec> = Vec::new();
        let fp = schema_fingerprint(&s);
        let out = serialize_values(fp, 42, &[], &s);
        assert_eq!(out.payload.len(), HEADER_LEN);
        let (ts2, values2) = deserialize_values(&out.payload, fp, &s).unwrap();
        assert_eq!(ts2, 42);
        assert!(values2.is_empty());
    }

    #[test]
    fn wrong_length_errors() {
        let s = schema(&[("x", DType::F64)]);
        let fp = schema_fingerprint(&s);
        // Valid fingerprint header but missing payload bytes.
        let mut bytes = fp.to_le_bytes().to_vec();
        bytes.extend_from_slice(&42u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]); // f64 needs 8 bytes
        match deserialize_values(&bytes, fp, &s) {
            Err(DecodeError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_mismatch_is_separated() {
        let s = schema(&[("x", DType::F64)]);
        let fp = schema_fingerprint(&s);
        let out = serialize_values(fp, 0, &[1.0], &s);
        match deserialize_values(&out.payload, fp ^ 0x1, &s) {
            Err(DecodeError::SchemaMismatch { expected, got }) => {
                assert_eq!(expected, fp ^ 0x1);
                assert_eq!(got, fp);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_changes_when_order_changes() {
        let a = schema(&[("x", DType::F32), ("y", DType::I8)]);
        let b = schema(&[("y", DType::I8), ("x", DType::F32)]);
        assert_ne!(schema_fingerprint(&a), schema_fingerprint(&b));
    }

    #[test]
    fn fingerprint_changes_when_dtype_changes() {
        let a = schema(&[("x", DType::F32)]);
        let b = schema(&[("x", DType::F64)]);
        assert_ne!(schema_fingerprint(&a), schema_fingerprint(&b));
    }

    #[test]
    fn fingerprint_is_stable_across_independent_constructions() {
        // Two peers build their schemas separately; if the field names,
        // order, and dtypes agree, the u32 fingerprint must match —
        // otherwise the receive path drops every packet.
        let peer_robot =
            schema(&[("j1", DType::F32), ("gripper", DType::Bool), ("mode", DType::I8)]);
        let peer_operator =
            schema(&[("j1", DType::F32), ("gripper", DType::Bool), ("mode", DType::I8)]);
        assert_eq!(schema_fingerprint(&peer_robot), schema_fingerprint(&peer_operator),);
    }

    #[test]
    fn fingerprint_changes_when_name_changes() {
        // Rename is the subtle bug: same position, same dtype, different
        // spelling. Must fail fingerprint comparison so the peer rejects
        // packets instead of silently misattributing values.
        let a = schema(&[("shoulder", DType::F32)]);
        let b = schema(&[("shouder", DType::F32)]);
        assert_ne!(schema_fingerprint(&a), schema_fingerprint(&b));
    }

    #[test]
    fn saturation_is_reported() {
        let s = schema(&[("x", DType::I8)]);
        let fp = schema_fingerprint(&s);
        let out = serialize_values(fp, 0, &[500.0], &s);
        assert_eq!(out.saturated_indices, vec![0]);
        let (_, values) = deserialize_values(&out.payload, fp, &s).unwrap();
        assert_eq!(values, vec![127.0]);
    }

    // --- Action wire (with in_reply_to_ts_us) ------------------------------

    #[test]
    fn action_roundtrip_with_correlation() {
        let s = schema(&[("a", DType::F64), ("b", DType::F32)]);
        let fp = action_fingerprint(&s);
        let out = serialize_action(fp, 100, Some(50), &[1.0, 2.5], &s);
        let (ts, reply, values) = deserialize_action(&out.payload, fp, &s).unwrap();
        assert_eq!(ts, 100);
        assert_eq!(reply, Some(50));
        assert_eq!(values, vec![1.0, 2.5]);
    }

    #[test]
    fn action_roundtrip_without_correlation() {
        let s = schema(&[("a", DType::F64)]);
        let fp = action_fingerprint(&s);
        let out = serialize_action(fp, 200, None, &[3.0], &s);
        let (ts, reply, values) = deserialize_action(&out.payload, fp, &s).unwrap();
        assert_eq!(ts, 200);
        assert_eq!(reply, None);
        assert_eq!(values, vec![3.0]);
    }

    #[test]
    fn action_fingerprint_differs_from_state_fingerprint() {
        // Same schema must not produce the same fingerprint across the
        // two streams — otherwise an action with no correlation slot
        // could be silently mistaken for a state on a misrouted topic.
        let s = schema(&[("x", DType::F32)]);
        assert_ne!(schema_fingerprint(&s), action_fingerprint(&s));
    }

    #[test]
    fn action_wire_rejects_state_fingerprint() {
        // A peer sending the old state-style 12-byte header would have a
        // different fingerprint and the deserializer must reject it as
        // SchemaMismatch, not as Malformed.
        let s = schema(&[("x", DType::F64)]);
        let action_fp = action_fingerprint(&s);
        let out = serialize_values(schema_fingerprint(&s), 0, &[1.0], &s);
        match deserialize_action(&out.payload, action_fp, &s) {
            Err(DecodeError::SchemaMismatch { .. }) => {}
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }
}
