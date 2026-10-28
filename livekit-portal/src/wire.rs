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

//! How Portal's transport model maps onto a LiveKit room: streams are data
//! topics (the sender is the publisher), presence is participant
//! attributes, RPC is LiveKit RPC, and video is a published track or a
//! byte stream per frame. Everything LiveKit names lives here, and none of
//! it may change without a `PROTOCOL_VERSION` bump.

use std::collections::HashMap;

use livekit_portal_core::transport::{
    PROTOCOL_VERSION, Presence, Stream, TransportCaps, VideoCaps, parse_role, role_name,
};
use livekit_portal_core::types::Role;

/// Participant attribute keys used by Portal for role discovery and the
/// multi-controller pointer. Namespaced to avoid colliding with any
/// application-level attributes the user may also be setting.
pub const ROLE_ATTR_KEY: &str = "lk.portal.role";
pub const ACTIVE_OPERATOR_ATTR_KEY: &str = "lk.portal.active_operator";
pub const PROTOCOL_VERSION_ATTR_KEY: &str = "lk.portal.version";

const STATE_TOPIC: &str = "portal_state";
const ACTION_TOPIC: &str = "portal_action";
const CLOCK_TOPIC: &str = "portal_clock";
const KEYPOINT_TOPIC: &str = "portal_keypoint";
/// One byte-stream topic multiplexes every frame-video track; the frame
/// envelope carries the track name.
pub const FRAME_VIDEO_TOPIC: &str = "portal_frame_video";

/// LiveKit byte streams fragment payloads into chunks of this many bytes.
/// Mirrors `livekit/src/room/data_stream/outgoing.rs::CHUNK_SIZE`. Exposed
/// so Portal users can do their own chunk-count math when picking a codec
/// — a payload above this size pays the per-chunk SCTP cost (~2 ms each
/// on localhost) for every additional chunk.
pub const BYTE_STREAM_CHUNK_SIZE: usize = 15_000;

/// A LiveKit room does everything natively, so none of the core's
/// polyfills run and the wire stays what it always was.
pub const CAPS: TransportCaps = TransportCaps {
    video: VideoCaps::TransportEncoded,
    native_rpc: true,
    native_presence: true,
    e2ee: true,
    // A data packet's ceiling on a default server.
    max_payload_bytes: 64 * 1024,
};

/// The topic a stream publishes on. Control and presence never reach the
/// wire on LiveKit: RPC and attributes are native.
pub fn topic(stream: Stream) -> Option<&'static str> {
    match stream {
        Stream::State => Some(STATE_TOPIC),
        Stream::Action => Some(ACTION_TOPIC),
        Stream::Clock => Some(CLOCK_TOPIC),
        Stream::Keypoint => Some(KEYPOINT_TOPIC),
        Stream::Control | Stream::Presence => None,
    }
}

pub fn stream(topic: &str) -> Option<Stream> {
    match topic {
        STATE_TOPIC => Some(Stream::State),
        ACTION_TOPIC => Some(Stream::Action),
        CLOCK_TOPIC => Some(Stream::Clock),
        KEYPOINT_TOPIC => Some(Stream::Keypoint),
        _ => None,
    }
}

/// What a participant announces, read from its attributes.
pub fn presence(attributes: &HashMap<String, String>) -> Presence {
    Presence {
        role: attributes.get(ROLE_ATTR_KEY).and_then(|r| parse_role(r)),
        protocol_version: attributes.get(PROTOCOL_VERSION_ATTR_KEY).cloned(),
        active_operator: active_operator(attributes),
    }
}

/// The active operator named by an attribute map, `None` when unset or
/// cleared (the empty string).
pub fn active_operator(attributes: &HashMap<String, String>) -> Option<String> {
    attributes.get(ACTIVE_OPERATOR_ATTR_KEY).filter(|v| !v.is_empty()).cloned()
}

