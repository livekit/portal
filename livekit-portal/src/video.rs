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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use livekit::options::{
    DegradationPreference, FrameMetadataFeatures, TrackPublishOptions, VideoCodec, VideoEncoding,
};
use livekit::prelude::*;
use livekit::webrtc::prelude::{
    I420Buffer, RtcVideoSource, VideoBuffer, VideoFrame, VideoResolution, VideoRotation,
};
use livekit::webrtc::video_frame::FrameMetadata;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use portal_core::Input;
use portal_core::config::{DEFAULT_H264_MAX_BITRATE_KBPS, Resolution, VideoTrackSpec};
use portal_core::error::{PortalError, PortalResult};
use portal_core::metrics::TrackMetrics;
use portal_core::types::{FrameSource, VideoFrameData};

use crate::clock::anchored_now_us;
use crate::codec::Codec;
use crate::portal::Shared;

const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 480;

// --- Publisher ---

pub(crate) struct VideoPublisher {
    source: NativeVideoSource,
    track: LocalVideoTrack,
    metrics: Arc<TrackMetrics>,
    fps: u32,
    codec: Codec,
    max_bitrate_kbps: Option<u32>,
    simulcast: bool,
    screencast: bool,
}

/// Map a Portal WebRTC codec to the libwebrtc codec the publish path sets.
/// Only ever called for WebRTC codecs — `add_video` routes byte-stream codecs
/// to the frame-video path, so they never reach a `VideoPublisher`.
fn webrtc_video_codec(codec: Codec) -> VideoCodec {
    match codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Vp8 => VideoCodec::VP8,
        Codec::Vp9 => VideoCodec::VP9,
        Codec::Av1 => VideoCodec::AV1,
        Codec::H265 => VideoCodec::H265,
        Codec::Raw | Codec::Png | Codec::Mjpeg => unreachable!(
            "byte-stream codecs never reach a VideoPublisher (add_video routes them \
             to the frame-video path)"
        ),
    }
}

/// Unset, the SDK picks `Balanced` for a source that isn't a camera or screen
/// share, which sheds frame rate as well as resolution.
fn degradation_preference(screencast: bool) -> DegradationPreference {
    if screencast {
        DegradationPreference::MaintainResolution
    } else {
        DegradationPreference::MaintainFramerate
    }
}

pub(crate) struct VideoPublisherOptions<'a> {
    pub spec: &'a VideoTrackSpec,
    pub metrics: Arc<TrackMetrics>,
    pub fps: u32,
}

impl VideoPublisher {
    pub fn new(options: VideoPublisherOptions<'_>) -> Self {
        let VideoPublisherOptions { spec, metrics, fps } = options;
        let Resolution { width, height } =
            spec.resolution.unwrap_or(Resolution { width: DEFAULT_WIDTH, height: DEFAULT_HEIGHT });
        let resolution = VideoResolution { width, height };
        // `screencast` is libwebrtc's content-type hint. The degradation
        // preference that goes with it is set at publish.
        let source = NativeVideoSource::new(resolution, spec.screencast);
        let rtc_source = RtcVideoSource::Native(source.clone());
        let track = LocalVideoTrack::create_video_track(&spec.name, rtc_source);
        Self {
            source,
            track,
            metrics,
            fps,
            codec: spec.codec,
            max_bitrate_kbps: spec.max_bitrate_kbps,
            simulcast: spec.simulcast,
            screencast: spec.screencast,
        }
    }

    pub async fn publish(&self, local_participant: &LocalParticipant) -> PortalResult<()> {
        // user_timestamp is mandatory: the receive path uses it to align frames
        // with state, and panics if it is missing. Subscribed tracks produced
        // by publishers that don't set this trailer are unsupported.
        let mut features = FrameMetadataFeatures::default();
        features.user_timestamp = true;

        // Pin encoder ceilings explicitly. Without `video_encoding`, libwebrtc's
        // `VideoStreamEncoder` picks conservative defaults and drops frames to
        // stay under its own rate target. For a teleop publisher we want the
        // encoder to keep up with the capture cadence, not the other way around.
        //
        //   max_framerate = fps * 2 — 2x headroom over the capture rate so the
        //     adaptive-framerate logic never throttles below our cadence.
        //   max_bitrate   = per-track ceiling (default 10 Mbps); the encoder
        //     still picks a much lower operating bitrate based on content. We
        //     just don't want a tight cap forcing frame drops on high-motion
        //     bursts. Configurable per track via `add_video`'s
        //     `max_bitrate_kbps`.
        let max_bitrate_kbps = self.max_bitrate_kbps.unwrap_or(DEFAULT_H264_MAX_BITRATE_KBPS);
        let options = TrackPublishOptions {
            video_codec: webrtc_video_codec(self.codec),
            simulcast: self.simulcast,
            frame_metadata_features: features,
            video_encoding: Some(VideoEncoding {
                max_framerate: (self.fps as f64) * 2.0,
                max_bitrate: (max_bitrate_kbps as u64) * 1_000,
            }),
            degradation_preference: Some(degradation_preference(self.screencast)),
            ..Default::default()
        };
        local_participant
            .publish_track(LocalTrack::Video(self.track.clone()), options)
            .await
            .map_err(|e| PortalError::Room(e.to_string()))?;
        Ok(())
    }

