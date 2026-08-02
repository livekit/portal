# Frame video

> Per-frame RGB over byte streams, for policies where the pixels matter.

The default video path is WebRTC H.264. It is lossy, it resamples colorspace,
and it drops frames silently to hold a bitrate target. For a human watching a
teleop feed that is exactly right. For a policy reading those pixels it can be
a problem.

Frame video is the alternative. Each frame is encoded independently and shipped
whole over a reliable byte stream. The bytes your camera produced are the bytes
your policy sees.

You select it by passing a byte-stream codec to `add_video`.

## When to use it

| Goal | Use |
|---|---|
| Live preview, teleop, monitoring | `add_video(name)`. Default H.264 on the WebRTC path. |
| Closed-loop policy inference | `add_video(name, codec=VideoCodec.MJPEG)` |
| Bit-exact frames for training data or benchmarks | `add_video(name, codec=VideoCodec.PNG)` |
| Small frames you want byte-for-byte with zero encode cost | `add_video(name, codec=VideoCodec.RAW)` |

The byte-stream codecs are `RAW`, `PNG`, and `MJPEG`. Everything else (`H264`,
`VP8`, `VP9`, `AV1`, `H265`) rides the WebRTC media path. See
[Portal API](03-portal-api.md#video-codecs) for the WebRTC options and
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

| Codec | Lossless | Compression | Encode and decode cost | Fits one chunk up to |
|---|---|---|---|---|
| `RAW` | yes | none | none | about 70x70 |
| `PNG` | yes | 2 to 3x on natural images | 3 to 10 ms at 480p | about 150x150 |
| `MJPEG` | no | 10 to 20x at q=90 | under 1 ms at 480p | about 480p |

That last column matters because of the latency floor below. A frame that fits
in one 15 KB chunk pays the floor once. A frame spread over N chunks pays it N
times.

For most inference work, **MJPEG at quality 90 is the right answer.** It is
visually near-lossless on natural images, it decodes in under a millisecond, and
it fits typical inference resolutions in one or two chunks.

Reach for PNG when you need bit-exactness and can afford the payload. Reach for
RAW only at small resolutions, where it is genuinely free.

## The latency floor

LiveKit byte streams fragment payloads at 15 KB and ship each chunk through a
single SCTP data channel. The drain rate is bounded by flow control inside
libwebrtc, not by Portal's encode cost. Measured on localhost:

```
latency ≈ 1 ms + 2 ms × ceil(encoded_size / 15 KB)
```

Per-frame send time is roughly that same number, so the ceiling per track is:

```
max fps ≈ 1000 / (1 + 2 × chunks)
```

| Resolution | Codec | Encoded | Chunks | Max fps per track |
|---|---|---|---|---|
| 224x224 | RAW | 150 KB | 11 | 43 |
| 224x224 | MJPEG q90 | 10 KB | 1 | 330 |
| 320x240 | RAW | 230 KB | 16 | 30 |
| 320x240 | MJPEG q90 | 15 KB | 1 to 2 | 200 to 330 |
| 480x360 | RAW | 518 KB | 35 | 14 |
| 480x360 | MJPEG q90 | 30 KB | 2 to 3 | 140 to 200 |
| 640x480 | RAW | 922 KB | 62 | 8 |
| 640x480 | MJPEG q90 | 60 KB | 4 to 5 | 90 to 110 |
| 720p | RAW | 2.7 MB | 185 | 2.7 |
| 720p | MJPEG q90 | 180 KB | 12 to 15 | 30 to 40 |
| 1080p | RAW | 6.1 MB | 415 | 1.2 |
| 1080p | MJPEG q90 | 410 KB | 28 to 35 | 14 to 17 |

Reading that table:

- MJPEG sustains real time at every resolution up to 720p.
- RAW is real time only at small resolutions, 320x240 and below.
- At 1080p, 30 fps closed-loop control needs MJPEG. RAW caps near 1 fps.
- At 224x224, the standard VLA inference size, even RAW clears 30 fps. Bit-exact
  RGB is genuinely on the table.

### Budget across tracks

Those are per-track ceilings. Every byte-stream track shares one SCTP data
channel, so you have to budget the total:

```
chunks_per_frame × fps × n_tracks  ≤  about 500 chunk-sends/sec
```

Three cameras at 30 fps and 12 chunks per frame is 1080 chunk-sends per second,
which is double the budget. Something has to give: a smaller resolution, a
lossier codec, a lower frame rate, or fewer tracks.

## Configuration

```text
cfg.add_video(
    name: str,
    codec: VideoCodec = VideoCodec.H264,
    quality: int = 90,
    max_bitrate_kbps: int | None = None,
) -> None
```

- **`codec`** is a WebRTC codec (`H264`, `VP8`, `VP9`, `AV1`, `H265`) or a
  byte-stream codec (`RAW`, `PNG`, `MJPEG`).
- **`quality`** is 1 to 100, and applies to MJPEG only. It is ignored for every
  other codec. 90 is visually near-lossless. 70 trades visible artifacts for
  roughly 2x more compression. Below 50 is unusable for inference.
- **`max_bitrate_kbps`** caps the WebRTC encoder's peak rate. It is rejected on
  the byte-stream codecs, because there is no encoder to cap.

## Metrics

Frame-video tracks carry per-track byte counters that WebRTC tracks cannot,
because Portal does the encoding itself:

```python
t = portal.metrics().transport

t.bytes_sent["front"]                        # cumulative on-wire bytes
t.bytes_received["front"]                    # same, operator side
t.frames_dropped_publisher_full["front"]     # dropped, in-flight queue at cap
```

Derive your actual chunk count to confirm a deployed track is in the regime you
designed for:

```python
avg_bytes = t.bytes_sent["front"] / t.frames_sent["front"]
chunks_per_frame = avg_bytes / 15_000
```

A track you sized for one chunk showing up as four in production means your real
camera has more entropy than your test fixture did. Re-check the table above
with the real number.

`frames_dropped_publisher_full` climbing means the publisher is offering frames
faster than the link ships them. See
[`publish-full`](08-troubleshooting.md#publish-full).

Full field list in [Metrics](07-metrics.md#transport).

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

- [Tuning](04-tuning.md). Match-window knobs, which interact with frame rate.
- [Metrics](07-metrics.md). The counters above, in full.
- [Troubleshooting](08-troubleshooting.md). Frame-video warning tags.
