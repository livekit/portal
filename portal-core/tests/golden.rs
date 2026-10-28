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

//! Golden wire vectors. `fixtures/golden.json` pins the exact bytes Portal
//! puts on the wire for known inputs, so any other implementation (the
//! browser shell) can check itself against the same file. Regenerate with
//! `PORTAL_UPDATE_GOLDEN=1 cargo test -p portal-core --test golden` only
//! when the wire format is meant to change, which also means bumping
//! `PROTOCOL_VERSION`.

use std::path::PathBuf;

use portal_core::clock::ClockPacket;
use portal_core::serialization::{
    action_fingerprint, deserialize_action, deserialize_values, schema_fingerprint,
    serialize_action, serialize_values,
};
use portal_core::{DType, FieldSpec, keypoint};
use serde_json::{Value, json};

const ALL_DTYPES: [DType; 9] = [
    DType::F64,
    DType::F32,
    DType::I32,
    DType::I16,
    DType::I8,
    DType::U32,
    DType::U16,
    DType::U8,
    DType::Bool,
];

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden.json")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

fn dtype(name: &str) -> DType {
    ALL_DTYPES.into_iter().find(|d| d.variant_name() == name).expect("known dtype")
}

fn schema(fields: &[(&str, DType)]) -> Vec<FieldSpec> {
    fields.iter().map(|(n, d)| FieldSpec::new(*n, *d)).collect()
}

fn schema_json(schema: &[FieldSpec]) -> Value {
    schema.iter().map(|f| json!([f.name, f.dtype.variant_name()])).collect()
}

fn schema_from_json(v: &Value) -> Vec<FieldSpec> {
    v.as_array()
        .expect("schema array")
        .iter()
        .map(|f| FieldSpec::new(f[0].as_str().expect("name"), dtype(f[1].as_str().expect("dtype"))))
        .collect()
}

fn values_from_json(v: &Value) -> Vec<f64> {
    v.as_array().expect("values").iter().map(|x| x.as_f64().expect("number")).collect()
}

/// Every dtype once, plus the same schema with out-of-range values to pin
/// the saturating casts.
fn mixed_schema() -> Vec<FieldSpec> {
    schema(&[
        ("x", DType::F64),
        ("y", DType::F32),
        ("i32", DType::I32),
        ("i16", DType::I16),
        ("i8", DType::I8),
        ("u32", DType::U32),
        ("u16", DType::U16),
        ("u8", DType::U8),
        ("gripper", DType::Bool),
    ])
}

fn state_vector(name: &str, schema: Vec<FieldSpec>, ts: u64, values: Vec<f64>) -> Value {
    let fp = schema_fingerprint(&schema);
    let out = serialize_values(fp, ts, &values, &schema);
    json!({
        "name": name,
        "schema": schema_json(&schema),
        "fingerprint": fp,
        "timestamp_us": ts,
        "values": values,
        "saturated": out.saturated_indices,
        "hex": hex(&out.payload),
    })
}

fn action_vector(
    name: &str,
    schema: Vec<FieldSpec>,
    ts: u64,
    in_reply_to: Option<u64>,
    values: Vec<f64>,
) -> Value {
    let fp = action_fingerprint(&schema);
    let out = serialize_action(fp, ts, in_reply_to, &values, &schema);
    json!({
        "name": name,
        "schema": schema_json(&schema),
        "fingerprint": fp,
        "timestamp_us": ts,
        "in_reply_to_ts_us": in_reply_to,
        "values": values,
        "saturated": out.saturated_indices,
        "hex": hex(&out.payload),
    })
}

fn keypoint_vector(kind: &str, payload: Value, ts: u64) -> Value {
    let map = payload.as_object().expect("object").clone();
    json!({
        "type": kind,
        "payload": payload,
        "timestamp_us": ts,
        "hex": hex(&keypoint::encode(kind, &map, ts)),
    })
}

fn clock_vector(packet: ClockPacket) -> Value {
    let hex = hex(&packet.encode());
    match packet {
        ClockPacket::Ping { seq, t1 } => {
            json!({ "kind": "ping", "seq": seq, "t1": t1, "hex": hex })
        }
        ClockPacket::Pong { seq, t1, t2, t3 } => {
            json!({ "kind": "pong", "seq": seq, "t1": t1, "t2": t2, "t3": t3, "hex": hex })
        }
    }
}

const TS: u64 = 1_767_225_600_123_456;