    pub fn send_frame(
        &self,
        rgb_data: &[u8],
        width: u32,
        height: u32,
        timestamp_us: u64,
    ) -> PortalResult<()> {
        // I420 packs U and V at half resolution in each axis. Odd dimensions
        // would silently desynchronize plane sizes (width/2 truncates), so
        // reject up front rather than copy garbage into the chroma planes.
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(PortalError::InvalidFrameDimensions { width, height });
        }
        let expected_size = (width * height * 3) as usize;
        if rgb_data.len() != expected_size {
            return Err(PortalError::WrongFrameSize {
                expected: expected_size,
                got: rgb_data.len(),
            });
        }
        let mut buffer = I420Buffer::new(width, height);
        rgb_to_i420(rgb_data, width, height, &mut buffer);
        let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
        frame.frame_metadata = Some(FrameMetadata {
            user_timestamp: Some(timestamp_us),
            frame_id: None,
            user_data: None,
        });
        self.source.capture_frame(&frame);
        self.metrics.record_sent();
        Ok(())
    }
}

// --- Receiver ---

/// Frames buffered between the drain task and the processing task. At 60fps
/// this is ~130ms of slack, enough to ride out bursts and brief callback
/// stalls before the oldest queued frames start being dropped. Kept small so
/// a sustained slow consumer sheds load promptly instead of ballooning
/// memory or delivery latency.
const RECV_CHANNEL_CAPACITY: usize = 8;

/// Minimum gap between `[recv-overflow]` warnings so a sustained stall logs
/// periodically instead of once per dropped frame.
const RECV_DROP_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// Bounded, latest-biased handoff between the stream-drain task and the
/// processing task.
///
/// The drain task must never block on downstream work (user callbacks, sync,
/// dispatch). If it does, libwebrtc's native receive queue backs up and
/// overflows, dropping thousands of frames in bulk (the "native video stream
/// queue overflow" warning). So the drain converts each frame and pushes it
/// here without blocking; when the processing task falls behind and the ring
/// is at capacity, the *oldest* queued frame is dropped so the freshest
/// frames keep flowing.
struct FrameChannel {
    inner: Mutex<FrameChannelState>,
    notify: Notify,
    capacity: usize,
}

struct FrameChannelState {
    queue: VecDeque<VideoFrameData>,
    closed: bool,
}

