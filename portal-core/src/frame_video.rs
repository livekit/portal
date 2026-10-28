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

//! Frame-video transport: per-frame byte-stream publish/receive that
//! bypasses the WebRTC media path.
//!
//! WebRTC video is I420 + lossy codec. Policies trained on raw uint8 RGB get
//! a different input distribution after a Portal hop, which is a silent
//! correctness bug for inference. Frame-video tracks ship each frame
//! independently over a reliable byte stream so the receiver gets exactly
//! the bytes the publisher fed to the codec — RGB on both ends, with no
//! temporal-codec rewriting in between.
//!
//! Wire format (per byte stream, topic `portal_frame_video`):
//!
//! ```text
//! [u8  version = 1]
//! [u8  codec_id = 0|1|2 (Raw|Png|Mjpeg)]
//! [u16 width  little-endian]
//! [u16 height little-endian]
//! [u64 timestamp_us little-endian]
//! [u16 track_name_len little-endian]
//! [u8 × track_name_len  utf-8 bytes]
//! [u8 × N  encoded codec payload]
//! ```
//!
//! Width/height as u16 covers anything up to 65535, well beyond camera
//! frame dimensions. Track name is in the payload (one topic for all
//! frame-video tracks) so each Portal can declare multiple tracks
//! without paying for one topic subscription per track.
//!
//! One byte stream per frame. Frame loss is contained — a dropped stream
//! drops one frame, not state. Stream-level reliability is byte-level
//! (TCP-like under the hood). Frames either arrive whole or do not arrive.
//!
//! ## Latency floor
//!
//! LiveKit byte streams fragment payloads at `BYTE_STREAM_CHUNK_SIZE`
//! (15 KB) and ship each chunk through a single SCTP data-channel
//! sender shared by every byte-stream track. On localhost that channel
//! carries ~5 MB/s in total, bounded by SCTP send and libwebrtc's
//! `buffered_amount` drain rather than the SDK's writer loop.
//!
//! Implication for codec choice: the cost is per byte, not per frame.
//! Measured on 640×480 robot camera frames, MJPEG q90 (~60-85 KB) lands
//! in ~40 ms; PNG (~300-450 KB) and deflated Raw (~400 KB) in ~70-105 ms.
//! Pick by encoded size, not by quality knob. Numbers in
//! docs/07-frame-video.md.

use std::collections::HashMap;

use bytes::Bytes;

use crate::codec::{Codec, CodecResult, decode_frame, encode_frame_into, estimated_encoded_size};
use crate::config::PortalConfig;
use crate::error::{PortalError, PortalResult};
use crate::types::{FrameSource, VideoFrameData};

/// Reserved Portal topic for frame-video byte streams. A single topic
/// multiplexes all frame-video tracks; the per-frame header carries the
/// track name.
pub const FRAME_VIDEO_TOPIC: &str = "portal_frame_video";

/// LiveKit byte streams fragment payloads into chunks of this many bytes.
/// Mirrors `livekit/src/room/data_stream/outgoing.rs::CHUNK_SIZE`. Exposed
/// so Portal users can do their own chunk-count math when picking a codec
/// — a payload above this size pays the per-chunk SCTP cost (~2 ms each
/// on localhost) for every additional chunk.
pub const BYTE_STREAM_CHUNK_SIZE: usize = 15_000;

/// On-wire version of the per-frame header. Bumped when the layout
/// changes; receivers ignore frames with unknown versions instead of
/// crashing on malformed-looking field offsets.
const WIRE_VERSION: u8 = 1;

/// Fixed prefix of the per-frame header. The track name follows, sized
/// by the trailing `u16` of this prefix.
///   1 (version) + 1 (codec) + 2 (w) + 2 (h) + 8 (ts) + 2 (name_len) = 16
const HEADER_FIXED_LEN: usize = 16;

/// Cap on track-name length on the wire. The framing header allots a `u16`
/// (65535 bytes), but a name longer than this is almost certainly malformed
/// — capping early bounds the CPU a malicious or buggy peer can spend on
/// the receive path before we drop. Matches the typical LiveKit topic-name
/// budget.
const TRACK_NAME_MAX: usize = 256;

fn codec_id(c: Codec) -> u8 {
    match c {
        Codec::Raw => 0,
        Codec::Png => 1,
        Codec::Mjpeg => 2,
        Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::H265 => unreachable!(
            "WebRTC codecs ride the WebRTC media path, not the byte-stream wire format"
        ),
    }
}

