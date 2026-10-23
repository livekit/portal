# Benchmark example

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../../../docs/images/example-benchmark-dark.svg">
  <img alt="p50 latency per stream on a local server: send 0.2 ms, state 1.2 ms, video 5.9 ms, observation 6.2 ms, action 0.5 ms, full loop 6.8 ms." src="../../../docs/images/example-benchmark-light.svg" width="100%">
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

| Stream | Measured on | From | To |
|---|---|---|---|
| `send` | robot | `send_video_frame` called | call returns (RGB to I420 conversion and enqueue) |
| `video` | operator | frame captured | decoded frame delivered to `on_video_frame` |
| `state` | operator | state captured | state delivered to `on_state` |
| `obs` | operator | capture | fused observation delivered to `on_observation` |
| `action` | robot | operator calls `send_action` | action delivered to `on_action` |
| `loop` | robot | capture | corresponding action arrives |

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

## Configuration

Set options in `.env`. Use the same `PORTAL_FPS` and `PORTAL_CODEC` on both sides.

| Variable | Default | Purpose |
|---|---|---|
| `LIVEKIT_URL` | none | Required. Server URL, e.g. `ws://localhost:7880`. |
| `LIVEKIT_API_KEY` | none | Required. |
| `LIVEKIT_API_SECRET` | none | Required. |
| `LIVEKIT_ROOM` | none | Required. `.env.example` uses `portal-benchmark`. |
| `PORTAL_FPS` | `30` | Capture rate. |
| `PORTAL_CODEC` | `h264` | `h264`, `vp8`, `vp9`, `av1`, `mjpeg`, `png` or `raw`. See [Frame video](../../../docs/07-frame-video.md). |
| `PORTAL_FRAME_WIDTH` | `640` | Frame width. Must be even. |
| `PORTAL_FRAME_HEIGHT` | `480` | Frame height. Must be even. |
| `PORTAL_WARMUP_SECONDS` | `3` | Streamed but not measured, while the encoder ramps up its bitrate. |
| `PORTAL_DURATION_SECONDS` | `30` | How long the robot measures. |

The token requests a 0–1 ms playout delay to reduce receiver buffering.

## Files

| File | What it does |
|---|---|
| `robot.py` | Streams video and state, measures `loop`, `action` and `send`. |
| `teleoperator.py` | Replies to every observation, measures `obs`, `video` and `state`. |
| `_common.py` | Settings, tokens, test frames, pacing, and statistics. |
