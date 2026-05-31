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
//! sender. Empirically, the per-chunk cost on localhost is ~2 ms,
//! dominated by SCTP send + libwebrtc's `buffered_amount` drain rate
//! rather than the SDK's writer-level loop. So:
//!
//! ```text
//! latency ≈ 1 ms + 2 ms × ⌈encoded_size / 15 KB⌉
//! ```
//!
//! Implication for codec choice: the throughput floor is per-byte, not
//! per-frame. A codec whose encoded output fits in one chunk pays the
//! single-chunk floor (≈2 ms). A codec spilling into 60+ chunks pays
//! ≈120 ms. MJPEG at typical inference resolutions (224×224 to 480p,
//! q=80–95) lands in the 1–4-chunk regime; PNG and Raw at the same
//! resolutions can run 16–60 chunks. Pick by chunk count, not by
//! quality knob.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use bytes::Bytes;
use livekit::prelude::*;
use livekit::StreamByteOptions;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::codec::{decode_frame, encode_frame_into, estimated_encoded_size, Codec};
use crate::config::FrameVideoSpec;
use crate::error::{PortalError, PortalResult};
use crate::metrics::TrackMetrics;
use crate::portal::ObservationSink;
use crate::sync_buffer::SyncBuffer;
use crate::types::VideoFrameData;
use crate::video::{now_us, VideoTrackSlots};

/// Reserved Portal topic for frame-video byte streams. A single topic
/// multiplexes all frame-video tracks; the per-frame header carries the
/// track name.
pub(crate) const FRAME_VIDEO_TOPIC: &str = "portal_frame_video";

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

/// Bound on the in-flight publish queue per track. Sized for ~1 second of
/// frames at 60 fps. The bound exists so a stalled byte-stream send
/// (slow link, SFU backpressure) cannot grow memory without limit; on
/// overflow we drop the newest frame and warn rather than block the
/// caller's send path. Video already tolerates frame loss, so dropping
/// is the right policy here.
const PUBLISH_QUEUE_CAP: usize = 60;

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

/// Build the on-wire payload for one frame: header + codec-encoded bytes,
/// in a single allocation. The encode writes directly into the buffer
/// after the header — no intermediate `Vec` and no second memcpy of the
/// payload (saves megabytes of bandwidth per frame for `Codec::Raw`).
pub(crate) fn build_frame_payload(
    track_name: &str,
    codec: Codec,
    width: u32,
    height: u32,
    timestamp_us: u64,
    rgb: &[u8],
    quality: u8,
) -> PortalResult<Vec<u8>> {
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
    let mut buf =
        Vec::with_capacity(header_len + estimated_encoded_size(width, height, codec));
    buf.push(WIRE_VERSION);
    buf.push(codec_id(codec));
    buf.extend_from_slice(&(width as u16).to_le_bytes());
    buf.extend_from_slice(&(height as u16).to_le_bytes());
    buf.extend_from_slice(&timestamp_us.to_le_bytes());
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    encode_frame_into(&mut buf, rgb, width, height, codec, quality)
        .map_err(|e| PortalError::Codec(e.to_string()))?;
    Ok(buf)
}

/// Parsed view of one frame payload header. Borrows the track-name bytes
/// from the input wire payload so we don't allocate per frame; the codec
/// bytes are referenced separately so the receiver can slice them zero-copy
/// out of the underlying `Bytes`.
#[derive(Debug)]
pub(crate) struct DeserializedHeader<'a> {
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

