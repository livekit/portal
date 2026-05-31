# Frame Video

Per-frame video over LiveKit byte streams, bypassing the WebRTC media
codec. Use this when the frames feeding your policy must arrive as the
same RGB bytes the camera produced — no I420 conversion, no temporal
codec, no quality drift.

Selected by passing a byte-stream codec to `add_video`. The byte-stream
codecs are `RAW`, `PNG`, and `MJPEG`; everything else (`H264`, `VP8`,
`VP9`, `AV1`, `H265`) rides the WebRTC media path instead — see
[portal-api.md](portal-api.md#webrtc-video-options) for the WebRTC codec
and `max_bitrate_kbps` options.

## When to use it

| Goal | Pick |
|------|------|
| Live preview, teleop, video monitoring | `add_video(name)` (default H264, WebRTC media path) |
| Closed-loop policy inference | `add_video(name, codec=VideoCodec.MJPEG)` |
| Bit-exact frames for benchmarks or training data | `add_video(name, codec=VideoCodec.PNG)` |
| Sub-15 KB frames you want byte-for-byte | `add_video(name, codec=VideoCodec.RAW)` |

Every codec uses the **same** user-facing API: `send_video_frame(name,
rgb)`, `on_video_frame(name, frame)`, `get_video_frame(name)`.
`frame.data` is packed RGB24 in both directions regardless of transport.
Track names must be unique across all `add_video` calls.

## Quickstart

```python
from livekit.portal import (
    DType, Portal, PortalConfig, Role,
    VideoCodec, frame_bytes_to_numpy_rgb,
)

cfg = PortalConfig("session", Role.ROBOT)

# Lossy WebRTC video — adaptive bitrate, low CPU. Default codec is H264.
cfg.add_video("preview")

# Byte-stream video — reliable per-frame transport, per-frame codec.
cfg.add_video("front", codec=VideoCodec.MJPEG, quality=90)
cfg.add_video("wrist", codec=VideoCodec.PNG)
cfg.add_video("debug", codec=VideoCodec.RAW)

cfg.add_state_typed([("j1", DType.F32)])

portal = Portal(cfg)
await portal.connect(url, token)

portal.send_video_frame("preview", rgb_array)   # WebRTC
portal.send_video_frame("front", rgb_array)     # MJPEG byte stream
```

On the operator side, `Observation.frames["front"]` carries the same
decoded RGB the publisher passed in. Helper:

```python
arr = frame_bytes_to_numpy_rgb(bytes(frame.data), frame.width, frame.height)
# arr is uint8 of shape (H, W, 3).
```

## Codec choice

| Codec | Lossless | Compression | Encode/decode CPU | Single-chunk frame size cap |
|-------|----------|-------------|-------------------|-----------------------------|
| `RAW` | yes | none | none | ~70×70 RGB |
| `PNG` | yes | ~2-3× on natural images | ~3-10 ms / 480p | ~150×150 RGB |
| `MJPEG` (default) | no | ~10-20× at q=90 | sub-1 ms / 480p | up to ~480p |

The "single-chunk" column matters because of the latency floor below.
A frame whose encoded payload fits in one 15 KB chunk pays the floor
once. A frame spilling into N chunks pays the floor N times.

## Latency floor

LiveKit byte streams fragment payloads at 15 KB and ship each chunk
through a single SCTP data channel. The drain rate is bounded by
`buffered_amount` flow control inside libwebrtc, not by Portal's
encode cost. On localhost we measure:

```
latency ≈ 1 ms + 2 ms × ⌈encoded_size / 15 KB⌉
```

Per-frame send time ≈ this latency, so **max sustainable fps per track
≈ 1000 / (1 + 2·chunks)**.

| Resolution | Codec | Encoded | Chunks | Max fps per track |
|------------|-------|---------|--------|-------------------|
| 224×224    | RAW   | 150 KB  | 11     | ~43 |
| 224×224    | MJPEG q90 | ~10 KB | 1   | ~330 |
| 320×240    | RAW   | 230 KB  | 16     | ~30 |
| 320×240    | MJPEG q90 | ~15 KB | 1-2 | ~200-330 |
| 480×360    | RAW   | 518 KB  | 35     | ~14 |
| 480×360    | MJPEG q90 | ~30 KB | 2-3 | ~140-200 |
| 640×480    | RAW   | 922 KB  | 62     | ~8 |
| 640×480    | MJPEG q90 | ~60 KB | 4-5 | ~90-110 |
| 720p       | RAW   | 2.7 MB  | 185    | ~2.7 |
| 720p       | MJPEG q90 | ~180 KB | 12-15 | ~30-40 |
| 1080p      | RAW   | 6.1 MB  | 415    | ~1.2 |
| 1080p      | MJPEG q90 | ~410 KB | 28-35 | ~14-17 |

**Reading the table:**

- 30 fps closed-loop control at 1080p needs MJPEG. RAW caps at ~1 fps.
- MJPEG hits real-time at every resolution up to 720p.
- RAW is real-time only at small resolutions (≤320×240).
- 224×224 (the standard VLA inference camera size) is fast enough on
  RAW that bit-exact RGB is on the table at 30+ fps.

These are per-track ceilings. Multiple tracks share the same SCTP
data channel. Budget total chunks across all tracks combined:

```
budget = chunks_per_frame × fps × n_tracks  ≤  ~500 chunk-sends/sec
```

A 3-camera 30 fps workload at 12 chunks per frame already hits 1080
chunk-sends/sec — over the budget. Drop a codec, drop a resolution,
drop fps, or drop a track.

## What about the regular WebRTC video path?

`add_video(name)` defaults to `VideoCodec.H264` and uses the WebRTC
media channel: H.264 over RTP, adaptive bitrate, hardware-accelerated
where available. It is the right call for live preview and teleop,
where the operator is watching the video and bandwidth adapts to the
link.

It is the wrong call when:

- A policy reads the pixels — H.264 introduces colorspace shift,
  block artifacts, and rate-adaptive quality drift.
- You need bit-exact frames for training data or regression tests.
- Frame rate must be deterministic. WebRTC's encoder will drop frames
  silently to fit a bitrate target.

Picking a byte-stream codec on the same `add_video` call routes the track
through the byte-stream transport, trading adaptive bitrate for
deterministic per-frame delivery and bit-exact RGB. The other WebRTC
codecs (`VP8`/`VP9`/`AV1`/`H265`) and the `max_bitrate_kbps` cap are
documented in [portal-api.md](portal-api.md#webrtc-video-options).

## Configuration

```python
PortalConfig.add_video(
    name: str,
    codec: VideoCodec = VideoCodec.H264,
    quality: int = 90,
    max_bitrate_kbps: int | None = None,
)
```

- `codec` is one of the WebRTC codecs `VideoCodec.H264` / `VP8` / `VP9` /
  `AV1` / `H265`, or a byte-stream codec `VideoCodec.RAW` / `PNG` / `MJPEG`.
- `max_bitrate_kbps` caps the WebRTC encoder's peak rate (a ceiling, not a
  target). Defaults to 10 Mbps. Rejected on the byte-stream codecs.
- `quality` is `1..=100` for MJPEG, ignored for every other codec.
  Quality 90 is visually near-lossless on natural images and produces
  10-20× compression. Quality 70 trades visible artifacts for ~2× more
  compression. Quality below 50 is unusable for inference.
- Track names must be unique across all `add_video` calls regardless
  of codec. A duplicate raises.

## Metrics

`portal.metrics().transport` carries per-track byte counters for
frame-video tracks:

- `bytes_sent[track]` — cumulative on-wire payload bytes.
- `bytes_received[track]` — cumulative on-wire payload bytes (operator).
- `frames_dropped_publisher_full[track]` — frames the publisher
  dropped because the in-flight queue was at the cap.

Derived:

```python
sent = portal.metrics().transport
chunks_per_frame = sent.bytes_sent[track] / sent.frames_sent[track] / 15000
```

Use this to confirm a deployed track is in the regime you expected.
A track sized for "1 chunk" that surfaces as 4 chunks in production
means your camera is delivering richer entropy than the test fixture.

## Wire format

One byte stream per frame on topic `portal_frame_video`. Header is
16 bytes plus the track name:

```
[u8  version = 1]
[u8  codec_id = 0|1|2 (RAW|PNG|MJPEG)]
[u16 width  little-endian]
[u16 height little-endian]
[u64 timestamp_us little-endian]
[u16 track_name_len little-endian]
[u8 × track_name_len  utf-8 bytes]
[u8 × N  encoded codec payload]
```

Width and height fit in `u16` (max 65535×65535 — far beyond any real
camera). Track name is capped at 256 bytes on send and receive. The
receiver dispatches frames by track name in the header, so multiple
byte-stream tracks share one topic.

Frame loss is contained: byte streams are TCP-like, frames either
arrive whole or do not arrive at all. A dropped frame does not affect
state synchronization.
