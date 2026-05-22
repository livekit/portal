use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

pub(crate) struct VideoReceiver {
    task_handle: JoinHandle<()>,
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
        let handle = tokio::spawn(async move {
            let mut stream = stream;
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

                if let Some(cb) = slots.cb.lock().as_ref() {
                    // User callback runs on this tokio worker; a panic
                    // would abort the receive task and silently stop
                    // delivering frames. Catch and log.
                    let result = catch_unwind(AssertUnwindSafe(|| cb(&name, &frame_arc)));
                    if result.is_err() {
                        log::error!(
                            "[callback-panic] video frame callback panicked on track '{name}', receive loop continues"
                        );
                    }
                }
                // VideoFrameData clone is cheap — pixel buffer is `Bytes`.
                *slots.latest.lock() = Some((*frame_arc).clone());
                let output = sync_buffer.lock().push_frame(&name, frame_arc);
                if !output.is_empty() {
                    obs_sink.dispatch(output);
                }
            }
        });
        Self { task_handle: handle }
    }

    pub fn abort(&self) {
        self.task_handle.abort();
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