fn generate() -> Value {
    let joints =
        schema(&[("shoulder", DType::F32), ("elbow", DType::F32), ("gripper", DType::Bool)]);
    json!({
        "topics": {
            "state": "portal_state",
            "action": "portal_action",
            "clock": "portal_clock",
            "keypoint": "portal_keypoint",
        },
        "state": [
            state_vector("empty", Vec::new(), TS, Vec::new()),
            state_vector("joints", joints.clone(), TS, vec![0.5, -1.25, 1.0]),
            state_vector(
                "every_dtype",
                mixed_schema(),
                TS,
                vec![1.0 / 3.0, 2.5, -123_456.0, -300.0, -7.0, 4_000_000_000.0, 65_535.0, 255.0, 0.0],
            ),
            state_vector(
                "saturated",
                mixed_schema(),
                TS,
                vec![f64::MAX, 1e40, 1e12, 1e6, -1e6, -1.0, 1e9, 300.0, 2.0],
            ),
        ],
        "action": [
            action_vector("uncorrelated", joints.clone(), TS, None, vec![0.5, -1.25, 0.0]),
            action_vector("correlated", joints, TS, Some(TS - 33_000), vec![0.5, -1.25, 1.0]),
        ],
        "keypoint": [
            keypoint_vector("idle", json!({}), TS),
            keypoint_vector(
                "recording",
                json!({ "task_description": "pick the blue cube 🧊", "nested": { "ok": true, "scores": [1, 2.5, null] } }),
                TS,
            ),
        ],
        "clock": [
            clock_vector(ClockPacket::Ping { seq: 7, t1: TS }),
            clock_vector(ClockPacket::Pong { seq: u32::MAX, t1: TS, t2: TS + 1_500, t3: TS + 1_750 }),
        ],
    })
}

#[test]
fn wire_matches_golden_vectors() {
    let generated = generate();
    if std::env::var_os("PORTAL_UPDATE_GOLDEN").is_some() {
        let text = serde_json::to_string_pretty(&generated).expect("json") + "\n";
        std::fs::write(fixture_path(), text).expect("write fixture");
    }
    let text = std::fs::read_to_string(fixture_path()).expect("fixture present");
    let fixture: Value = serde_json::from_str(&text).expect("fixture json");
    assert_eq!(generated, fixture, "wire bytes changed; see the module docs");
}

/// Decodes every fixture independently of `generate`, so the file alone
/// specifies the format.
#[test]
fn golden_vectors_decode() {
    let text = std::fs::read_to_string(fixture_path()).expect("fixture present");
    let fixture: Value = serde_json::from_str(&text).expect("fixture json");

    for v in fixture["state"].as_array().expect("state") {
        let schema = schema_from_json(&v["schema"]);
        let fp = v["fingerprint"].as_u64().expect("fp") as u32;
        assert_eq!(fp, schema_fingerprint(&schema));
        let (ts, values) = deserialize_values(&unhex(v["hex"].as_str().expect("hex")), fp, &schema)
            .unwrap_or_else(|_| panic!("decode {}", v["name"]));
        assert_eq!(ts, v["timestamp_us"].as_u64().expect("ts"));
        if v["saturated"].as_array().expect("saturated").is_empty() {
            assert_eq!(values.len(), schema.len());
            for ((got, want), f) in values.iter().zip(values_from_json(&v["values"])).zip(&schema) {
                let want = match f.dtype {
                    DType::F32 => want as f32 as f64,
                    _ => want,
                };
                assert_eq!(*got, want, "{} field {}", v["name"], f.name);
            }
        }
    }

    for v in fixture["action"].as_array().expect("action") {
        let schema = schema_from_json(&v["schema"]);
        let fp = v["fingerprint"].as_u64().expect("fp") as u32;
        assert_eq!(fp, action_fingerprint(&schema));
        let (ts, reply, values) =
            deserialize_action(&unhex(v["hex"].as_str().expect("hex")), fp, &schema)
                .unwrap_or_else(|_| panic!("decode {}", v["name"]));
        assert_eq!(ts, v["timestamp_us"].as_u64().expect("ts"));
        assert_eq!(reply, v["in_reply_to_ts_us"].as_u64());
        assert_eq!(values, values_from_json(&v["values"]));
    }

    for v in fixture["keypoint"].as_array().expect("keypoint") {
        let kp = keypoint::decode(&unhex(v["hex"].as_str().expect("hex")), "peer".into())
            .expect("keypoint decodes");
        assert_eq!(kp.kind, v["type"].as_str().expect("type"));
        assert_eq!(Value::Object(kp.payload), v["payload"]);
        assert_eq!(kp.timestamp_us, v["timestamp_us"].as_u64().expect("ts"));
    }

    for v in fixture["clock"].as_array().expect("clock") {
        let packet = ClockPacket::decode(&unhex(v["hex"].as_str().expect("hex"))).expect("decodes");
        let field = |k: &str| v[k].as_u64().expect("u64");
        let expected = match v["kind"].as_str().expect("kind") {
            "ping" => ClockPacket::Ping { seq: field("seq") as u32, t1: field("t1") },
            _ => ClockPacket::Pong {
                seq: field("seq") as u32,
                t1: field("t1"),
                t2: field("t2"),
                t3: field("t3"),
            },
        };
        assert_eq!(packet, expected);
    }
}
