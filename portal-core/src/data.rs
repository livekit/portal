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

//! State and action packets: the publish side's carry-forward and dtype
//! checks, and the receive side's record building.

use std::collections::{HashMap, HashSet};

use crate::config::FieldSpec;
use crate::error::{PortalError, PortalResult};
use crate::metrics::DataStream;
use crate::serialization::{
    action_fingerprint, schema_fingerprint, serialize_action, serialize_values,
};
use crate::types::{Action, State, TypedValue, to_value_maps};

/// Reserved Portal topics. State and action travel as data packets.
pub const STATE_TOPIC: &str = "portal_state";
pub const ACTION_TOPIC: &str = "portal_action";

/// Encodes one stream's packets. Keeps the per-field carry-forward
/// snapshot, so it lives as long as the connection.
pub struct DataPublisher {
    schema: Vec<FieldSpec>,
    /// Precomputed schema fingerprint, embedded in every outgoing packet.
    fingerprint: u32,
    topic: &'static str,
    stream: DataStream,
    // Per-field snapshot of the last sent value, stored as f64 for lossless
    // carry-forward. `send_map` carries these forward when a caller supplies
    // only a subset of the declared fields, so partial updates stay
    // consistent with the robot's actual state. Seeded with 0.0, so fields
    // never sent resolve to 0.
    last_values: Vec<f64>,
    /// Field indices already reported as saturating. Each field warns at
    /// most once per publisher lifetime to keep the hot path quiet.
    warned_saturated: HashSet<usize>,
    /// Keys the caller sent that aren't in the schema. Logged once each so
    /// typos are visible without spamming per packet.
    warned_unknown_keys: HashSet<String>,
}

pub struct SendMap<'a> {
    pub values: &'a HashMap<String, TypedValue>,
    pub timestamp_us: u64,
    pub in_reply_to_ts_us: Option<u64>,
}

pub struct Encoded {
    pub payload: Vec<u8>,
    /// The post-carry-forward, post-saturation values a receiver will
    /// reconstruct after decode. The action self-echo uses this to record
    /// exactly what the robot will execute.
    pub wire_values: Vec<f64>,
}

impl DataPublisher {
    pub fn new(schema: &[FieldSpec], stream: DataStream) -> Self {
        let schema = schema.to_vec();
        let (fingerprint, topic) = match stream {
            DataStream::State => (schema_fingerprint(&schema), STATE_TOPIC),
            DataStream::Action => (action_fingerprint(&schema), ACTION_TOPIC),
        };
        let last_values = vec![0.0; schema.len()];
        Self {
            schema,
            fingerprint,
            topic,
            stream,
            last_values,
            warned_saturated: HashSet::new(),
            warned_unknown_keys: HashSet::new(),
        }
    }