fn codec_from_id(id: u8) -> Option<Codec> {
    match id {
        0 => Some(Codec::Raw),
        1 => Some(Codec::Png),
        2 => Some(Codec::Mjpeg),
        _ => None,
    }
}

/// One RGB24 frame to put on the wire.
pub struct RgbFrame<'a> {
    pub track_name: &'a str,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
    pub rgb: &'a [u8],
    pub quality: u8,
}

/// Codec encode a driver can swap in, e.g. a native JPEG encoder.
pub type EncodeFn = fn(&mut Vec<u8>, &[u8], u32, u32, Codec, u8) -> CodecResult<()>;

/// Build the on-wire payload for one frame with the core's own encoders.
pub fn build_frame_payload(frame: RgbFrame<'_>) -> PortalResult<Vec<u8>> {
    build_frame_payload_with(frame, encode_frame_into)
}

/// Build the on-wire payload for one frame: header + codec-encoded bytes,
/// in a single allocation. The encode writes directly into the buffer
/// after the header — no intermediate `Vec` and no second memcpy of the
/// payload (saves megabytes of bandwidth per frame for `Codec::Raw`).
pub fn build_frame_payload_with(frame: RgbFrame<'_>, encode: EncodeFn) -> PortalResult<Vec<u8>> {
    let RgbFrame { track_name, codec, width, height, timestamp_us, rgb, quality } = frame;
    // The wire header stores width/height as u16. Reject up front rather
    // than truncate. Distinct from `InvalidFrameDimensions` (which carries
    // the WebRTC-only "must be even" caveat) — frame video has no parity
    // constraint, only a u16 ceiling.
    if width > u16::MAX as u32 || height > u16::MAX as u32 {
        return Err(PortalError::Codec(format!(
            "frame_video dimensions {}x{} exceed u16 max ({})",
            width,
            height,
            u16::MAX
        )));
    }
    let name_bytes = track_name.as_bytes();
    if name_bytes.len() > TRACK_NAME_MAX {
        return Err(PortalError::Codec(format!(
            "frame_video track name too long: {} bytes (max {})",
            name_bytes.len(),
            TRACK_NAME_MAX
        )));
    }
    let header_len = HEADER_FIXED_LEN + name_bytes.len();
    let mut buf = Vec::with_capacity(header_len + estimated_encoded_size(width, height, codec));
    buf.push(WIRE_VERSION);
    buf.push(codec_id(codec));
    buf.extend_from_slice(&(width as u16).to_le_bytes());
    buf.extend_from_slice(&(height as u16).to_le_bytes());
    buf.extend_from_slice(&timestamp_us.to_le_bytes());
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    encode(&mut buf, rgb, width, height, codec, quality)
        .map_err(|e| PortalError::Codec(e.to_string()))?;
    Ok(buf)
}

/// Parsed view of one frame payload header. Borrows the track-name bytes
/// from the input wire payload so we don't allocate per frame; the codec
/// bytes are referenced separately so the receiver can slice them zero-copy
/// out of the underlying `Bytes`.
#[derive(Debug)]
pub struct DeserializedHeader<'a> {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
    pub track_name: &'a str,
    /// Byte offset into the input where the codec payload starts. The
    /// receiver uses this with `Bytes::slice` to hand the codec the
    /// payload as a refcounted view (no memcpy).
    pub payload_offset: usize,
}

pub fn deserialize_frame(bytes: &[u8]) -> Result<DeserializedHeader<'_>, &'static str> {
    if bytes.len() < HEADER_FIXED_LEN {
        return Err("frame_video payload shorter than fixed header");
    }
    if bytes[0] != WIRE_VERSION {
        return Err("frame_video payload has unknown version");
    }
    let codec = codec_from_id(bytes[1]).ok_or("unknown codec id")?;
    let width = u16::from_le_bytes([bytes[2], bytes[3]]) as u32;
    let height = u16::from_le_bytes([bytes[4], bytes[5]]) as u32;
    let timestamp_us = u64::from_le_bytes(bytes[6..14].try_into().expect("length checked above"));
    let name_len = u16::from_le_bytes([bytes[14], bytes[15]]) as usize;
    if name_len > TRACK_NAME_MAX {
        return Err("frame_video track name exceeds 256-byte cap");
    }
    let payload_offset = HEADER_FIXED_LEN + name_len;
    if bytes.len() < payload_offset {
        return Err("frame_video payload truncated mid-header");
    }
    let track_name = std::str::from_utf8(&bytes[HEADER_FIXED_LEN..payload_offset])
        .map_err(|_| "track name not valid utf-8")?;
    Ok(DeserializedHeader { codec, width, height, timestamp_us, track_name, payload_offset })
}

