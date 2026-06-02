# Tuning

Portal assumes **unified sampling**: the robot captures state and frames at
the same tick. All sync parameters derive from a single `fps`, and all
internal buffers share a single `slack` size.

```python
config.set_fps(30)            # unified capture rate (default: 30)
config.set_slack(5)           # ticks of pipeline headroom (default: 5)
config.set_tolerance(1.5)     # match window in tick units (default: 1.5)

config.set_state_reliable(True)   # default: True
config.set_action_reliable(True)  # default: True

config.set_ping_ms(1000)      # RTT ping cadence. 0 disables. default: 1000.

config.set_reuse_stale_frames(False)  # freeze video on loss instead of dropping (default: False)
```

## The three knobs

| Parameter | What it controls | When to change |
|---|---|---|
| `fps` | Unified sampling rate. Drives the match window. | Use the **video** rate if video and state differ. Raise to 60 for high-rate robots. |
| `slack` | Ticks of pipeline headroom for every internal buffer. Larger = more jitter tolerance at the cost of staleness. | Default 5 ≈ 167 ms @ 30 fps. Bump under asymmetric rates (see below). Minimum useful value is 2. |
| `tolerance` | How far a state reaches when matching a frame, in tick units. `search_range = tolerance / fps`. | See picker below. |

## Choosing `tolerance`

| Use case | Pick | Why |
|---|---|---|
| Real-time inference / control | `0.5` | A misaligned observation is silently wrong. A drop is an explicit signal. |
| Data collection for VLA training | `1.5` | A ±1-tick misalignment (~16 ms @ 60 fps) is invisible to a trained model. A dropped observation is lost data. |
| Teleop viewer | `1.5` | Visual continuity > frame-perfect alignment. |
| Clean local network (<1% loss) | either | Drops are already rare. |
| Lossy / cellular / wireless | `1.5` | Widening materially reduces drop rate under real loss. |
| Strict-alignment datasets | `0.5` | If downstream tooling relies on exact pairing, drops are cheaper than mislabeled pairs. |

## Asymmetric rates (video faster than state)

1. **Set `fps` to the video rate**, not the state rate. The match window is
   measured in frame intervals.
2. **Set `slack ≥ ceil(video_rate / state_rate) + 1`**. Default `slack=5`
   cleanly handles up to ~4× asymmetry.

```python
# Example: 60 fps video, 10 Hz state
config.set_fps(60)
config.set_slack(8)          # ceil(60/10) + 2
config.set_tolerance(1.5)    # still measured in video-tick intervals (~16.6 ms)
```

Under asymmetric rates, the overall drop rate is proportional to
`state_rate × video_loss_rate`, not the video rate.

## Reuse stale frames

Off by default. Flip on with `config.set_reuse_stale_frames(True)` when
the application would rather see a frozen-video observation than a
dropped state. A state whose video match window has elapsed reuses the
most recent already-emitted frame on that track — video "freezes" on
the last good frame while state keeps flowing. Every state becomes an
observation once every track has emitted at least once. Before that,
the strict drop-on-horizon rule still applies so the state buffer stays
bounded if video never starts.

| Use case | Pick |
|---|---|
| Real-time inference / control | `False` — a stale frame would misalign the perception/action loop. |
| Data collection / logging | `True` — a dropped state is lost data; a transient video freeze is recoverable. |
| Teleop viewer | `True` — visual continuity > frame-perfect alignment. |

Under reuse, the freeze signal is `metrics.sync.stale_observations_emitted`
— a rising counter at steady `observations_emitted` rate means a track
is silently frozen. `match_delta_us_p95` also tracks drift between the
state and its stale frame and can be used to gauge freeze duration, but
it becomes unbounded once reuse kicks in so any alert keyed on it
should be re-scoped. `last_blocker_track` only updates while a track is
still waiting for its first frame, so don't rely on it to identify a
freeze after startup — use `stale_observations_emitted` instead.

## Reliability

State and action use **reliable (lossless, ordered)** SCTP delivery by
default. For high-frequency control where only the latest value matters,
switch to unreliable (`set_state_reliable(False)`) to avoid head-of-line
blocking under packet loss. Video is always unreliable (RTP).

```python
config.set_state_reliable(False)   # allow drops, no HOL blocking
config.set_action_reliable(True)   # actions typically want ordering
```

## Inspecting real behavior

`portal.metrics()` exposes the live sync and transport counters: RTT
percentiles, match-delta percentiles, per-track frame jitter, buffer fill,
observations emitted, states dropped. The examples under `examples/python/`
print these periodically. Adapt `periodic_metrics` from
[`examples/python/basic/_common.py`](../examples/python/basic/_common.py) for
your own scripts.

If you're seeing unexpected drops, compare
`metrics.transport.frames_received` (operator) with `frames_sent` (robot) to
distinguish transport loss from sync-window misses.