impl FrameChannel {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(FrameChannelState { queue: VecDeque::new(), closed: false }),
            notify: Notify::new(),
            capacity,
        }
    }

    /// Enqueue a frame, dropping the oldest if the ring is full. Returns
    /// `true` when a frame was dropped to make room. Never blocks.
    fn push(&self, frame: VideoFrameData) -> bool {
        let dropped = {
            let mut st = self.inner.lock();
            let dropped = st.queue.len() >= self.capacity && st.queue.pop_front().is_some();
            st.queue.push_back(frame);
            dropped
        };
        self.notify.notify_one();
        dropped
    }

    /// Signal that no more frames will arrive. Wakes a parked consumer so it
    /// can drain the remainder and exit.
    fn close(&self) {
        self.inner.lock().closed = true;
        self.notify.notify_one();
    }

    /// Await the next frame, or `None` once the channel is closed and drained.
    async fn recv(&self) -> Option<VideoFrameData> {
        loop {
            // Register interest before checking the queue so a `push` racing
            // between the check and the await can't be missed (a permit
            // stored by `notify_one` satisfies the pending `notified`).
            let notified = self.notify.notified();
            {
                let mut st = self.inner.lock();
                if let Some(frame) = st.queue.pop_front() {
                    return Some(frame);
                }
                if st.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

pub(crate) struct VideoReceiverOptions {
    pub name: String,
    pub stream: NativeVideoStream,
    pub shared: Arc<Shared>,
    pub metrics: Arc<TrackMetrics>,
}

pub(crate) struct VideoReceiver {
    drain_handle: JoinHandle<()>,
    process_handle: JoinHandle<()>,
}

impl VideoReceiver {
    pub fn spawn(options: VideoReceiverOptions) -> Self {
        let VideoReceiverOptions { name, stream, shared, metrics } = options;
        let channel = Arc::new(FrameChannel::new(RECV_CHANNEL_CAPACITY));

        // Drain task: pull decoded frames off the native stream as fast as
        // they arrive and hand them to the processing task. This task does
        // the bare minimum and never touches user callbacks, sync, or
        // dispatch — any stall here lets libwebrtc's native receive queue
        // overflow and drop frames in bulk. Its only per-frame cost is
        // bounded CPU: colour-convert plus a latest-wins slot update.
        let drain_channel = channel.clone();
        let drain_shared = shared.clone();
        let drain_name = name.clone();
        let drain_handle = tokio::spawn(async move {
            let mut stream = stream;
            let mut dropped_total: u64 = 0;
            let mut last_warn: Option<Instant> = None;
            let mut untimed_total: u64 = 0;
            let mut last_untimed_warn: Option<Instant> = None;
            while let Some(frame) = stream.next().await {
                // Every frame needs a user_timestamp to be aligned. Portal
                // publishers always set one, but frames decoded before the
                // SDK attaches its trailer handler arrive without it, as do
                // frames from publishers that never enable it (see the
                // "Sender requirement" note in README.md). Skip those rather
                // than stamping them with a made-up time.
                let Some(timestamp_us) =
                    frame.frame_metadata.as_ref().and_then(|m| m.user_timestamp)
                else {
                    untimed_total += 1;
                    // The first frame after subscribing routinely races the
                    // trailer handler; only repeated skips point at the sender.
                    if untimed_total == 1 {
                        log::debug!("[missing-timestamp] '{drain_name}' skipped first frame");
                        continue;
                    }
                    let now = Instant::now();
                    if last_untimed_warn
                        .is_none_or(|t| now.duration_since(t) >= RECV_DROP_WARN_INTERVAL)
                    {
                        log::warn!(
                            "[missing-timestamp] '{drain_name}' skipped {untimed_total} frame(s) \
                             with no user_timestamp; the sender must enable \
                             FrameMetadataFeatures.user_timestamp"
                        );
                        last_untimed_warn = Some(now);
                    }
                    continue;
                };
                let frame_data = convert_frame(&frame, timestamp_us);

                metrics.record_received(timestamp_us, anchored_now_us());

                // Freshest-frame slot for `get_video_frame`. Updated on the
                // drain so polling consumers see the newest frame even when
                // per-frame callbacks fall behind. Clone is cheap — pixel
                // buffer is `Bytes`.
                drain_shared.sinks.set_latest_frame(&drain_name, &frame_data);

                if drain_channel.push(frame_data) {
                    dropped_total += 1;
                    let now = Instant::now();
                    let should_warn =
                        last_warn.is_none_or(|t| now.duration_since(t) >= RECV_DROP_WARN_INTERVAL);
                    if should_warn {
                        log::warn!(
                            "[recv-overflow] '{drain_name}' frame processing is behind; dropped \
                             {dropped_total} frame(s) so far to keep the receive loop \
                             draining. A slow on-frame or on-observation callback is the \
                             usual cause."
                        );
                        last_warn = Some(now);
                    }
                }
            }
            // Stream ended (track unsubscribed / room closed). Let the
            // processing task drain what's left and exit on its own.
            drain_channel.close();
        });

        // Processing task: everything that can block on user code or contend
        // for the core — the per-frame callback, sync and observation
        // dispatch. Falling behind here drops frames at the channel instead
        // of stalling the drain.
        let process_handle = tokio::spawn(async move {
            while let Some(frame) = channel.recv().await {
                shared.step(Input::VideoFrame { track: &name, frame });
            }
        });

        Self { drain_handle, process_handle }
    }

    pub fn abort(&self) {
        self.drain_handle.abort();
        self.process_handle.abort();
    }
}

// --- Helpers ---

/// Convert a libwebrtc-decoded video frame into the RGB24 payload the user
/// API hands back. WebRTC's H264 decoder emits I420; the user-facing
/// `VideoFrameData.data` is packed RGB24 (R,G,B byte order, `W*H*3` bytes)
/// so it round-trips cleanly with the RGB the publisher accepted on the
/// other end. Frame-video tracks use the same RGB layout.
fn convert_frame<T: AsRef<dyn VideoBuffer>>(
    frame: &VideoFrame<T>,
    timestamp_us: u64,
) -> VideoFrameData {
    let i420 = frame.buffer.as_ref().to_i420();
    let (sy, su, sv) = i420.strides();
    let (y, u, v) = i420.data();
    let width = i420.width();
    let height = i420.height();
    let total = (width as usize) * (height as usize) * 3;
    let dst_stride = (width as i32) * 3;

    // Single-allocation buffer with no zero-init — libyuv writes RGB24
    // directly into the reserved capacity, then we move ownership into
    // `Bytes`. `set_len(total)` after libyuv is the only way to avoid a
    // `vec![0; total]` zero-pass that would cost ~6 MB of writes per
    // 1080p frame for nothing.
    let mut buf: Vec<u8> = Vec::with_capacity(total);
    // SAFETY: src planes are sized by libwebrtc per `width`/`height`;
    // libyuv writes exactly `width*height*3` bytes within `dst_stride`
    // bounds; we then `set_len(total)` to publish those bytes as
    // initialized. The capacity was reserved above.
    unsafe {
        yuv_sys::rs_I420ToRAW(
            y.as_ptr(),
            sy as i32,
            u.as_ptr(),
            su as i32,
            v.as_ptr(),
            sv as i32,
            buf.as_mut_ptr(),
            dst_stride,
            width as i32,
            height as i32,
        );
        buf.set_len(total);
    }
    VideoFrameData {
        width,
        height,
        data: Bytes::from(buf),
        timestamp_us,
        source: FrameSource::Live,
    }
}

// RGB24 (R,G,B byte order) -> I420 via libyuv. libyuv's `RAW` format is R,G,B;
// its `RGB24` is B,G,R. We advertise RGB, so RAWToI420 is the correct call.
fn rgb_to_i420(src: &[u8], width: u32, height: u32, buffer: &mut I420Buffer) {
    let (sy, su, sv) = buffer.strides();
    let (y_dst, u_dst, v_dst) = buffer.data_mut();
    // SAFETY: `src` has width*height*3 bytes (checked by caller); dst planes
    // are sized by I420Buffer::new(width, height); strides come from the
    // buffer itself. libyuv only reads/writes within these bounds.
    unsafe {
        yuv_sys::rs_RAWToI420(
            src.as_ptr(),
            (width * 3) as i32,
            y_dst.as_mut_ptr(),
            sy as i32,
            u_dst.as_mut_ptr(),
            su as i32,
            v_dst.as_mut_ptr(),
            sv as i32,
            width as i32,
            height as i32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_holds_frame_rate_and_screencast_holds_resolution() {
        assert_eq!(degradation_preference(false), DegradationPreference::MaintainFramerate);
        assert_eq!(degradation_preference(true), DegradationPreference::MaintainResolution);
    }

    fn frame(ts: u64) -> VideoFrameData {
        VideoFrameData {
            width: 2,
            height: 2,
            data: Bytes::from_static(&[0u8; 12]),
            timestamp_us: ts,
            source: FrameSource::Live,
        }
    }

    /// Frames flow through in FIFO order when the consumer keeps up.
    #[tokio::test]
    async fn channel_fifo_order() {
        let ch = FrameChannel::new(4);
        assert!(!ch.push(frame(1)));
        assert!(!ch.push(frame(2)));
        assert_eq!(ch.recv().await.unwrap().timestamp_us, 1);
        assert_eq!(ch.recv().await.unwrap().timestamp_us, 2);
    }

    /// At capacity, `push` drops the oldest frame and reports it, keeping the
    /// freshest `capacity` frames.
    #[tokio::test]
    async fn channel_drops_oldest_when_full() {
        let ch = FrameChannel::new(2);
        assert!(!ch.push(frame(1)));
        assert!(!ch.push(frame(2)));
        // Ring full; this evicts frame(1).
        assert!(ch.push(frame(3)));
        // Survivors are the two freshest, oldest-first.
        assert_eq!(ch.recv().await.unwrap().timestamp_us, 2);
        assert_eq!(ch.recv().await.unwrap().timestamp_us, 3);
    }

    /// After close, a consumer drains what remains and then gets `None`.
    #[tokio::test]
    async fn channel_close_drains_then_ends() {
        let ch = FrameChannel::new(4);
        ch.push(frame(1));
        ch.close();
        assert_eq!(ch.recv().await.unwrap().timestamp_us, 1);
        assert!(ch.recv().await.is_none());
    }

    /// A consumer parked on an empty channel wakes when a frame is pushed
    /// from another task.
    #[tokio::test]
    async fn channel_recv_wakes_on_push() {
        let ch = Arc::new(FrameChannel::new(4));
        let producer = ch.clone();
        let handle = tokio::spawn(async move { producer.push(frame(42)) });
        assert_eq!(ch.recv().await.unwrap().timestamp_us, 42);
        handle.await.unwrap();
    }

    /// A consumer parked on an empty channel wakes and ends when the channel
    /// is closed with nothing queued.
    #[tokio::test]
    async fn channel_recv_wakes_on_close() {
        let ch = Arc::new(FrameChannel::new(4));
        let closer = ch.clone();
        let handle = tokio::spawn(async move { closer.close() });
        assert!(ch.recv().await.is_none());
        handle.await.unwrap();
    }
}
