use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use livekit::options::{PacketTrailerFeatures, TrackPublishOptions, VideoCodec, VideoEncoding};
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

use crate::codec::Codec;
use crate::config::DEFAULT_H264_MAX_BITRATE_KBPS;
use crate::error::{PortalError, PortalResult};
use crate::metrics::TrackMetrics;
use crate::portal::ObservationSink;
use crate::sync_buffer::SyncBuffer;
use crate::types::VideoFrameData;

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

impl VideoPublisher {
    pub fn new(
        name: &str,
        metrics: Arc<TrackMetrics>,
        fps: u32,
        codec: Codec,
        max_bitrate_kbps: Option<u32>,
    ) -> Self {
        let resolution = VideoResolution { width: DEFAULT_WIDTH, height: DEFAULT_HEIGHT };
        let source = NativeVideoSource::new(resolution, false);
        let rtc_source = RtcVideoSource::Native(source.clone());
        let track = LocalVideoTrack::create_video_track(name, rtc_source);
        Self { source, track, metrics, fps, codec, max_bitrate_kbps }
    }

    pub async fn publish(&self, local_participant: &LocalParticipant) -> PortalResult<()> {
        // user_timestamp is mandatory: the receive path uses it to align frames
        // with state, and panics if it is missing. Subscribed tracks produced
        // by publishers that don't set this trailer are unsupported.
        let mut features = PacketTrailerFeatures::default();
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
            simulcast: false,
            packet_trailer_features: features,
            video_encoding: Some(VideoEncoding {
                max_framerate: (self.fps as f64) * 2.0,
                max_bitrate: (max_bitrate_kbps as u64) * 1_000,
            }),
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
        timestamp_us: Option<u64>,
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
        let ts = timestamp_us.unwrap_or_else(now_us);
        let mut buffer = I420Buffer::new(width, height);
        rgb_to_i420(rgb_data, width, height, &mut buffer);
        let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
        frame.frame_metadata = Some(FrameMetadata { user_timestamp: Some(ts), frame_id: None });
        self.source.capture_frame(&frame);
        self.metrics.record_sent();
        Ok(())
    }
}

// --- Receiver ---

pub(crate) type VideoCb = Box<dyn Fn(&str, &VideoFrameData) + Send + Sync>;

/// Push callback + latest-wins slot for a single video track, paired so the
/// receiver task and `get_video_frame` share one allocation.
pub(crate) struct VideoTrackSlots {
    pub cb: Mutex<Option<VideoCb>>,
    pub latest: Mutex<Option<VideoFrameData>>,
}

impl VideoTrackSlots {
    pub fn new() -> Self {
        Self { cb: Mutex::new(None), latest: Mutex::new(None) }
    }

    pub fn clear(&self) {
        *self.latest.lock() = None;
    }
}

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
    queue: VecDeque<Arc<VideoFrameData>>,
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
    fn push(&self, frame: Arc<VideoFrameData>) -> bool {
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
    async fn recv(&self) -> Option<Arc<VideoFrameData>> {
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

pub(crate) struct VideoReceiver {
    drain_handle: JoinHandle<()>,
    process_handle: JoinHandle<()>,
}

impl VideoReceiver {
    pub fn spawn(
        name: String,
        stream: NativeVideoStream,
        sync_buffer: Arc<Mutex<SyncBuffer>>,
        slots: Arc<VideoTrackSlots>,
        obs_sink: Arc<ObservationSink>,
        metrics: Arc<TrackMetrics>,
    ) -> Self {
        let channel = Arc::new(FrameChannel::new(RECV_CHANNEL_CAPACITY));

        // Drain task: pull decoded frames off the native stream as fast as
        // they arrive and hand them to the processing task. This task does
        // the bare minimum and never touches user callbacks, sync, or
        // dispatch — any stall here lets libwebrtc's native receive queue
        // overflow and drop frames in bulk. Its only per-frame cost is
        // bounded CPU: colour-convert plus a latest-wins slot update.
        let drain_channel = channel.clone();
        let drain_slots = slots.clone();
        let drain_name = name.clone();
        let drain_handle = tokio::spawn(async move {
            let mut stream = stream;
            let mut dropped_total: u64 = 0;
            let mut last_warn: Option<Instant> = None;
            while let Some(frame) = stream.next().await {
                // Hard requirement: every frame must carry a user_timestamp.
                // Portal-published tracks set this automatically; subscribed
                // tracks from other publishers must do the same. See the
                // "Sender requirement" note in README.md.
                let timestamp_us = frame.frame_metadata.and_then(|m| m.user_timestamp).expect(
                    "video frame missing user_timestamp — \
                         sender must enable PacketTrailerFeatures.user_timestamp",
                );
                let frame_data = convert_frame(&frame, timestamp_us);
                let frame_arc = Arc::new(frame_data);

                metrics.record_received(timestamp_us, now_us());

                // Freshest-frame slot for `get_video_frame`. Updated on the
                // drain so polling consumers see the newest frame even when
                // per-frame callbacks fall behind. Clone is cheap — pixel
                // buffer is `Bytes`.
                *drain_slots.latest.lock() = Some((*frame_arc).clone());

                if drain_channel.push(frame_arc) {
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
        // for shared locks — the per-frame callback, sync-buffer push, and
        // observation dispatch. Falling behind here drops frames at the
        // channel instead of stalling the drain.
        let process_handle = tokio::spawn(async move {
            while let Some(frame_arc) = channel.recv().await {
                if let Some(cb) = slots.cb.lock().as_ref() {
                    // User callback runs on this tokio worker; a panic would
                    // abort the task and silently stop delivering frames.
                    // Catch and log.
                    let result = catch_unwind(AssertUnwindSafe(|| cb(&name, &frame_arc)));
                    if result.is_err() {
                        log::error!(
                            "[callback-panic] video frame callback panicked on track '{name}', receive loop continues"
                        );
                    }
                }
                let output = sync_buffer.lock().push_frame(&name, frame_arc);
                if !output.is_empty() {
                    obs_sink.dispatch(output);
                }
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
    VideoFrameData { width, height, data: Bytes::from(buf), timestamp_us }
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

pub(crate) fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ts: u64) -> Arc<VideoFrameData> {
        Arc::new(VideoFrameData {
            width: 2,
            height: 2,
            data: Bytes::from_static(&[0u8; 12]),
            timestamp_us: ts,
        })
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