    pub fn topic(&self) -> &'static str {
        self.topic
    }

    /// Encode from a map of typed values, reordering to declared field
    /// order. Missing fields inherit their last sent value (0.0 if never
    /// sent). Keys absent from the schema are logged once per key, then
    /// ignored.
    ///
    /// Each value's `TypedValue` variant must match the declared dtype for
    /// its field; a mismatch returns `PortalError::DtypeMismatch` and
    /// nothing is encoded, so a caller bug fails loud instead of silently
    /// round-tripping through an unintended cast.
    pub fn send_map(&mut self, input: SendMap<'_>) -> PortalResult<Encoded> {
        let SendMap { values, timestamp_us, in_reply_to_ts_us } = input;
        self.check_dtypes(values)?;
        self.warn_unknown_keys(values);
        apply_carry_forward(&self.schema, &mut self.last_values, values);
        let out = match self.stream {
            DataStream::State => {
                serialize_values(self.fingerprint, timestamp_us, &self.last_values, &self.schema)
            }
            DataStream::Action => serialize_action(
                self.fingerprint,
                timestamp_us,
                in_reply_to_ts_us,
                &self.last_values,
                &self.schema,
            ),
        };
        // Each value round-tripped through its declared dtype, so
        // out-of-range inputs reflect the saturated wire bytes.
        let wire_values = self
            .last_values
            .iter()
            .zip(self.schema.iter())
            .map(|(v, f)| TypedValue::from_f64(*v, f.dtype).as_f64())
            .collect();
        if !out.saturated_indices.is_empty() {
            self.warn_saturated(&out.saturated_indices);
        }
        Ok(Encoded { payload: out.payload, wire_values })
    }

    /// Reject on the first field whose `TypedValue` variant does not
    /// match the declared dtype.
    fn check_dtypes(&self, map: &HashMap<String, TypedValue>) -> PortalResult<()> {
        for field in &self.schema {
            if let Some(v) = map.get(&field.name)
                && v.dtype() != field.dtype
            {
                return Err(PortalError::DtypeMismatch {
                    field: field.name.clone(),
                    expected: field.dtype,
                    got: v.variant_name(),
                });
            }
        }
        Ok(())
    }

    fn warn_unknown_keys(&mut self, map: &HashMap<String, TypedValue>) {
        // Small schemas make a linear scan faster than a HashSet lookup.
        for key in map.keys() {
            if self.schema.iter().any(|f| &f.name == key) {
                continue;
            }
            if self.warned_unknown_keys.insert(key.clone()) {
                log::warn!(
                    "[unknown-field] topic '{}': field '{}' not in schema, ignored",
                    self.topic,
                    key
                );
            }
        }
    }

    fn warn_saturated(&mut self, indices: &[usize]) {
        for &i in indices {
            if self.warned_saturated.insert(i) {
                let field = &self.schema[i];
                log::warn!(
                    "[saturated] topic '{}': field '{}' clamped to {:?} range",
                    self.topic,
                    field.name,
                    field.dtype
                );
            }
        }
    }
}

/// Update `last` in place with values from `map` for each declared field,
/// leaving other slots untouched (carry-forward). Typed values are
/// lossless-widened to `f64` on the way in.
fn apply_carry_forward(schema: &[FieldSpec], last: &mut [f64], map: &HashMap<String, TypedValue>) {
    for (i, field) in schema.iter().enumerate() {
        if let Some(v) = map.get(&field.name) {
            last[i] = v.as_f64();
        }
    }
}

/// Who sent an action and whether the gate counted it as active, both
/// decided when the packet passed the gate.
pub struct ActionOrigin {
    pub sender: String,
    pub active: bool,
}

pub struct ActionRecord<'a> {
    pub timestamp_us: u64,
    pub in_reply_to_ts_us: Option<u64>,
    pub schema: &'a [FieldSpec],
    pub values: &'a [f64],
    pub origin: ActionOrigin,
}

pub fn build_action(record: ActionRecord<'_>) -> Action {
    let ActionRecord { timestamp_us, in_reply_to_ts_us, schema, values, origin } = record;
    let (typed, raw) = to_value_maps(schema, values);
    let ActionOrigin { sender, active } = origin;
    Action { values: typed, raw_values: raw, timestamp_us, in_reply_to_ts_us, sender, active }
}