pub(crate) fn deserialize_frame(bytes: &[u8]) -> Result<DeserializedHeader<'_>, &'static str> {
    if bytes.len() < HEADER_FIXED_LEN {
        return Err("frame_video payload shorter than fixed header");
    }
    if bytes[0] != WIRE_VERSION {
        return Err("frame_video payload has unknown version");
    }
    let codec = codec_from_id(bytes[1]).ok_or("unknown codec id")?;
    let width = u16::from_le_bytes(bytes[2..4].try_into().unwrap()) as u32;
    let height = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as u32;
    let timestamp_us = u64::from_le_bytes(bytes[6..14].try_into().unwrap());
    let name_len = u16::from_le_bytes(bytes[14..16].try_into().unwrap()) as usize;
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

// --- Per-track entry (publisher + receiver share) ---

/// Pre-resolved per-track context for the receive hot path. Bundles the
/// declared spec with the `Arc<TrackMetrics>` and `Arc<VideoTrackSlots>`
/// the dispatcher needs, so each received frame does one HashMap lookup
/// instead of three. Built once at `Portal::new` and never mutated.
pub(crate) struct FrameVideoTrackEntry {
    pub spec: FrameVideoSpec,
    pub metrics: Arc<TrackMetrics>,
    pub slots: Arc<VideoTrackSlots>,
}

// --- Publisher ---

/// Publishes one frame-video track's frames as a sequence of LiveKit byte
/// streams. Sends are serialized through an mpsc onto a single drainer
/// task — one byte stream in flight at a time per track. A faster send
/// rate that backs up the queue spills into newest-frame-drops at the
/// `try_send` boundary (video tolerates loss; latency under load is more
/// important than fidelity).
pub(crate) struct FrameVideoPublisher {
    spec: FrameVideoSpec,
    tx: mpsc::Sender<Vec<u8>>,
    task: Option<JoinHandle<()>>,
    metrics: Arc<TrackMetrics>,
}

impl FrameVideoPublisher {
    pub fn new(
        spec: FrameVideoSpec,
        local_participant: LocalParticipant,
        metrics: Arc<TrackMetrics>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(PUBLISH_QUEUE_CAP);
        let track_name = spec.name.clone();
        let task = tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let options = StreamByteOptions {
                    topic: FRAME_VIDEO_TOPIC.to_string(),
                    ..Default::default()
                };
                if let Err(e) = local_participant.send_bytes(payload, options).await {
                    log::warn!(
                        "frame_video '{track_name}': failed to send byte stream: {e}"
                    );
                }
            }
        });
        Self { spec, tx, task: Some(task), metrics }
    }

    /// Encode one RGB frame and queue it for publish. `rgb_data` is packed
    /// RGB24 (`W*H*3` bytes). Returns once the frame is queued; the actual
    /// byte-stream send happens on the background task. Header and codec
    /// payload share a single `Vec` allocation so a 1080p Raw frame avoids
    /// a 6 MB intermediate copy on the hot path.
    pub fn send_frame(
        &self,
        rgb_data: &[u8],
        width: u32,
        height: u32,
        timestamp_us: Option<u64>,
    ) -> PortalResult<()> {
        let ts = timestamp_us.unwrap_or_else(now_us);
        let payload = build_frame_payload(
            &self.spec.name,
            self.spec.codec,
            width,
            height,
            ts,
            rgb_data,
            self.spec.quality,
        )?;
        let payload_len = payload.len();
        match self.tx.try_send(payload) {
            Ok(()) => self.metrics.record_sent_bytes(payload_len),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.record_dropped_publisher_full();
                log::warn!(
                    "frame_video '{}' publish queue full (cap={}); dropping frame",
                    self.spec.name,
                    PUBLISH_QUEUE_CAP
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Drainer task gone (disconnect / drop). Caller is in
                // teardown; silent.
            }
        }
        Ok(())
    }
}

impl Drop for FrameVideoPublisher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// --- Receiver dispatch ---

