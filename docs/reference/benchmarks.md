# Benchmarks

How long a frame takes from `send_video_frame` on the robot to the operator's
callback, and how that compares with the plain LiveKit Python SDK doing the same
work. On every WebRTC codec and size Portal lands at 9 to 21 ms, the same as the
SDK in a room with tight playout delay bounds.

**Setup.** One Apple M4 Pro running macOS 26.6, a local `livekit-server --dev`
1.13.5, Portal v0.3 built in release mode, and `livekit` 1.1.21 for the SDK. The
robot and the operator run as separate processes. Frames are 30 fps RGB24, a
moving gradient with mild noise, so the encoders do real work. Each run warms up
for 8 s and measures for 10 s.

**Reading the charts.** Each bar is the median of three runs' p50. The thin line
spans the fastest to the slowest run. Latency is the operator's clock minus the
timestamp the robot put on the frame. Both run on one host, so they share a
clock.

The SDK side publishes a camera track sized to the frame, with the same codec,
bitrate ceiling and frame-rate ceiling as Portal, and `user_timestamp` frame
metadata. It receives frames as RGB24, as Portal delivers them. It runs twice: in
a room created with default settings, and in one created with
`min_playout_delay=0, max_playout_delay=1`, as the
[quickstart](../01-quickstart.md) does. Portal declares each track with `width`
and `height`, in a room with default settings.

## Video latency

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="benchmarks/e2e-latency-dark.svg">
  <img alt="End-to-end video latency for H.264, VP8, VP9 and AV1 at 480p, 720p and 1080p: Portal, the LiveKit Python SDK, and the SDK in a room with tight playout delay bounds" src="benchmarks/e2e-latency-light.svg">
</picture>

- **Portal matches the SDK at its fastest.** Every codec and size lands within
  about 1 ms of the SDK in a room with tight playout delay bounds.
- **Playout delay is most of the difference between codecs.** By default
  libwebrtc's receiver holds decoded frames back to smooth playback. In a default
  room the SDK waits 45 to 146 ms on VP8, VP9 and AV1 at 1080p. Portal turns that
  off on its own receivers, so it does not depend on how the room was created.
- **H.264 is the steadiest**, 9 to 21 ms from 480p to 1080p, with almost no
  spread between runs. VP9 and AV1 come in a little lower above 480p.

<details>
<summary>Table</summary>

| | Portal | LiveKit SDK | LiveKit SDK, room playout delay |
|---|---|---|---|
| H.264 480p | 9.3 (9.1–9.3) | 9.5 (9.5–9.6) | 9.3 (9.2–9.4) |
| H.264 720p | 14 (14–15) | 15 (15–15) | 14 (14–14) |
| H.264 1080p | 21 (21–21) | 22 (22–22) | 21 (21–21) |
| VP8 480p | 15 (15–16) | 67 (54–96) | 14 (14–16) |
| VP8 720p | 13 (13–13) | 100 (45–105) | 13 (13–13) |
| VP8 1080p | 16 (16–16) | 146 (142–173) | 15 (15–15) |
| VP9 480p | 13 (12–13) | 40 (31–42) | 13 (12–13) |
| VP9 720p | 12 (12–12) | 50 (42–56) | 12 (12–12) |
| VP9 1080p | 13 (13–14) | 83 (70–135) | 12 (12–13) |
| AV1 480p | 8.8 (8.8–9.0) | 24 (23–25) | 8.7 (8.6–8.8) |
| AV1 720p | 13 (13–13) | 13 (13–15) | 12 (12–13) |
| AV1 1080p | 15 (15–15) | 45 (37–51) | 14 (14–14) |

</details>

## Time the send call blocks

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="benchmarks/send-blocking-dark.svg">
  <img alt="Time send_video_frame blocks the caller, Portal compared with the LiveKit Python SDK's capture_frame" src="benchmarks/send-blocking-light.svg">
</picture>

`send_video_frame` returns in 1.5 ms or less on every WebRTC codec and size, so
a 30 fps loop spends at most 5% of its frame budget sending. Encoding happens on
libwebrtc's own threads after the call returns.

Portal blocks 0.3 to 0.4 ms longer than the SDK at 1080p on VP8, VP9 and AV1.
That is the copy `send_video_frame` makes of a numpy array (`tobytes()`, 0.29 ms
for a 1080p frame) before handing it to Rust. Passing `bytes` skips it.

<details>
<summary>Table</summary>