/// Attributes a peer sets on itself at connect so others can discover it.
pub fn role_attributes(role: Role) -> HashMap<String, String> {
    HashMap::from([
        (ROLE_ATTR_KEY.to_string(), role_name(role).to_string()),
        (PROTOCOL_VERSION_ATTR_KEY.to_string(), PROTOCOL_VERSION.to_string()),
    ])
}

/// The one attribute a robot rewrites after connect: role and version never
/// change, so a presence update only carries the pointer, empty to clear.
pub fn active_operator_attributes(presence: &Presence) -> HashMap<String, String> {
    let value = presence.active_operator.clone().unwrap_or_default();
    HashMap::from([(ACTIVE_OPERATOR_ATTR_KEY.to_string(), value)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire is frozen: these names are what every deployed peer reads.
    #[test]
    fn names_on_the_wire_are_unchanged() {
        let topics = [Stream::State, Stream::Action, Stream::Clock, Stream::Keypoint].map(topic);
        assert_eq!(
            topics,
            [
                Some("portal_state"),
                Some("portal_action"),
                Some("portal_clock"),
                Some("portal_keypoint")
            ]
        );
        assert_eq!(FRAME_VIDEO_TOPIC, "portal_frame_video");
        assert_eq!(
            (ROLE_ATTR_KEY, ACTIVE_OPERATOR_ATTR_KEY, PROTOCOL_VERSION_ATTR_KEY),
            ("lk.portal.role", "lk.portal.active_operator", "lk.portal.version")
        );
        assert_eq!(PROTOCOL_VERSION, "3");
        for s in [Stream::State, Stream::Action, Stream::Clock, Stream::Keypoint] {
            assert_eq!(topic(s).and_then(stream), Some(s));
        }
        assert_eq!(topic(Stream::Control), None);
        assert_eq!(topic(Stream::Presence), None);
    }

    #[test]
    fn role_attributes_read_back_as_a_compatible_presence() {
        for role in [Role::Robot, Role::Operator, Role::Observer] {
            assert_eq!(presence(&role_attributes(role)).compatible_role(), Some(role));
        }
        let robot = role_attributes(Role::Robot)
            .into_iter()
            .chain([(ACTIVE_OPERATOR_ATTR_KEY.to_string(), "teleop".to_string())])
            .collect();
        assert_eq!(presence(&robot).active_operator.as_deref(), Some("teleop"));
    }

    #[test]
    fn missing_or_other_version_is_ignored() {
        let attrs = |role: &str, version: Option<&str>| {
            let mut attrs = HashMap::from([(ROLE_ATTR_KEY.to_string(), role.to_string())]);
            if let Some(v) = version {
                attrs.insert(PROTOCOL_VERSION_ATTR_KEY.to_string(), v.to_string());
            }
            attrs
        };
        assert_eq!(presence(&attrs("operator", None)).compatible_role(), None);
        assert_eq!(presence(&attrs("operator", Some("2"))).compatible_role(), None);
        assert_eq!(presence(&attrs("robot", Some(""))).compatible_role(), None);
        assert_eq!(presence(&HashMap::new()).compatible_role(), None);
    }

    #[test]
    fn an_empty_pointer_attribute_means_none() {
        let cleared = HashMap::from([(ACTIVE_OPERATOR_ATTR_KEY.to_string(), String::new())]);
        assert_eq!(active_operator(&cleared), None);
        let p = Presence { active_operator: None, ..Presence::local(Role::Robot) };
        assert_eq!(active_operator_attributes(&p)[ACTIVE_OPERATOR_ATTR_KEY], "");
    }

    /// The golden fixtures other implementations test against name the
    /// LiveKit topics too; they must agree with what this driver sends.
    #[test]
    fn topics_match_the_golden_fixtures() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../livekit-portal-core/tests/fixtures/golden.json"
        ))
        .expect("fixtures parse");
        for s in [Stream::State, Stream::Action, Stream::Clock, Stream::Keypoint] {
            assert_eq!(fixtures["topics"][s.name()].as_str(), topic(s), "{}", s.name());
        }
    }
}
