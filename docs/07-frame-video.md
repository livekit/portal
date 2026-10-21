# Frame video

Choose a video path based on what consumes the images. A human teleoperator
usually needs a responsive preview. A policy or dataset may also need precise
pixel values or frames compressed independently.

Portal defaults to H.264 over WebRTC. To send each frame over a reliable byte
stream instead, select `RAW`, `PNG`, or `MJPEG` in `add_video`. RAW and PNG
preserve RGB pixels; MJPEG is lossy. All three use the same frame API and
participate in observation matching.

## When to use it

| Goal | Use |
|---|---|
| Live preview, teleop, monitoring | `add_video(name)`. Default H.264 on the WebRTC path. |
| Closed-loop policy inference | `add_video(name, codec=VideoCodec.MJPEG)` |
| Bit-exact frames for training data or benchmarks | `add_video(name, codec=VideoCodec.PNG)` |
| Small frames you want byte-for-byte with zero encode cost | `add_video(name, codec=VideoCodec.RAW)` |

The byte-stream codecs are `RAW`, `PNG`, and `MJPEG`. Everything else (`H264`,
`VP8`, `VP9`, `AV1`, `H265`) rides the WebRTC media path. See
[Portal API](05-portal-api.md#video-codecs) for the WebRTC options and
`max_bitrate_kbps`.

> **Note.** There is no default byte-stream codec. `add_video(name)` with no
> `codec` gives you H.264 on the WebRTC path. You have to name a byte-stream
> codec explicitly to leave that path.

## The API does not change

```python
from livekit.portal import DType, RobotConfig, VideoCodec

cfg = RobotConfig("session-1")

cfg.add_video("preview")                                    # H264, WebRTC
cfg.add_video("front", codec=VideoCodec.MJPEG, quality=90)   # byte stream
cfg.add_video("wrist", codec=VideoCodec.PNG)                 # byte stream
cfg.add_video("debug", codec=VideoCodec.RAW)                 # byte stream

cfg.add_state_typed([("j1", DType.F32)])
```

Sending is identical for every track, regardless of transport:

```python
robot.send_video_frame("preview", rgb_array, timestamp_us=ts)
robot.send_video_frame("front", rgb_array, timestamp_us=ts)
```

So is receiving. `frame.data` is packed RGB24 in both directions, and
frame-video tracks participate in observations exactly like WebRTC tracks:

```python
from livekit.portal import frame_bytes_to_numpy_rgb

def on_observation(obs):
    frame = obs.frames["front"]
    rgb = frame_bytes_to_numpy_rgb(bytes(frame.data), frame.width, frame.height)
    # rgb is uint8, shape (H, W, 3). Byte-identical to what the robot sent,
    # for RAW and PNG. Visually near-identical for MJPEG at q=90.
```

Track names must be unique across all `add_video` calls, whatever the codec.

## Picking a codec

Measured on real robot camera frames (an xlerobot head camera and wrist
cameras) on an Apple M4 Pro:

| Codec | Lossless | Size vs raw RGB | CPU per frame at 640x480 |
|---|---|---|---|
| `RAW` | yes | 1.6 to 2.5x smaller, from byte-stream deflate | 12 to 16 ms to deflate on send |
| `PNG` | yes | 1.6 to 3x smaller | about 1.3 ms encode, 2 ms decode |
| `MJPEG` q90 | no | 8 to 17x smaller | under 1 ms each way |

For inference that can accept lossy images, start by evaluating MJPEG at
quality 90. Measure image quality and latency with your camera data and policy;
encoded size depends on the scene. A busy head camera encodes about 50% larger
than a wrist camera looking at a gripper.

Use PNG when you need exact pixels. It is about as small as deflated RAW and far
cheaper to produce, so RAW only makes sense at small resolutions or when the
sender has CPU to spare.

## Throughput and latency

LiveKit byte streams cut each frame into 15 KB chunks and send them over one
SCTP data channel. Throughput is bounded by that channel, not by encoding. On
localhost it carries about **5 MB/s in total**, shared by every byte-stream
track, so per-frame latency and the frame-rate ceiling both follow from the
encoded size.

Measured end to end through the Python API, on localhost against
`livekit-server --dev`, with the real camera frames above. Latency is the median
from `send_video_frame` to the operator's callback, sent at 5 fps so nothing
queues. Max fps is one track pushed flat out.

| Resolution | Codec | Encoded | Latency | Max fps per track |
|---|---|---|---|---|
| 224x224 | MJPEG q90 | 15 to 19 KB | 8 to 17 ms | 260 to 310 |
| 224x224 | PNG | 68 to 92 KB | 23 to 35 ms | 60 to 80 |
| 224x224 | RAW | 150 KB, about 85 KB deflated | 27 to 39 ms | 63 to 68 |
| 320x240 | MJPEG q90 | 21 to 27 KB | 10 to 22 ms | 200 to 250 |
| 320x240 | PNG | 97 to 135 KB | 34 to 56 ms | 41 to 56 |
| 320x240 | RAW | 230 KB, about 125 KB deflated | 35 to 46 ms | 40 to 48 |
| 480x360 | MJPEG q90 | 37 to 51 KB | 16 to 18 ms | 140 to 150 |
| 480x360 | PNG | 181 to 265 KB | 52 to 67 ms | 20 to 30 |
| 480x360 | RAW | 518 KB, about 250 KB deflated | 62 to 68 ms | 25 to 27 |
| 640x480 | MJPEG q90 | 57 to 84 KB | 38 to 39 ms | 74 to 96 |
| 640x480 | PNG | 293 to 456 KB | 71 to 100 ms | 11 to 18 |
| 640x480 | RAW | 922 KB, about 400 KB deflated | 93 to 106 ms | 14 to 16 |
| 720p | MJPEG q90 | about 210 KB | 61 ms | 26 |
| 1080p | MJPEG q90 | about 380 KB | 95 ms | 14 |

The 720p and 1080p rows use a downscaled photo, since the robot cameras were
640x480. Expect a real network to be slower than localhost, never faster.

Reading that table:

- MJPEG sustains 30 fps up to 640x480, and 720p only at about 25 fps.
- PNG and RAW sustain 30 fps up to about 320x240. At 224x224, the standard VLA
  inference size, both clear 60 fps, so bit-exact RGB is genuinely on the table.
- Nothing here reaches 30 fps at 1080p. Reduce resolution or frame rate, or use
  the WebRTC path.
- Latency grows with size even when there is headroom: a 640x480 MJPEG frame
  takes about 40 ms, a 224x224 one under 20 ms.

### Budget across tracks

Those are per-track ceilings with nothing else sending. Every byte-stream track
shares the same data channel, and the total stays at about 5 MB/s whether it is
split across one, two or three tracks:

```
encoded_bytes_per_frame × fps × n_tracks  ≤  about 5 MB/s on localhost
```

Three 640x480 MJPEG cameras at 30 fps is about 6.5 MB/s, over the budget. Two
fit. Something has to give: a smaller resolution, a lower frame rate, or fewer
tracks.

## Configuration

```text
cfg.add_video(
    name: str,
    codec: VideoCodec = VideoCodec.H264,
    quality: int = 90,
    max_bitrate_kbps: int | None = None,
    *,
    simulcast: bool | None = None,
    screencast: bool | None = None,
    stall_behavior: StallBehavior | None = None,
    max_lag_ms: int | None = None,
    width: int | None = None,
    height: int | None = None,
) -> None
```

- **`codec`** is a WebRTC codec (`H264`, `VP8`, `VP9`, `AV1`, `H265`) or a
  byte-stream codec (`RAW`, `PNG`, `MJPEG`).
- **`quality`** is 1 to 100, and applies to MJPEG only. It is ignored for every
  other codec. 90 is visually near-lossless. 70 trades visible artifacts for
  roughly 2x more compression. Below 50 is unusable for inference.
- **`max_bitrate_kbps`** caps the WebRTC encoder's peak rate. `add_video`
  ignores it on the byte-stream codecs, because there is no encoder to cap, and
  the [YAML loader](reference/config-file.md) rejects it there.
- **`simulcast`** and **`screencast`** configure the WebRTC encoder and default
  to off. Like `max_bitrate_kbps`, they are ignored on the byte-stream codecs
  and rejected there in YAML. See
  [Simulcast and screencast](05-portal-api.md#simulcast-and-screencast).
- **`stall_behavior`** and **`max_lag_ms`** override the session's stall
  handling for this track. See [Tuning](06-tuning.md).
- **`width`** and **`height`** declare the frame size you send. See
  [Frame size](05-portal-api.md#frame-size).

## Metrics

Frame-video tracks carry per-track byte counters that WebRTC tracks cannot,
because Portal does the encoding itself:

```python
t = portal.metrics().transport

t.bytes_sent["front"]                        # cumulative payload bytes (RAW: before deflate)
t.bytes_received["front"]                    # same, operator side
t.frames_dropped_publisher_full["front"]     # replaced by a newer frame before sending
```

Check the real encoded size of a deployed track against the budget:

```python
avg_bytes = t.bytes_sent["front"] / t.frames_sent["front"]
mb_per_s = avg_bytes * fps / 1e6
```

For RAW, `bytes_sent` counts the payload before deflate, so this overstates it
by about 2x on camera images.

A track that measured 20 KB per frame on a test fixture and 60 KB in production
means your real camera has more entropy than the fixture did. Re-check the
tables above with the real number.

`frames_dropped_publisher_full` climbing means the publisher is offering frames
faster than the link ships them. Each track sends one frame at a time and holds
only the newest one waiting behind it, so a slow link costs frame rate, not
latency. See [`publish-full`](10-troubleshooting.md#publish-full).

Full field list in [Metrics](09-metrics.md#transport).

## Wire format

One byte stream per frame, all tracks sharing the topic
`portal_frame_video`. The header is 16 fixed bytes plus the track name:

```
[u8  version = 1]
[u8  codec_id = 0 RAW | 1 PNG | 2 MJPEG]
[u16 width            little-endian]
[u16 height           little-endian]
[u64 timestamp_us     little-endian]
[u16 track_name_len   little-endian]
[u8 × track_name_len  track name, UTF-8]
[u8 × N               encoded payload]
```

Width and height fit in `u16`, which caps at 65535 and is far beyond any real
camera. Track names are capped at 256 bytes on both send and receive. The
receiver demultiplexes by the track name in the header, which is how several
byte-stream tracks share one topic.

Frame loss is contained. Byte streams behave like TCP, so a frame either arrives
whole or does not arrive. A lost frame does not corrupt anything and does not
affect state synchronization. It just means one fewer candidate for matching.

Building a peer in another SDK? The full contract is in
[Wire protocol](reference/wire-protocol.md#frame-video).

## Next steps

- [Tuning](06-tuning.md). Match-window knobs, which interact with frame rate.
- [Metrics](09-metrics.md). The counters above, in full.
- [Troubleshooting](10-troubleshooting.md). Frame-video warning tags.
