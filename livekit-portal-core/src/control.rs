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

//! Wire formats of the polyfills, for transports without native RPC or
//! presence. JSON, since both are low-rate and debugging them by eye is
//! worth more than the bytes. Transports only broadcast, so point-to-point
//! messages carry their addressee and receivers drop the rest.

use serde::{Deserialize, Serialize};

use crate::rpc::RpcError;
use crate::transport::{PeerId, Presence, parse_role, role_name};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    RpcRequest {
        /// Unique per caller; the reply echoes it.
        id: u64,
        to: PeerId,
        method: String,
        payload: String,
        timeout_ms: u32,
    },
    RpcResponse {
        id: u64,
        to: PeerId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WireRpcError>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRpcError {
    pub code: u32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

impl From<RpcError> for WireRpcError {
    fn from(e: RpcError) -> Self {
        Self { code: e.code, message: e.message, data: e.data }
    }
}

impl From<WireRpcError> for RpcError {
    fn from(e: WireRpcError) -> Self {
        RpcError::new(e.code, e.message, e.data)
    }
}

impl ControlMessage {
    pub fn response(id: u64, to: PeerId, result: Result<String, RpcError>) -> Self {
        match result {
            Ok(ok) => ControlMessage::RpcResponse { id, to, ok: Some(ok), error: None },
            Err(e) => ControlMessage::RpcResponse { id, to, ok: None, error: Some(e.into()) },
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("control messages always serialize")
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Presence on the `Presence` stream. A field this build does not know
/// (say, a role from a newer release) decodes as `None`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WirePresence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_operator: Option<String>,
}

pub fn encode_presence(presence: &Presence) -> Vec<u8> {
    let wire = WirePresence {
        role: presence.role.map(|r| role_name(r).to_string()),
        version: presence.protocol_version.clone(),
        active_operator: presence.active_operator.clone(),
    };
    serde_json::to_vec(&wire).expect("presence always serializes")
}

pub fn decode_presence(bytes: &[u8]) -> Option<Presence> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if !value.is_object() {
        return None;
    }
    let wire: WirePresence = serde_json::from_value(value).ok()?;
    Some(Presence {
        role: wire.role.as_deref().and_then(parse_role),
        protocol_version: wire.version,
        active_operator: wire.active_operator.filter(|id| !id.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    #[test]
    fn control_messages_roundtrip() {
        let messages = [
            ControlMessage::RpcRequest {
                id: 7,
                to: "robot".into(),
                method: "portal.set_active_operator".into(),
                payload: "alice".into(),
                timeout_ms: 15_000,
            },
            ControlMessage::response(7, "alice".into(), Ok(String::new())),
            ControlMessage::response(8, "bob".into(), Err(RpcError::new(2003, "no", None))),
        ];
        for m in messages {
            assert_eq!(ControlMessage::decode(&m.encode()), Some(m));
        }
        assert_eq!(ControlMessage::decode(b"{\"type\":\"gossip\"}"), None);
        assert_eq!(ControlMessage::decode(b"garbage"), None);
    }

    #[test]
    fn presence_roundtrips_and_tolerates_the_unknown() {
        let p = Presence { active_operator: Some("alice".into()), ..Presence::local(Role::Robot) };
        assert_eq!(decode_presence(&encode_presence(&p)), Some(p));
        let future = decode_presence(br#"{"role":"drone","version":"9","extra":1}"#).unwrap();
        assert_eq!((future.role, future.protocol_version.as_deref()), (None, Some("9")));
        assert_eq!(decode_presence(b"[]"), None);
    }
}
