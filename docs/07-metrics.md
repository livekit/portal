# Metrics

> Every counter `portal.metrics()` returns, and which ones to watch.

```python
m = portal.metrics()

m.sync.observations_emitted
m.transport.frames_received["cam1"]
m.buffers.state_fill
m.rtt.rtt_us_p95
m.policy.e2e_us_p95

portal.reset_metrics()   # zero the counters, keep the session
```

`metrics()` returns a snapshot. Reading it is cheap and lock-free enough to call
on a timer. Counters are cumulative since construction or the last
`reset_metrics()`. Gauges reflect the instant you asked.

Every duration is **microseconds**. Percentiles come from a rolling
256-sample window and read `None` until enough samples land.

## The five groups

| Group | Answers |
|---|---|
| [`sync`](#sync) | Is matching working? |
| [`transport`](#transport) | Is data getting across? |
| [`buffers`](#buffers) | Are we running out of headroom? |
| [`rtt`](#rtt) | How far away is the peer? |
| [`policy`](#policy) | How long does the full loop take? |

## What to watch first

If you only instrument four numbers, use these.

| Metric | Healthy | What a bad value means |
|---|---|---|
| `sync.states_dropped` | flat | Rising means your match window is too tight, or a camera is behind. |
| `sync.observations_emitted` | rising at your fps | Flat while state arrives means sync never completes. Check `last_blocker_track`. |
| `rtt.rtt_us_p95` | stable | Spikes mean the network, not Portal. |
| `policy.e2e_us_p95` | under your control budget | This is the number that actually bounds your control loop. |

## `sync`

Operator side. Everything about turning separate streams into observations.

| Field | Type | Meaning |
|---|---|---|
| `observations_emitted` | `int` | Total observations handed to your callback. |
| `stale_observations_emitted` | `int` | Subset of the above where at least one track contributed a reused frame. Always `0` unless `reuse_stale_frames` is on. |
| `states_dropped` | `int` | States that never found a matching frame. |
| `match_delta_us_p50` | `int \| None` | Median worst-track alignment per observation. |
| `match_delta_us_p95` | `int \| None` | 95th percentile of the same. |
| `last_blocker_track` | `str \| None` | The track that most recently stalled matching. Sticky, so it still names the culprit after recovery. |

**`match_delta_us_*` is the alignment quality of your data.** For each
observation, Portal records `max |state_ts - frame_ts|` across tracks. So it is
the worst-aligned camera in that observation, not the average.

Compare it against your window. At the defaults, the window is 50 000 µs. A p95
of 8 000 means you are using a sixth of the window and could tighten
`tolerance`. A p95 near 45 000 means you are riding the edge and any added
jitter will start dropping.

**`stale_observations_emitted` is your freeze detector.** With
`reuse_stale_frames` on, a frozen camera produces observations exactly like a
healthy one. This counter is the only thing that distinguishes them. Rising here
while `observations_emitted` holds steady means video is frozen and state is
still flowing.

**`last_blocker_track` is sticky and startup-biased.** It updates when a new
block occurs, which is useful for post-hoc diagnosis. Under
`reuse_stale_frames`, it stops updating once every track has emitted once, so do
not use it to detect a freeze. Use `stale_observations_emitted`.

```python
s = portal.metrics().sync
total = s.observations_emitted + s.states_dropped
if total:
    print(f"drop rate {100 * s.states_dropped / total:.2f}%")
```

## `transport`

Both sides. Raw counts of what left and what arrived. The per-track fields are
dicts keyed by track name.

| Field | Type | Meaning |
|---|---|---|
| `frames_sent` | `dict[str, int]` | Frames handed to the transport, per track. |
| `frames_received` | `dict[str, int]` | Frames arrived, per track. |
| `frames_dropped_publisher_full` | `dict[str, int]` | Frames dropped because the publisher's in-flight queue was at its cap. |
| `bytes_sent` | `dict[str, int]` | On-wire payload bytes, per track. Frame-video tracks only. |
| `bytes_received` | `dict[str, int]` | Same, receive side. Frame-video only. |
| `states_sent` | `int` | State packets published. |
| `states_received` | `int` | State packets arrived. |
| `actions_sent` | `int` | Action packets published. |
| `actions_received` | `int` | Action packets arrived. |
| `action_chunks_sent` | `int` | Chunk byte streams published. |
| `action_chunks_received` | `int` | Chunk byte streams arrived. |
| `frame_jitter_us` | `dict[str, int]` | Inter-arrival jitter per video track. |
| `state_jitter_us` | `int` | Inter-arrival jitter for state. |
| `action_jitter_us` | `int` | Inter-arrival jitter for actions. |
| `action_chunk_jitter_us` | `int` | Inter-arrival jitter for chunks. |

**`bytes_sent` and `bytes_received` cover frame-video tracks only.** WebRTC
frames are encoded by libwebrtc inside its own transport, so Portal cannot
observe their byte count. A WebRTC track will show frames but no bytes. That is
expected, not a bug.

The same applies to `frames_dropped_publisher_full`. Only frame-video tracks can
drop there. WebRTC frames flow through libwebrtc's own backpressure.

**Jitter is an RFC 3550 inter-arrival estimate**, smoothed as an EWMA with
alpha 1/16. It measures how irregularly packets arrive, not how late they are.
Steady high jitter argues for more `slack`. A steady high RTT with low jitter
does not.

The single most useful comparison in this whole page:

```python
t = portal.metrics().transport         # on the operator
# ... and frames_sent from the robot's own metrics

# frames_received ≈ frames_sent  -> transport is fine, sync window is tight
# frames_received << frames_sent -> loss upstream of sync
```

## `buffers`

Operator side. Instantaneous fill levels, plus cumulative evictions.

| Field | Type | Meaning |
|---|---|---|
| `video_fill` | `dict[str, int]` | Frames currently buffered, per track. |
| `state_fill` | `int` | States currently buffered awaiting a match. |
| `evictions` | `dict[str, int]` | Cumulative frames evicted from overflow, per track. |

Both fill gauges are bounded by `slack`, which defaults to 5. A track sitting at
5 is at capacity and evicting.

`video_fill` pinned at capacity on one track while others sit low points at that
track running ahead. `state_fill` pinned at capacity means states are piling up
with nothing to match against, which usually means a camera stalled. See
[`state-overflow`](08-troubleshooting.md#state-overflow).

Some eviction is normal when video arrives faster than state. Newest frames are
kept, so matching still works. Eviction paired with rising `states_dropped` is
the combination that matters. See
[`video-overflow`](08-troubleshooting.md#video-overflow).

## `rtt`

Both sides. Populated only if `ping_ms` is non-zero on this side.

| Field | Type | Meaning |
|---|---|---|
| `rtt_us_last` | `int \| None` | Most recent round trip. |
| `rtt_us_mean` | `int \| None` | Mean over the rolling window. |
| `rtt_us_p95` | `int \| None` | 95th percentile over the rolling window. |
| `pings_sent` | `int` | Probes sent. |
| `pongs_received` | `int` | Probes echoed back. |

`pings_sent` climbing while `pongs_received` stays flat means the peer is gone or
not answering, which is a liveness signal that arrives before a disconnect event
does.

RTT is pure network round trip on an unreliable channel. It is **not** your
control-loop latency. For that, use `policy`.

## `policy`

The one that measures your actual loop. Populated on whichever side **receives**
correlated actions, which is normally the robot.

| Field | Type | Meaning |
|---|---|---|
| `e2e_us_p50` | `int \| None` | Median observation-to-action latency. |
| `e2e_us_p95` | `int \| None` | 95th percentile of the same. |
| `correlated_received` | `int` | Actions and chunks that carried a correlation stamp. |

This measures from the observation timestamp the operator correlated against to
the local receive time of the resulting action. It therefore includes the
robot-to-operator network hop, the match wait, your policy's inference time, and
the operator-to-robot hop. That is the number that bounds your control rate.

**It only works if the operator passes `in_reply_to_ts_us`.**

```python
def on_observation(obs):
    action = policy(obs)
    op.send_action(action, in_reply_to_ts_us=obs.timestamp_us)   # <- required
```

Without it, `correlated_received` stays at `0` and both percentiles stay `None`.
That is the fix for "my e2e metrics are empty."

Use `correlated_received` as a denominator to check what fraction of your traffic
carries timing data:

```python
t = portal.metrics()
if t.transport.actions_received:
    share = t.policy.correlated_received / t.transport.actions_received
    print(f"{share:.0%} of actions are correlated")
```

## Printing a snapshot

The examples ship a working periodic printer. Lift it from
[`examples/python/basic/_common.py`](../examples/python/basic/_common.py).

A minimal version:

```python
import asyncio


def fmt(us):
    if us is None:
        return "-"
    return f"{us / 1000:.1f}ms"


async def report(portal, every: float = 2.0):
    while True:
        await asyncio.sleep(every)
        m = portal.metrics()
        print(
            f"obs={m.sync.observations_emitted}"
            f" dropped={m.sync.states_dropped}"
            f" align_p95={fmt(m.sync.match_delta_us_p95)}"
            f" rtt_p95={fmt(m.rtt.rtt_us_p95)}"
            f" e2e_p95={fmt(m.policy.e2e_us_p95)}"
            f" fill={dict(m.buffers.video_fill)}/{m.buffers.state_fill}"
            f" blocker={m.sync.last_blocker_track}"
        )
```

Start it with `asyncio.create_task(report(op))` and cancel it before
`disconnect()`.

## Metrics against logs

Both describe the same events from different angles.

Logs tell you that something happened and what to do about it. Every drop
carries a [reference tag](08-troubleshooting.md#tag-reference) naming its cause
and fix. Warnings are throttled, so the log is not a count.

Metrics tell you how much and how often. Counters never throttle, so the metric
is the source of truth for totals.

Reach for the tag when you want the cause. Reach for the counter when you want
the number.

## Next steps

- [Troubleshooting](08-troubleshooting.md). Each warning tag, its cause, its fix.
- [Tuning](04-tuning.md). The knobs these numbers should drive.
