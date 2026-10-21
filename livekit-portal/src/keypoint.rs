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

//! Keypoints: free-form `{type, payload}` annotations on the robot's clock.
//! Portal delivers and records them but never interprets either field.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) const KEYPOINT_TOPIC: &str = "portal_keypoint";

/// An annotation. `kind` is the keypoint's `type`, renamed because `type`
/// is a Rust keyword.
#[derive(Debug, Clone, PartialEq)]
pub struct Keypoint {
    pub kind: String,
    pub payload: Map<String, Value>,
    /// Where the mark belongs, on the robot's clock.
    pub timestamp_us: u64,
    /// Identity of the participant that sent it.
    pub sender: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Map<String, Value>,
    timestamp_us: u64,
}

pub(crate) fn encode(kind: &str, payload: &Map<String, Value>, timestamp_us: u64) -> Vec<u8> {
    #[derive(Serialize)]
    struct WireRef<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        payload: &'a Map<String, Value>,
        timestamp_us: u64,
    }
    serde_json::to_vec(&WireRef { kind, payload, timestamp_us })
        .expect("a JSON map with string keys always serializes")
}

pub(crate) fn decode(bytes: &[u8], sender: String) -> Result<Keypoint, serde_json::Error> {
    let Wire { kind, payload, timestamp_us } = serde_json::from_slice(bytes)?;
    Ok(Keypoint { kind, payload, timestamp_us, sender })
}

type KeypointCb = Box<dyn Fn(&Keypoint) + Send + Sync>;

/// Push-only delivery for keypoints, shared with the room event loop.
pub(crate) struct KeypointSlot {
    cb: Mutex<Option<KeypointCb>>,
    /// Senders already warned about for a malformed packet, so a broken peer
    /// logs once instead of on every packet.
    warned: Mutex<HashSet<String>>,
}

impl KeypointSlot {
    pub fn new() -> Self {
        Self { cb: Mutex::new(None), warned: Mutex::new(HashSet::new()) }
    }

    pub fn set_callback(&self, cb: KeypointCb) {
        *self.cb.lock() = Some(cb);
    }

    pub fn deliver(&self, keypoint: &Keypoint) {
        if let Some(cb) = self.cb.lock().as_ref()
            && catch_unwind(AssertUnwindSafe(|| cb(keypoint))).is_err()
        {
            log::error!("[callback-panic] keypoint callback panicked, event loop continues");
        }
    }

    pub fn handle_packet(&self, bytes: &[u8], sender: String) {
        match decode(bytes, sender.clone()) {
            Ok(keypoint) => self.deliver(&keypoint),
            Err(e) => {
                if self.warned.lock().insert(sender.clone()) {
                    log::warn!("[bad-payload] keypoint from '{sender}' is not valid: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn roundtrip_keeps_type_payload_and_timestamp() {
        let payload = map(serde_json::json!({
            "task_description": "pick the blue cube 🧊",
            "nested": { "scores": [1, 2.5, null], "ok": true },
        }));
        let bytes = encode("recording", &payload, 42);
        let kp = decode(&bytes, "teleop".into()).unwrap();
        assert_eq!(
            kp,
            Keypoint {
                kind: "recording".into(),
                payload,
                timestamp_us: 42,
                sender: "teleop".into()
            }
        );
    }

    #[test]
    fn wire_uses_type_as_the_key() {
        let bytes = encode("idle", &Map::new(), 7);
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json, serde_json::json!({ "type": "idle", "payload": {}, "timestamp_us": 7 }));
    }

    #[test]
    fn payload_defaults_to_empty_and_extra_fields_are_ignored() {
        let kp =
            decode(br#"{"type":"idle","timestamp_us":1,"future":"field"}"#, "x".into()).unwrap();
        assert!(kp.payload.is_empty());
    }

    #[test]
    fn malformed_packets_are_rejected() {
        for bad in [
            &b""[..],
            b"not json",
            br#"{"payload":{},"timestamp_us":1}"#,
            br#"{"type":"idle"}"#,
            br#"{"type":"idle","payload":[1,2],"timestamp_us":1}"#,
            br#"{"type":"idle","payload":{},"timestamp_us":-1}"#,
        ] {
            assert!(decode(bad, "x".into()).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn slot_warns_once_per_sender_and_keeps_delivering() {
        let slot = KeypointSlot::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        slot.set_callback(Box::new(move |kp| sink.lock().push(kp.kind.clone())));

        slot.handle_packet(b"garbage", "broken".into());
        slot.handle_packet(b"garbage", "broken".into());
        slot.handle_packet(&encode("idle", &Map::new(), 1), "teleop".into());

        assert_eq!(*seen.lock(), vec!["idle".to_string()]);
        assert_eq!(slot.warned.lock().len(), 1);
    }

    #[test]
    fn a_panicking_callback_does_not_propagate() {
        let slot = KeypointSlot::new();
        slot.set_callback(Box::new(|_| panic!("boom")));
        slot.handle_packet(&encode("idle", &Map::new(), 1), "teleop".into());
    }
}