// --- Receive ---

/// One frame-video frame, decoded and checked against its declared track.
pub struct ReceivedFrame {
    pub track_name: String,
    pub frame: VideoFrameData,
    /// Bytes on the wire, header included.
    pub wire_len: usize,
}

/// Declared frame-video tracks, for checking and decoding received frames.
/// Immutable, so a driver can decode off its main loop.
pub struct FrameVideoTracks {
    codecs: HashMap<String, Codec>,
}

impl FrameVideoTracks {
    pub fn new(config: &PortalConfig) -> Self {
        let codecs = config.frame_video_tracks().map(|s| (s.name.clone(), s.codec)).collect();
        Self { codecs }
    }

    pub fn is_empty(&self) -> bool {
        self.codecs.is_empty()
    }

    /// Validate the header against the declared track and decode the codec
    /// bytes back to RGB. A mismatch (unknown track, codec disagreement,
    /// decode failure) drops the frame and warns.
    ///
    /// `payload` is consumed as `Bytes` so the `Raw` codec path hands the
    /// pixels straight through to `VideoFrameData.data` with one refcount
    /// bump and no memcpy.
    pub fn decode(&self, payload: Bytes) -> Option<ReceivedFrame> {
        let wire_len = payload.len();
        let header = match deserialize_frame(&payload) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("[bad-payload] frame_video: bad header ({e})");
                return None;
            }
        };
        let Some(&declared) = self.codecs.get(header.track_name) else {
            log::warn!(
                "[unknown-track] frame_video: track '{}' not declared, dropping frame",
                header.track_name
            );
            return None;
        };
        if header.codec != declared {
            log::warn!(
                "[codec-mismatch] frame_video '{}': declared {:?}, got {:?}, dropping frame",
                header.track_name,
                declared,
                header.codec
            );
            return None;
        }
        let track_name = header.track_name.to_string();
        let DeserializedHeader { codec, width, height, timestamp_us, payload_offset, .. } = header;
        // Zero-copy slice of the codec payload.
        let codec_payload = payload.slice(payload_offset..);
        let decoded = match decode_frame(codec_payload, codec, width, height) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("[decode-failed] frame_video '{track_name}': decode failed: {e}");
                return None;
            }
        };
        let frame = VideoFrameData {
            width: decoded.width,
            height: decoded.height,
            data: decoded.rgb,
            timestamp_us,
            source: FrameSource::Live,
        };
        Some(ReceivedFrame { track_name, frame, wire_len })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8x8 RGB24 = 192 bytes. Used as a stand-in source for
    /// `build_frame_payload`'s `rgb` argument in the wire-format tests.
    fn rgb8x8() -> Vec<u8> {
        vec![0u8; 8 * 8 * 3]
    }

    fn payload(
        track_name: &str,
        codec: Codec,
        width: u32,
        height: u32,
        timestamp_us: u64,
        rgb: &[u8],
        quality: u8,
    ) -> PortalResult<Vec<u8>> {
        build_frame_payload(RgbFrame {
            track_name,
            codec,
            width,
            height,
            timestamp_us,
            rgb,
            quality,
        })
    }

    fn tracks() -> FrameVideoTracks {
        let mut config = PortalConfig::new("s", crate::types::Role::Operator);
        config.add_video("cam", Codec::Raw, crate::config::VideoOptions::default());
        config.add_video("png", Codec::Png, crate::config::VideoOptions::default());
        FrameVideoTracks::new(&config)
    }

    #[test]
    fn decodes_declared_tracks_zero_copy() {
        let rgb: Vec<u8> = (0..8 * 8 * 3).map(|i| i as u8).collect();
        let wire = Bytes::from(payload("cam", Codec::Raw, 8, 8, 77, &rgb, 0).unwrap());
        let wire_ptr = wire.as_ptr();
        let wire_len = wire.len();
        let got = tracks().decode(wire).expect("decodes");
        assert_eq!(got.track_name, "cam");
        assert_eq!(got.wire_len, wire_len);
        assert_eq!((got.frame.width, got.frame.height, got.frame.timestamp_us), (8, 8, 77));
        assert_eq!(got.frame.data.as_ref(), rgb.as_slice());
        assert_eq!(got.frame.data.as_ptr(), wire_ptr.wrapping_add(wire_len - rgb.len()));

        let png = payload("png", Codec::Png, 8, 8, 1, &rgb, 0).unwrap();
        assert_eq!(
            tracks().decode(Bytes::from(png)).expect("png").frame.data.as_ref(),
            rgb.as_slice()
        );
    }

    #[test]
    fn drops_undeclared_tracks_and_codec_mismatches() {
        let rgb = rgb8x8();
        let unknown = payload("other", Codec::Raw, 8, 8, 0, &rgb, 0).unwrap();
        assert!(tracks().decode(Bytes::from(unknown)).is_none());
        let mismatch = payload("cam", Codec::Png, 8, 8, 0, &rgb, 0).unwrap();
        assert!(tracks().decode(Bytes::from(mismatch)).is_none());
        let truncated = payload("cam", Codec::Raw, 8, 8, 0, &rgb, 0).unwrap();
        assert!(
            tracks().decode(Bytes::copy_from_slice(&truncated[..truncated.len() - 1])).is_none()
        );
    }

    #[test]
    fn header_roundtrip_with_raw_codec() {
        // Raw is the easiest codec to assert the wire layout against — the
        // codec output equals the input bytes, so the bytes after the
        // header should be the rgb buffer we passed in.
        let rgb = rgb8x8();
        let bytes = payload("cam_left", Codec::Raw, 8, 8, 12_345, &rgb, 0).unwrap();
        let h = deserialize_frame(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Raw);
        assert_eq!((h.width, h.height), (8, 8));
        assert_eq!(h.timestamp_us, 12_345);
        assert_eq!(h.track_name, "cam_left");
        assert_eq!(&bytes[h.payload_offset..], rgb.as_slice());
    }

    #[test]
    fn rejects_oversize_dims() {
        let err = payload("cam", Codec::Raw, 100_000, 240, 0, &[], 0).unwrap_err();
        assert!(matches!(err, PortalError::Codec(msg) if msg.contains("u16 max")));
    }

    #[test]
    fn rejects_oversize_track_name() {
        let big_name = "x".repeat(TRACK_NAME_MAX + 1);
        let rgb = rgb8x8();
        let err = payload(&big_name, Codec::Raw, 8, 8, 0, &rgb, 0).unwrap_err();
        assert!(matches!(err, PortalError::Codec(msg) if msg.contains("track name too long")));
    }

    #[test]
    fn rejects_bad_version() {
        let rgb = rgb8x8();
        let mut bytes = payload("cam", Codec::Raw, 8, 8, 0, &rgb, 0).unwrap();
        bytes[0] = 99;
        assert!(deserialize_frame(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_codec_id() {
        let rgb = rgb8x8();
        let mut bytes = payload("cam", Codec::Raw, 8, 8, 0, &rgb, 0).unwrap();
        bytes[1] = 99;
        assert!(deserialize_frame(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated_header() {
        let rgb = rgb8x8();
        let bytes = payload("cam_left_long_name", Codec::Png, 8, 8, 0, &rgb, 0).unwrap();
        // Strip past mid-name to simulate truncation.
        let truncated = &bytes[..HEADER_FIXED_LEN + 4];
        assert!(deserialize_frame(truncated).is_err());
    }

    #[test]
    fn rejects_oversize_track_name_on_deserialize() {
        // Forge a payload that claims a name length above TRACK_NAME_MAX
        // and confirm the deserializer rejects without trying to read it.
        let mut bytes = vec![WIRE_VERSION, codec_id(Codec::Raw)];
        bytes.extend_from_slice(&8u16.to_le_bytes()); // width
        bytes.extend_from_slice(&8u16.to_le_bytes()); // height
        bytes.extend_from_slice(&0u64.to_le_bytes()); // ts
        bytes.extend_from_slice(&((TRACK_NAME_MAX + 1) as u16).to_le_bytes());
        // No name body — deserializer should reject on length check.
        assert!(deserialize_frame(&bytes).is_err());
    }
}