pub fn build_state(timestamp_us: u64, schema: &[FieldSpec], values: &[f64]) -> State {
    let (typed, raw) = to_value_maps(schema, values);
    State { values: typed, raw_values: raw, timestamp_us }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtype::DType;
    use crate::serialization::{deserialize_action, deserialize_values};

    #[test]
    fn carry_forward_fills_missing_fields() {
        let schema = vec![
            FieldSpec::new("j1", DType::F64),
            FieldSpec::new("j2", DType::F64),
            FieldSpec::new("j3", DType::F64),
        ];
        let mut last = vec![0.0; 3];

        let m: HashMap<String, TypedValue> =
            [("j1".to_string(), TypedValue::F64(1.0))].into_iter().collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![1.0, 0.0, 0.0], "unsent fields start at seed (0.0)");

        let m: HashMap<String, TypedValue> =
            [("j2".to_string(), TypedValue::F64(2.5))].into_iter().collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![1.0, 2.5, 0.0], "j1 carries forward; j2 updates; j3 still at seed");

        let m: HashMap<String, TypedValue> =
            [("j1".to_string(), TypedValue::F64(-1.0)), ("j3".to_string(), TypedValue::F64(7.0))]
                .into_iter()
                .collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![-1.0, 2.5, 7.0], "j2 carries forward when omitted; others update");
    }

    #[test]
    fn check_dtypes_rejects_variant_mismatch() {
        let schema =
            vec![FieldSpec::new("gripper", DType::Bool), FieldSpec::new("mode", DType::I8)];
        let publisher = DataPublisher::new(&schema, DataStream::State);

        let m: HashMap<String, TypedValue> = [
            ("gripper".to_string(), TypedValue::Bool(true)),
            ("mode".to_string(), TypedValue::I8(3)),
        ]
        .into_iter()
        .collect();
        assert!(publisher.check_dtypes(&m).is_ok());

        let m: HashMap<String, TypedValue> =
            [("gripper".to_string(), TypedValue::F32(1.0))].into_iter().collect();
        let err = publisher.check_dtypes(&m).unwrap_err();
        assert!(matches!(
            err,
            PortalError::DtypeMismatch { ref field, expected: DType::Bool, got: "F32" } if field == "gripper"
        ));
    }

    #[test]
    fn typed_inputs_widen_to_f64_losslessly() {
        let schema = vec![
            FieldSpec::new("joint", DType::F32),
            FieldSpec::new("gripper", DType::Bool),
            FieldSpec::new("mode", DType::I8),
        ];
        let mut last = vec![0.0; 3];

        let m: HashMap<String, TypedValue> = [
            ("joint".to_string(), 0.5f32.into()),
            ("gripper".to_string(), true.into()),
            ("mode".to_string(), 3i8.into()),
        ]
        .into_iter()
        .collect();
        apply_carry_forward(&schema, &mut last, &m);
        assert_eq!(last, vec![0.5, 1.0, 3.0]);
    }

    #[test]
    fn dtype_mismatch_sends_nothing_and_keeps_carry_forward() {
        let schema = vec![FieldSpec::new("a", DType::F64), FieldSpec::new("b", DType::Bool)];
        let mut publisher = DataPublisher::new(&schema, DataStream::State);
        let good = HashMap::from([("a".to_string(), TypedValue::F64(1.0))]);
        publisher
            .send_map(SendMap { values: &good, timestamp_us: 1, in_reply_to_ts_us: None })
            .unwrap();
        let bad = HashMap::from([
            ("a".to_string(), TypedValue::F64(9.0)),
            ("b".to_string(), TypedValue::U8(1)),
        ]);
        assert!(
            publisher
                .send_map(SendMap { values: &bad, timestamp_us: 2, in_reply_to_ts_us: None })
                .is_err()
        );
        assert_eq!(publisher.last_values, vec![1.0, 0.0]);
    }

    #[test]
    fn wire_values_match_what_the_receiver_decodes() {
        let schema = vec![FieldSpec::new("u8", DType::U8), FieldSpec::new("x", DType::F32)];
        let fp = action_fingerprint(&schema);
        let mut publisher = DataPublisher::new(&schema, DataStream::Action);
        let values = HashMap::from([("u8".to_string(), TypedValue::U8(200))]);
        let first = publisher
            .send_map(SendMap { values: &values, timestamp_us: 10, in_reply_to_ts_us: Some(7) })
            .unwrap();
        let (ts, reply, decoded) = deserialize_action(&first.payload, fp, &schema).ok().unwrap();
        assert_eq!((ts, reply), (10, Some(7)));
        assert_eq!(decoded, first.wire_values);

        let state = DataPublisher::new(&schema, DataStream::State);
        assert_eq!(state.topic(), STATE_TOPIC);
        let mut state = state;
        let out = state
            .send_map(SendMap { values: &values, timestamp_us: 3, in_reply_to_ts_us: None })
            .unwrap();
        let (ts, decoded) =
            deserialize_values(&out.payload, schema_fingerprint(&schema), &schema).ok().unwrap();
        assert_eq!(ts, 3);
        assert_eq!(decoded, out.wire_values);
    }
}
