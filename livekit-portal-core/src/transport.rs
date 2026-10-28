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

//! The transport model the core speaks: peers publish named streams,
//! subscribers receive them tagged with the publisher, and every publish
//! carries a delivery intent. A transport declares what it can do natively
//! in `TransportCaps`; the core polyfills the rest.
//!
//! See `docs/transports.md` for how LiveKit, the loopback and MoQ map onto
//! this.

use crate::config::PortalConfig;
use crate::error::{PortalError, PortalResult};
use crate::serialization::{action_packet_len, state_packet_len};
use crate::types::Role;

/// A peer's identity, as the transport authenticated it.
///
/// **Trust:** the core treats every `PeerId` a transport hands it as
/// authenticated. The active-operator gate and the `set_active_operator`
/// check rest on this, so a transport must guarantee that a message tagged
/// `from: X` was published by X. LiveKit derives identities from the JWT
/// via the SFU; a MoQ relay must only let peer X publish under
/// `{session}/X/`.
pub type PeerId = String;

/// Wire protocol version every peer announces. Peers only recognise each
/// other when it matches, so a mixed-version session fails loudly instead
/// of exchanging packets neither side can read.
pub const PROTOCOL_VERSION: &str = "3";

/// A peer's named data streams. Video tracks are streams too, named
/// `video/{track}` (see `stream_path`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    State,
    Action,
    Clock,
    Keypoint,
    /// Polyfilled RPC requests and responses.
    Control,
    /// Polyfilled presence: role, protocol version, active operator.
    Presence,
}

impl Stream {
    pub fn name(self) -> &'static str {
        match self {
            Stream::State => "state",
            Stream::Action => "action",
            Stream::Clock => "clock",
            Stream::Keypoint => "keypoint",
            Stream::Control => "control",
            Stream::Presence => "presence",
        }
    }
}

/// Full name of a publisher-scoped stream: `{session}/{peer}/{stream}`,
/// where `stream` is a `Stream::name` or `video/{track}`. MoQ maps this 1:1
/// to namespace plus track name.
pub fn stream_path(session: &str, peer: &str, stream: &str) -> String {
    format!("{session}/{peer}/{stream}")
}

/// What a publisher needs from the transport for one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Every message, in order: actions, keypoints, control, RPC.
    Reliable,
    /// A newer value supersedes older ones, and a late subscriber starts
    /// from the latest: state, presence.
    LatestOnly,
    /// Drop rather than deliver later than this.
    BestEffort { max_age_ms: u32 },
}

/// How a transport carries video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCaps {
    /// Only RAW/PNG/MJPEG, as Portal frame envelopes on a byte stream
    /// (loopback, zenoh, plain QUIC).
    ByteFramesOnly,
    /// The transport takes raw frames and encodes them itself, carrying
    /// the timestamp in its own metadata (LiveKit/WebRTC).
    TransportEncoded,
    /// The driver encodes with its own encoder and carries the timestamp
    /// in Portal's envelope (future MoQ via WebCodecs/ffmpeg).
    DriverEncoded,
}

/// What a transport does natively. The core polyfills the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportCaps {
    pub video: VideoCaps,
    /// Request/response RPC. Without it the core runs RPC over `Control`.
    pub native_rpc: bool,
    /// Per-peer presence with join/leave. Without it the core publishes
    /// presence on `Presence` with heartbeats and times out silent peers.
    pub native_presence: bool,
    /// End-to-end encryption of every stream.
    pub e2ee: bool,
    /// Largest data message the transport carries.
    pub max_payload_bytes: usize,
}

impl TransportCaps {
    /// Reject a config this transport cannot honor. A WebRTC codec needs a
    /// transport that encodes video; an E2EE key needs a transport that
    /// encrypts, so plaintext is never sent silently.
    pub fn validate(&self, config: &PortalConfig) -> PortalResult<()> {
        if self.video == VideoCaps::ByteFramesOnly
            && let Some(spec) = config.video_tracks().iter().find(|s| s.is_webrtc())
        {
            return Err(PortalError::Unsupported(format!(
                "video track '{}' uses {:?}, a WebRTC codec, but this transport only carries \
                 byte frames; declare it as RAW, PNG or MJPEG",
                spec.name, spec.codec
            )));
        }
        if config.has_e2ee_key() && !self.e2ee {
            return Err(PortalError::Unsupported(
                "an E2EE key is set but this transport does not encrypt; refusing to send \
                 plaintext. Remove the key or use a transport with E2EE"
                    .into(),
            ));
        }
        for (stream, size) in [
            ("state", state_packet_len(config.state_schema())),
            ("action", action_packet_len(config.action_schema())),
        ] {
            if size > self.max_payload_bytes {
                return Err(PortalError::Unsupported(format!(
                    "{stream} packets are {size} bytes, over this transport's {} byte limit",
                    self.max_payload_bytes
                )));
            }
        }
        for spec in config.video_tracks() {
            let tuned = spec.simulcast || spec.screencast || spec.max_bitrate_kbps.is_some();
            if tuned && self.video != VideoCaps::TransportEncoded {
                log::warn!(
                    "[unsupported-option] video track '{}': simulcast, screencast and \
                     max_bitrate_kbps only apply to WebRTC transports and are ignored here",
                    spec.name
                );
            }
        }
        Ok(())
    }
}

