# Benchmark example

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../../../docs/images/example-benchmark-dark.svg">
  <img alt="The robot and the teleoperator with each stream between them, p50 on a local server: video 5.9 ms and state 1.2 ms to the teleoperator, which matches them into an observation at 6.2 ms, action 0.5 ms back, full loop 6.8 ms." src="../../../docs/images/example-benchmark-light.svg" width="100%">
</picture>

Measure latency, jitter, and gaps between arrivals for video, state, observations,
and actions. The robot streams synthetic data; the operator replies to each
observation immediately.

Both peers use the robot's clock to measure one-way latency across machines.

## Requirements

- Python 3.12 and [`uv`](https://docs.astral.sh/uv/)
- A LiveKit server: local `livekit-server --dev` or
  [LiveKit Cloud](https://cloud.livekit.io). Results include network latency.

## Run

From this directory, copy `.env.example`, set your LiveKit URL and credentials,
and install dependencies:

```bash
cp .env.example .env
uv sync
```

Run the scripts in separate terminals, on one or two machines:

```bash
uv run robot.py           # terminal 1
```

```bash
uv run teleoperator.py    # terminal 2
```

The robot waits for the operator to take control, streams through a warmup, then
measures for `PORTAL_DURATION_SECONDS`. The teleoperator measures until the robot
leaves. Each side prints a live p50/p95 line every second and a results table at
the end.

## Reading the results

Sample output with a local `livekit-server --dev`, 640x480 H264 at 30 fps:

```
[teleoperator] results:
  obs      n=240    p50=   6.15ms p95=   8.30ms p99=  10.01ms max=  10.55ms jitter=    967us max_gap=  36.91ms
  video    n=240    p50=   5.92ms p95=   8.02ms p99=   9.80ms max=  10.27ms jitter=    961us max_gap=  36.92ms
  state    n=241    p50=   1.18ms p95=   1.71ms p99=   2.26ms max=   2.70ms jitter=    180us max_gap=  35.38ms
  portal   rtt_p95=1.50ms clock_uncertainty=315us sync_delta_p95=0us states_dropped=0

[robot] results:
  loop     n=240    p50=   6.84ms p95=   9.23ms p99=  11.14ms max=  35.05ms jitter=   1.25ms max_gap=  61.10ms
  action   n=240    p50=    506us p95=    840us p99=    979us max=  28.79ms jitter=    364us max_gap=  61.10ms
  send     n=240    p50=    187us p95=    324us p99=    536us max=    708us jitter=     46us max_gap=  35.06ms
  portal   e2e_p50=6.64ms e2e_p95=8.93ms
```

Stream definitions:

| Stream   | Measured on | From                         | To                                                |
| -------- | ----------- | ---------------------------- | ------------------------------------------------- |
| `send`   | robot       | `send_video_frame` called    | call returns (RGB to I420 conversion and enqueue) |
| `video`  | operator    | frame captured               | decoded frame delivered to `on_video_frame`       |
| `state`  | operator    | state captured               | state delivered to `on_state`                     |
| `obs`    | operator    | capture                      | fused observation delivered to `on_observation`   |
| `action` | robot       | operator calls `send_action` | action delivered to `on_action`                   |
| `loop`   | robot       | capture                      | corresponding action arrives                      |

Columns:

- **`p50` / `p95` / `p99` / `max`:** Latency percentiles and maximum.
- **`jitter`:** Mean absolute change in latency between consecutive samples.
- **`max_gap`:** Longest interval between arrivals; about 33 ms at a steady 30 fps.

The `portal` line reports SDK [metrics](../../../docs/09-metrics.md):
`e2e_p50/p95` measures the observation-action loop, `rtt_p95` measures round-trip
latency, and `clock_uncertainty` estimates clock error. One-way measurements below
that uncertainty are not reliable.

### Interpreting results

- `obs` waits for both video and state, so it usually tracks the slower video path.
- `loop` includes observation latency, action latency, and callback processing.
- High `loop` relative to `rtt` can indicate media pipeline overhead. Try smaller
  frames or `PORTAL_CODEC=mjpeg`.
- With `mjpeg`, `png`, or `raw`, video and state share a reliable data channel.
  Large frames can delay state delivery.
- High `send` latency blocks the robot's control loop. Try a lower resolution.

## Measuring overhead across regions

Over a real network, most of `loop` is the network itself. To see what LiveKit
and Portal add, compare three round trips taken at the same time between the
same two machines:

1. **Network floor.** A direct ping between the two machines, outside LiveKit.
   Don't route it through a VPN relay: if both machines are behind NAT, punch a
   direct UDP path rather than measuring the relay.
2. **LiveKit.** The same round trip on the plain LiveKit SDK (`livekit.rtc`):
   the robot sends a reliable data packet and a video frame stamped with
   `FrameMetadata.user_timestamp`, and the other side echoes each one back.
3. **Portal.** The same round trip through Portal: the operator replies with
   `send_action(..., in_reply_to_ts_us=...)` from `on_state`,
   `on_video_frame` and `on_observation`. This is what `loop` measures.

Time every round trip on the robot's clock, so clock sync can't skew it.
Interleave the LiveKit and Portal runs, and keep the ping running throughout,
so all three see the same network.

### Example: Vietnam to San Francisco

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../../../docs/images/example-benchmark-cloud-dark.svg">
  <img alt="Robot in San Francisco, teleoperator in Hanoi, on LiveKit Cloud, 640x480 H264 at 30 fps. Video about 124 ms and state about 111 ms to the teleoperator, observation at about 124 ms, action about 111 ms back, loop 235.1 ms. The direct ping round trip is 184.1 ms; Portal adds about 1.5 ms over plain LiveKit. One-way values are derived from round trips." src="../../../docs/images/example-benchmark-cloud-light.svg" width="100%">
</picture>

A hard case: the robot in San Francisco, the operator in Hanoi, Vietnam, both
on [LiveKit Cloud](https://cloud.livekit.io). H264 640x480 at 30 fps, 30 s per
run. Median round trips from the best of three interleaved runs:

| Round trip                        | p50      | Added                                          |
| --------------------------------- | -------- | ---------------------------------------------- |
| Direct UDP ping                   | 184.1 ms | network floor                                  |
| LiveKit SDK, data echo            | 221.0 ms | +36.9 ms LiveKit route                         |
| Portal, `on_state` → action       | 222.4 ms | +1.4 ms Portal                                 |
| LiveKit SDK, video echo           | 233.9 ms | +12.9 ms video pipeline                        |
| Portal, `on_video_frame` → action | 235.5 ms | +1.6 ms Portal                                 |
| Portal, `on_observation` → action | 235.1 ms | fusing frame and state adds nothing measurable |

Most of LiveKit's extra time is the route: each machine connects to its nearest
LiveKit edge (97 ms round trip from Hanoi, 10 ms from San Francisco). Tail
latency followed the network: p95 ranged from about 320 ms to over 1 s from run
to run, and the direct ping's p95 moved with it. Portal's share was
under 5 ms in every run that wasn't hit by a network burst. A shorter path
shrinks the network and LiveKit terms.

### Example: within San Francisco

Two machines in San Francisco, the robot on one and the operator on the other,
both on LiveKit Cloud. Traffic between them goes through the LiveKit SFU rather
than directly, so this reflects a same-region deployment. H264 640x480 at 30 fps,
30 s per run, median of three interleaved runs:

| Round trip                        | p50     | p95     | Added                         |
| --------------------------------- | ------- | ------- | ----------------------------- |
| LiveKit SDK, data echo            | 17.6 ms | 23.3 ms | LiveKit route                 |
| Portal, `on_state` → action       | 18.2 ms | 24.9 ms | +0.6 ms Portal                |
| LiveKit SDK, video echo           | 23.9 ms | 54.6 ms | +6.3 ms video pipeline        |
| Portal, `on_video_frame` → action | 26.0 ms | 56.5 ms | +2.1 ms Portal                |
| Portal, `on_observation` → action | 25.4 ms | 57.3 ms | waits for its own video frame |

In the same region the route shrinks to about 18 ms, and Portal still adds
about 1–2 ms. Video is the slower path, about 6 ms behind state, so an
observation completes when its frame arrives.

## Configuration

Set options in `.env`. Use the same `PORTAL_FPS` and `PORTAL_CODEC` on both sides.

| Variable                  | Default | Purpose                                                                                                   |
| ------------------------- | ------- | --------------------------------------------------------------------------------------------------------- |
| `LIVEKIT_URL`             | none    | Required. Server URL, e.g. `ws://localhost:7880`.                                                         |
| `LIVEKIT_API_KEY`         | none    | Required.                                                                                                 |
| `LIVEKIT_API_SECRET`      | none    | Required.                                                                                                 |
| `LIVEKIT_ROOM`            | none    | Required. `.env.example` uses `portal-benchmark`.                                                         |
| `PORTAL_FPS`              | `30`    | Capture rate.                                                                                             |
| `PORTAL_CODEC`            | `h264`  | `h264`, `vp8`, `vp9`, `av1`, `mjpeg`, `png` or `raw`. See [Frame video](../../../docs/07-frame-video.md). |
| `PORTAL_FRAME_WIDTH`      | `640`   | Frame width. Must be even.                                                                                |
| `PORTAL_FRAME_HEIGHT`     | `480`   | Frame height. Must be even.                                                                               |
| `PORTAL_WARMUP_SECONDS`   | `3`     | Streamed but not measured, while the encoder ramps up its bitrate.                                        |
| `PORTAL_DURATION_SECONDS` | `30`    | How long the robot measures.                                                                              |

The token requests a 0–1 ms playout delay to reduce receiver buffering.

## Files

| File              | What it does                                                       |
| ----------------- | ------------------------------------------------------------------ |
| `robot.py`        | Streams video and state, measures `loop`, `action` and `send`.     |
| `teleoperator.py` | Replies to every observation, measures `obs`, `video` and `state`. |
| `_common.py`      | Settings, tokens, test frames, pacing, and statistics.             |
