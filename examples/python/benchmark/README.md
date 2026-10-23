# Benchmark example

> How fast is Portal on your network, and how steady?

The same robot and operator as the [basic example](../basic), stripped down to
measure. The robot streams synthetic video and joint state. The operator replies
to every observation straight away, as a zero-latency policy would. Each side
reports latency percentiles, jitter, and the longest silence for every stream it
receives.

Every peer syncs to the robot's clock, so `now_us() - timestamp_us` is a true
one-way latency, even when the two scripts run on different machines.

## What you need

- Python 3.12 and [`uv`](https://docs.astral.sh/uv/)
- A LiveKit server. Run a local `livekit-server --dev` to measure Portal itself,
  or point at [LiveKit Cloud](https://cloud.livekit.io) to measure your network.

## Run it

```bash
cp .env.example .env      # fill in LIVEKIT_API_KEY and LIVEKIT_API_SECRET
uv sync
```

Then two terminals, on one machine or two:

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

Here is a run against a local `livekit-server --dev`, 640x480 H264 at 30 fps:

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

The streams, in the order data moves:

| Stream | Measured on | From | To |
|---|---|---|---|
| `send` | robot | `send_video_frame` called | it returns. RGB to I420 conversion plus enqueue, the time your control loop is blocked. |
| `video` | operator | frame captured | decoded frame delivered to `on_video_frame` |
| `state` | operator | state captured | state delivered to `on_state` |
| `obs` | operator | capture | fused observation delivered to `on_observation` |
| `action` | robot | operator calls `send_action` | action delivered to `on_action` |
| `loop` | robot | capture | the action that answers it arrives. This bounds your control loop. |

And the columns:

- **`p50` / `p95` / `p99` / `max`** latency percentiles. Size your control
  budget against the tail, not the median.
- **`jitter`** the mean change in latency between consecutive samples. High
  jitter with a low median means bursts: your policy sees uneven timing even
  though the average looks fine.
- **`max_gap`** the longest silence between two arrivals. At 30 fps the floor
  is about 33 ms. A large value is a stall or a freeze.

Each side also prints a `portal` line, taken from
[`metrics()`](../../../docs/09-metrics.md). The robot's `e2e_p50/p95` is
Portal's own measure of `loop`. The operator's `rtt_p95` is the network round
trip alone, and `clock_uncertainty` is how far the shared clock could be off.
Read one-way numbers smaller than it as noise.

### What to look for

- **`obs` tracks `video`, not `state`.** An observation waits for both, and
  video is the slow path: encode, packetize, jitter buffer, decode. The gap
  between `video` and `state` is the price of the video codec.
- **`loop` is about `obs + action`.** The difference is time spent in your
  callbacks. Here it is zero, so anything left over is scheduling.
- **`loop` far above `rtt`** means the cost is in the media pipeline, not the
  network. Try a smaller frame, or `PORTAL_CODEC=mjpeg`.
- **`state` rises to match `video` on a byte-stream codec.** `mjpeg`, `png`
  and `raw` frames share the reliable data channel with state, so a state
  packet can wait behind a frame.
- **`send` above a few milliseconds** eats into the robot's tick. Lower the
  resolution.

## Configuration

Everything is driven by `.env`. Robot and operator must agree on `PORTAL_FPS`
and `PORTAL_CODEC`.

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

The token sets the room's playout delay to zero, as the
[basic example](../basic) does. Without it the server lets the receiver buffer
video for smoothness, and that buffer shows up as latency.

## The files

| File | What it does |
|---|---|
| `robot.py` | Streams video and state, measures `loop`, `action` and `send`. |
| `teleoperator.py` | Replies to every observation, measures `obs`, `video` and `state`. |
| `_common.py` | Everything that isn't Portal: settings, tokens, test frames, pacing and statistics. |