/// What a peer announces about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Presence {
    /// `None` until the transport has seen one, or for a value this peer
    /// does not recognise.
    pub role: Option<Role>,
    pub protocol_version: Option<String>,
    /// Robot only: the operator whose actions it executes.
    pub active_operator: Option<PeerId>,
}

impl Presence {
    /// What a peer of `role` on this build announces.
    pub fn local(role: Role) -> Self {
        Self {
            role: Some(role),
            protocol_version: Some(PROTOCOL_VERSION.to_string()),
            active_operator: None,
        }
    }

    /// The role, if the peer speaks this build's protocol version.
    pub fn compatible_role(&self) -> Option<Role> {
        self.role.filter(|_| self.is_compatible())
    }

    pub fn is_compatible(&self) -> bool {
        self.protocol_version.as_deref() == Some(PROTOCOL_VERSION)
    }
}

/// Portal's names for roles, shared by every transport that spells a role
/// as a string.
pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Robot => "robot",
        Role::Operator => "operator",
        Role::Observer => "observer",
    }
}

pub fn parse_role(name: &str) -> Option<Role> {
    match name {
        "robot" => Some(Role::Robot),
        "operator" => Some(Role::Operator),
        "observer" => Some(Role::Observer),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Codec;
    use crate::config::VideoOptions;
    use crate::dtype::DType;

    fn caps(video: VideoCaps) -> TransportCaps {
        TransportCaps {
            video,
            native_rpc: false,
            native_presence: false,
            e2ee: false,
            max_payload_bytes: 64 * 1024,
        }
    }

    #[test]
    fn webrtc_codec_needs_an_encoding_transport() {
        let mut config = PortalConfig::new("s", Role::Robot);
        config.add_video("cam", Codec::H264, VideoOptions::default());
        let err = caps(VideoCaps::ByteFramesOnly).validate(&config).unwrap_err();
        assert!(err.to_string().contains("RAW, PNG or MJPEG"), "{err}");
        assert!(caps(VideoCaps::TransportEncoded).validate(&config).is_ok());
        assert!(caps(VideoCaps::DriverEncoded).validate(&config).is_ok());
    }

    #[test]
    fn e2ee_key_needs_an_encrypting_transport() {
        let mut config = PortalConfig::new("s", Role::Robot);
        config.set_e2ee_key(b"secret".to_vec());
        let err = caps(VideoCaps::ByteFramesOnly).validate(&config).unwrap_err();
        assert!(err.to_string().contains("plaintext"), "{err}");
        let encrypting = TransportCaps { e2ee: true, ..caps(VideoCaps::ByteFramesOnly) };
        assert!(encrypting.validate(&config).is_ok());
    }

    #[test]
    fn oversized_packets_are_rejected() {
        let mut config = PortalConfig::new("s", Role::Robot);
        config.add_state_typed((0..20).map(|i| (format!("j{i}"), DType::F64)));
        let tiny = TransportCaps { max_payload_bytes: 100, ..caps(VideoCaps::ByteFramesOnly) };
        let err = tiny.validate(&config).unwrap_err();
        assert!(err.to_string().contains("state packets are 172 bytes"), "{err}");
    }

    #[test]
    fn presence_needs_the_same_protocol_version() {
        let mut p = Presence::local(Role::Operator);
        assert_eq!(p.compatible_role(), Some(Role::Operator));
        p.protocol_version = Some("2".into());
        assert_eq!(p.compatible_role(), None);
        for role in [Role::Robot, Role::Operator, Role::Observer] {
            assert_eq!(parse_role(role_name(role)), Some(role));
        }
        assert_eq!(parse_role("pilot"), None);
    }

    #[test]
    fn stream_paths_are_publisher_scoped() {
        assert_eq!(stream_path("lab", "robot", Stream::State.name()), "lab/robot/state");
        assert_eq!(stream_path("lab", "robot", "video/cam"), "lab/robot/video/cam");
    }
}
