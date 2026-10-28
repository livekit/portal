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

//! Frame-video transport over LiveKit byte streams: one stream per frame on
//! `portal_frame_video`, bypassing the WebRTC media path so the receiver
//! gets exactly the bytes the publisher fed the codec. The wire format
//! lives in `livekit_portal_core::frame_video`.
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

use std::sync::Arc;

use livekit::StreamByteOptions;
use livekit::prelude::*;
use livekit_portal_core::Input;
use livekit_portal_core::config::VideoTrackSpec;
use livekit_portal_core::error::PortalResult;
use livekit_portal_core::frame_video::{RgbFrame, build_frame_payload_with};
use livekit_portal_core::metrics::TrackMetrics;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::clock::anchored_now_us;
use crate::codec::{Codec, encode_frame_into};
use crate::portal::Shared;
use crate::wire::FRAME_VIDEO_TOPIC;

// --- Publisher ---

/// Publishes one frame-video track's frames as a sequence of LiveKit byte
/// streams, one in flight at a time per track. Frames wait in a single
/// latest-wins slot: a frame sent while the previous one is still pending
/// replaces it, so latency stays at one frame when the link is slower than
/// the send rate instead of growing with a queue.
pub(crate) struct FrameVideoPublisher {
    spec: VideoTrackSpec,
    pending: Arc<PendingFrame>,
    task: Option<JoinHandle<()>>,
    metrics: Arc<TrackMetrics>,
}

pub(crate) struct FrameVideoPublisherOptions {
    pub spec: VideoTrackSpec,
    pub local_participant: LocalParticipant,
    pub metrics: Arc<TrackMetrics>,
}

impl FrameVideoPublisher {
    pub fn new(options: FrameVideoPublisherOptions) -> Self {
        let FrameVideoPublisherOptions { spec, local_participant, metrics } = options;
        let pending = Arc::new(PendingFrame::default());
        let track_name = spec.name.clone();
        // MJPEG and PNG are already compressed, so deflating them only costs
        // CPU. Raw usually shrinks enough to be worth it on a slow link.
        let compress = spec.codec == Codec::Raw;
        let slot = pending.clone();
        let task_metrics = metrics.clone();
        let task = tokio::spawn(async move {
            loop {
                slot.ready.notified().await;
                let Some(payload) = slot.frame.lock().take() else { continue };
                task_metrics.record_sent_bytes(payload.len());
                let options =
                    StreamByteOptions::new_with_topic(FRAME_VIDEO_TOPIC).with_compress(compress);
                if let Err(e) = local_participant.send_bytes(payload, options).await {
                    log::warn!(
                        "[publish-failed] frame_video '{track_name}' byte stream failed: {e}"
                    );
                }
            }
        });
        Self { spec, pending, task: Some(task), metrics }
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
        timestamp_us: u64,
    ) -> PortalResult<()> {
        let frame = RgbFrame {
            track_name: &self.spec.name,
            codec: self.spec.codec,
            width,
            height,
            timestamp_us,
            rgb: rgb_data,
            quality: self.spec.quality,
        };
        let payload = build_frame_payload_with(frame, encode_frame_into)?;
        if self.pending.frame.lock().replace(payload).is_some() {
            self.metrics.record_dropped_publisher_full();
        }
        self.pending.ready.notify_one();
        Ok(())
    }
}

#[derive(Default)]
struct PendingFrame {
    frame: Mutex<Option<Vec<u8>>>,
    ready: Notify,
}

impl Drop for FrameVideoPublisher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// --- Receiver ---

/// Read one finished `portal_frame_video` byte stream, decode it off the
/// core's lock, and feed the frame through the same path WebRTC video uses.
pub(crate) fn spawn_receive(reader: livekit::ByteStreamReader, shared: Arc<Shared>) {
    tokio::spawn(async move {
        use livekit::StreamReader;
        // `Bytes::from(Vec)` is a move, so the `Raw` codec gets a zero-copy
        // view of the wire payload all the way to `VideoFrameData.data`.
        let payload = match reader.read_all().await {
            Ok(payload) => payload,
            Err(e) => {
                log::warn!("[bad-payload] failed to read frame_video byte stream: {e}");
                return;
            }
        };
        let Some(received) = shared.frame_video.decode(payload) else {
            return;
        };
        let name = received.track_name;
        if let Some(metrics) = shared.metrics.track(&name) {
            metrics.record_received_bytes(
                received.frame.timestamp_us,
                anchored_now_us(),
                received.wire_len,
            );
        }
        shared.sinks.set_latest_frame(&name, &received.frame);
        shared.step(Input::VideoFrame { track: &name, frame: received.frame });
    });
}

#[cfg(test)]
mod tests {
    use livekit_portal_core::frame_video::{build_frame_payload, deserialize_frame};

    use super::*;

    fn rgb(w: u32, h: u32) -> Vec<u8> {
        (0..w * h * 3).map(|i| (i % 253) as u8).collect()
    }

    /// The native MJPEG path keeps the core's header and swaps only the
    /// encoder for libjpeg-turbo.
    #[test]
    fn native_mjpeg_payload_has_the_core_header() {
        let rgb = rgb(32, 16);
        let frame = || RgbFrame {
            track_name: "cam",
            codec: Codec::Mjpeg,
            width: 32,
            height: 16,
            timestamp_us: 9,
            rgb: &rgb,
            quality: 90,
        };
        let native = build_frame_payload_with(frame(), encode_frame_into).unwrap();
        let header = deserialize_frame(&native).unwrap();
        assert_eq!(
            (header.codec, header.width, header.height, header.timestamp_us),
            (Codec::Mjpeg, 32, 16, 9)
        );
        let offset = header.payload_offset;
        let core = build_frame_payload(frame()).unwrap();
        assert_eq!(native[..offset], core[..offset]);
        let jpeg = crate::codec::encode_frame(&rgb, 32, 16, Codec::Mjpeg, 90).unwrap();
        assert_eq!(&native[offset..], jpeg.as_slice());
    }
}