| | Portal | LiveKit SDK |
|---|---|---|
| H.264 480p | 0.5 (0.5–0.5) | 0.5 (0.5–0.5) |
| H.264 720p | 1.5 (1.4–1.5) | 1.1 (1.1–1.1) |
| H.264 1080p | 1.4 (1.0–1.5) | 1.8 (1.8–1.9) |
| VP8 480p | 0.1 (0.1–0.1) | 0.1 (0.1–0.1) |
| VP8 720p | 0.4 (0.4–0.4) | 0.2 (0.2–0.2) |
| VP8 1080p | 0.8 (0.7–0.8) | 0.4 (0.4–0.4) |
| VP9 480p | 0.1 (0.1–0.1) | 0.1 (0.1–0.1) |
| VP9 720p | 0.4 (0.4–0.4) | 0.2 (0.2–0.2) |
| VP9 1080p | 0.8 (0.7–0.8) | 0.4 (0.4–0.5) |
| AV1 480p | 0.4 (0.4–0.4) | 0.4 (0.4–0.4) |
| AV1 720p | 0.4 (0.4–0.4) | 0.2 (0.2–0.2) |
| AV1 1080p | 0.7 (0.7–0.8) | 0.4 (0.4–0.4) |

</details>

## Control

State from the robot and actions from the operator, 100 Hz each, with 14 `f32`
fields per sample. The last two rows add H.264 720p video at 30 fps alongside
state and actions at 90 Hz.

| | Portal | LiveKit SDK | LiveKit SDK, room playout delay |
|---|---|---|---|
| State, robot to operator | 1.9 (1.7–1.9) | 1.8 (1.7–1.8) | 1.7 (1.7–1.7) |
| Action, operator to robot | 1.9 (1.8–1.9) | 1.7 (1.5–1.8) | 1.7 (1.7–1.7) |
| H.264 720p video, with control | 14 (13–14) | 14 (14–14) | 14 (13–14) |
| State, alongside H.264 720p | 1.2 (1.1–1.2) | 1.2 (1.1–1.3) | 1.1 (1.0–1.2) |

All values are p50 in milliseconds: the median of three runs, then the fastest
and slowest run. Video arrived at 30 fps in every run on every side, with one
SDK run at 29.7.

## Frame video

The byte-stream codecs have no SDK equivalent, so these are Portal alone, one run
each. MJPEG, PNG and Raw travel as LiveKit byte streams. Those send one chunk at a
time and top out at about 12 MB/s on this setup, so past that rate the link sets
the frame rate, not the encoder. Portal keeps only the newest unsent frame, so a
slow link costs frame rate instead of latency.

| | Send blocks (ms) | End-to-end (ms) | Received fps |
|---|---|---|---|
| MJPEG 480p | 1.1 | 15 | 30 |
| MJPEG 720p | 2.4 | 24 | 30 |
| MJPEG 1080p | 6.4 | 253 | 26 |
| PNG 480p | 1.4 | 241 | 23 |
| PNG 720p | 4.1 | 334 | 8 |
| PNG 1080p | 9.1 | 510 | 4 |
| Raw 480p | 0.1 | 269 | 14 |
| Raw 720p | 0.5 | 418 | 5 |
| Raw 1080p | 1.0 | 690 | 2 |

MJPEG encodes with the libjpeg-turbo inside libwebrtc, at 4:4:4. It holds 30 fps
through 720p. For PNG and Raw above 480p, use a WebRTC codec, or lower the
resolution or frame rate. See [Frame video](../05-frame-video.md).

## Since v0.2.8

Same harness and machine, p50. v0.2.8 is one run, taken the day before; v0.3 is
the median of the runs above.

| | v0.2.8 send blocks | v0.3 send blocks | v0.2.8 end-to-end | v0.3 end-to-end |
|---|---|---|---|---|
| H.264 720p | 13 ms | 1.5 ms | 49 ms | 14 ms |
| H.264 1080p | 26 ms | 1.4 ms | 49 ms | 21 ms |
| VP8 1080p | 28 ms | 0.8 ms | 121 ms | 16 ms |
| VP9 1080p | 26 ms | 0.8 ms | 132 ms | 13 ms |
| AV1 1080p | 28 ms | 0.7 ms | 106 ms | 15 ms |
| MJPEG 1080p | 64 ms | 6.4 ms | 150 ms | 253 ms |
| PNG 720p | 38 ms | 4.1 ms | 8318 ms | 334 ms |
| Raw 1080p | 31 ms | 1.0 ms | 12815 ms | 690 ms |

- **The send call** got cheaper from building the library at `opt-level = 3`
  instead of `"z"` and from passing frames into Rust without a copy. MJPEG also
  moved from the `image` crate's encoder to libjpeg-turbo.
- **WebRTC latency** fell mostly from zero playout delay on the receiver.
- **Frame-video latency** fell because the publisher used to queue up to 60
  frames behind a slow link.

MJPEG 1080p is the exception. v0.2.8 spent 64 ms encoding each frame, which held
it to 16 fps, under what the link carries. v0.3 sends 26 fps, which fills the
link, so frames wait in it instead.

## Reproduce

The harness and the raw results behind this page are in
[`scripts/bench`](../../scripts/bench/README.md).