/// Operator-side: handle one finished `portal_frame_video` byte stream.
/// Validates the header against the declared `FrameVideoTrackEntry`,
/// decodes the codec bytes back to RGB, and pushes the frame through the
/// same slots + sync-buffer path WebRTC video uses. A mismatch (unknown
/// track, codec disagreement, decode failure) drops the frame and warns.
///
/// `payload` is consumed as `Bytes` so the `Raw` codec path can hand the
/// pixels straight through to `VideoFrameData.data` with one refcount
/// bump and no memcpy. The fused `entries` map (spec + slots + metrics in
/// one struct) means each received frame does a single HashMap lookup.
pub(crate) fn dispatch_frame_payload(
    payload: Bytes,
    entries: &HashMap<String, Arc<FrameVideoTrackEntry>>,
    sync_buffer: &Arc<Mutex<SyncBuffer>>,
    obs_sink: &Arc<ObservationSink>,
) {
    let wire_len = payload.len();
    let header = match deserialize_frame(&payload) {
        Ok(h) => h,
        Err(e) => {
            log::warn!("frame_video: bad payload header ({e})");
            return;
        }
    };
    let Some(entry) = entries.get(header.track_name) else {
        log::warn!(
            "frame_video: dropping frame for undeclared track '{}'",
            header.track_name
        );
        return;
    };
    if header.codec != entry.spec.codec {
        log::warn!(
            "frame_video '{}': codec mismatch (declared {:?}, got {:?}); dropping frame",
            header.track_name,
            entry.spec.codec,
            header.codec
        );
        return;
    }
    // Capture the borrow's bits before consuming `payload` into a slice.
    let track_name_len = header.track_name.len();
    let codec = header.codec;
    let width = header.width;
    let height = header.height;
    let timestamp_us = header.timestamp_us;
    let payload_offset = header.payload_offset;
    // Zero-copy slice of the codec payload — for `Raw` this is the path
    // that ends up as `VideoFrameData.data` with zero allocations and
    // zero memcpy. PNG / MJPEG decoders read it as a slice and produce
    // their own decoded buffer.
    let codec_payload = payload.slice(payload_offset..);

    let decoded = match decode_frame(codec_payload, codec, width, height) {
        Ok(d) => d,
        Err(e) => {
            // Re-borrow the name from the header buffer for the log; the
            // buffer is `payload`, still alive because we sliced from it.
            let track_name = std::str::from_utf8(
                &payload[HEADER_FIXED_LEN..HEADER_FIXED_LEN + track_name_len],
            )
            .unwrap_or("<invalid utf-8>");
            log::warn!("frame_video '{}': decode failed: {e}", track_name);
            return;
        }
    };

    entry.metrics.record_received_bytes(timestamp_us, now_us(), wire_len);

    let frame = Arc::new(VideoFrameData {
        width: decoded.width,
        height: decoded.height,
        data: decoded.rgb,
        timestamp_us,
    });

    let track_name_for_dispatch = entry.spec.name.as_str();
    if let Some(cb) = entry.slots.cb.lock().as_ref() {
        // Match VideoReceiver: a panicking user callback would abort the
        // tokio worker that ran read_all; catch and log instead.
        let result = catch_unwind(AssertUnwindSafe(|| cb(track_name_for_dispatch, &frame)));
        if result.is_err() {
            log::error!(
                "frame_video frame callback panicked on track '{}'; \
                 receive loop continues",
                track_name_for_dispatch
            );
        }
    }
    *entry.slots.latest.lock() = Some((*frame).clone());

    let output = sync_buffer.lock().push_frame(track_name_for_dispatch, frame);
    if !output.is_empty() {
        obs_sink.dispatch(output);
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

    #[test]
    fn header_roundtrip_with_raw_codec() {
        // Raw is the easiest codec to assert the wire layout against — the
        // codec output equals the input bytes, so the bytes after the
        // header should be the rgb buffer we passed in.
        let rgb = rgb8x8();
        let bytes = build_frame_payload("cam_left", Codec::Raw, 8, 8, 12_345, &rgb, 0).unwrap();
        let h = deserialize_frame(&bytes).unwrap();
        assert_eq!(h.codec, Codec::Raw);
        assert_eq!((h.width, h.height), (8, 8));
        assert_eq!(h.timestamp_us, 12_345);
        assert_eq!(h.track_name, "cam_left");
        assert_eq!(&bytes[h.payload_offset..], rgb.as_slice());
    }

    #[test]
    fn rejects_oversize_dims() {
        let err = build_frame_payload("cam", Codec::Raw, 100_000, 240, 0, &[], 0).unwrap_err();
        assert!(matches!(err, PortalError::Codec(msg) if msg.contains("u16 max")));
    }

    #[test]
    fn rejects_oversize_track_name() {
        let big_name = "x".repeat(TRACK_NAME_MAX + 1);
        let rgb = rgb8x8();
        let err = build_frame_payload(&big_name, Codec::Raw, 8, 8, 0, &rgb, 0).unwrap_err();
        assert!(matches!(err, PortalError::Codec(msg) if msg.contains("track name too long")));
    }

    #[test]
    fn rejects_bad_version() {
        let rgb = rgb8x8();
        let mut bytes = build_frame_payload("cam", Codec::Raw, 8, 8, 0, &rgb, 0).unwrap();
        bytes[0] = 99;
        assert!(deserialize_frame(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_codec_id() {
        let rgb = rgb8x8();
        let mut bytes = build_frame_payload("cam", Codec::Raw, 8, 8, 0, &rgb, 0).unwrap();
        bytes[1] = 99;
        assert!(deserialize_frame(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated_header() {
        let rgb = rgb8x8();
        let bytes =
            build_frame_payload("cam_left_long_name", Codec::Png, 8, 8, 0, &rgb, 0).unwrap();
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
