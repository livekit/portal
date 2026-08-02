# Tuning

> Three knobs control matching. Two more control transport reliability.

Portal assumes **unified sampling**. The robot captures a frame and a state
reading on the same tick. Every sync parameter derives from a single `fps`, and
every internal buffer shares a single `slack`.

The defaults target 30 fps video with state at 100 Hz or below, over a WAN with
roughly 50 ms RTT. If that describes your setup, you can skip this page.

```python
cfg.set_fps(30)                      # capture rate
cfg.set_slack(5)                     # ticks of buffer headroom
cfg.set_tolerance(1.5)               # match window, in ticks

cfg.set_state_reliable(True)
cfg.set_action_reliable(True)
cfg.set_ping_ms(1000)                # 0 disables RTT probing on this side
cfg.set_reuse_stale_frames(False)
```

## The three matching knobs

| Knob | Default | Controls | Change it when |
|---|---|---|---|
| `fps` | 30 | Sampling rate. Sets the tick length the other two are measured in. | Your capture rate is not 30. If video and state differ, use the **video** rate. |
| `slack` | 5 | Buffer depth, in ticks, for every internal buffer. | You need to ride out longer stalls, or your rates are asymmetric. Minimum useful value is 2. |
| `tolerance` | 1.5 | How far a state reaches to find a frame, in ticks. | See the picker below. |

The match window comes from two of them:

```
search_range = tolerance / fps
```

At the defaults that is `1.5 / 30`, so 50 ms. A state at timestamp `S` accepts a
frame anywhere in `S ± 50 ms` and takes the nearest one.

`slack` trades staleness for jitter tolerance. At 30 fps, the default 5 ticks is
about 167 ms of headroom.

## Choosing `tolerance`

This is the knob with a real tradeoff. A wide window preserves observations at
the cost of sometimes pairing a state with a frame from a neighbouring tick. A
narrow window guarantees tight pairing and drops instead.

| Your situation | Pick | Why |
|---|---|---|
| Real-time inference or control | `0.5` | A misaligned observation is silently wrong. A drop is an explicit signal you can count. |
| Recording data for VLA training | `1.5` | One tick of misalignment (33 ms at 30 fps) is invisible to a trained model. A dropped observation is lost data. |
| Teleop viewer | `1.5` | Visual continuity matters more than frame-perfect pairing. |
| Lossy link, cellular or wireless | `1.5` | Widening cuts the drop rate materially under real loss. |
| Clean local network | either | Drops are already rare, so the choice barely shows up. |
| Datasets with strict pairing downstream | `0.5` | If your tooling assumes exact pairing, drops are cheaper than mislabeled pairs. |

Keep `tolerance` at 1 or above unless you specifically want tight pairing. Below
1, the window is narrower than one frame interval and ordinary jitter starts
producing drops.

Going above 2 lets a state match a frame two ticks away. Recovery improves a
little and misalignment risk grows a lot. It is rarely worth it.

> **Note.** A wide window does not let an early state steal a frame that a later
> state has a better claim to. Portal applies a fair-share check. See
> [Synchronization](reference/synchronization.md#5-fair-share).

## Asymmetric rates

When video runs faster than state, two things change.

1. **Set `fps` to the video rate.** The match window is measured in frame
   intervals, not state intervals.
2. **Set `slack` to at least `ceil(video_rate / state_rate) + 1`.** The default
   of 5 cleanly handles up to about 4x asymmetry.

```python
# 60 fps video, 10 Hz state
cfg.set_fps(60)
cfg.set_slack(8)        # ceil(60 / 10) + 2
cfg.set_tolerance(1.5)  # ticks are video ticks now, so about 25 ms
```

Under asymmetric rates the overall drop rate scales with
`state_rate x video_loss_rate`, not with the video rate. Fewer states means
fewer things that can fail to match.

## Reusing stale frames

Off by default. Turn it on when your application would rather see a frozen frame
than lose a state entirely.

```python
cfg.set_reuse_stale_frames(True)
```

With it on, a state whose match window has elapsed falls back to the most recent
frame that track already emitted. Video freezes on the last good frame while
state keeps flowing. Every state becomes an observation, once every track has
emitted at least once.

Before that first emission there is no fallback, so the strict drop rule still
applies. That keeps the state buffer bounded if video never starts at all.

| Your situation | Pick |
|---|---|
| Real-time inference or control | `False`. A stale frame silently misaligns the perception and action loop. |
| Data collection or logging | `True`. A dropped state is lost data. A brief video freeze is recoverable. |
| Teleop viewer | `True`. Continuity beats exact pairing. |

**Watch `metrics.sync.stale_observations_emitted`.** That counter rising while
`observations_emitted` holds steady is the signal that a track is silently
frozen. It is the only reliable freeze indicator.

Two metrics become less useful once reuse is on. `match_delta_us_p95` tracks the
drift between a state and its stale frame, so it grows unbounded during a
freeze. Any alert keyed on it needs rescoping. `last_blocker_track` only updates
while a track is still waiting for its first frame, so it will not identify a
freeze after startup.

## Transport reliability

State and actions use **reliable** delivery by default, which is lossless and
ordered over SCTP. Video is always unreliable, because that is what RTP is.

```python
cfg.set_state_reliable(False)   # allow loss, no head-of-line blocking
cfg.set_action_reliable(True)   # actions usually want ordering
```

Switch state to unreliable for high-frequency control where only the newest
value matters. Under packet loss, reliable delivery retransmits and everything
behind the lost packet waits. Unreliable delivery drops it and moves on, which
is what you want when a fresher reading is already in flight.

Actions usually want ordering, because applying a stale action after a newer one
moves the arm backwards.

## RTT probing

```python
cfg.set_ping_ms(1000)   # default
cfg.set_ping_ms(0)      # stop probing from this side
```

Each side sends an unreliable ping on this cadence and the peer echoes it back.
Setting `0` disables sending. The echo path stays active either way, so the peer
can still measure. Raise the interval or disable it on bandwidth-constrained
links.

## Seeing what is actually happening

`portal.metrics()` carries the live counters. The comparisons that matter most
for tuning:

| Compare | Tells you |
|---|---|
| `sync.observations_emitted` against `sync.states_dropped` | Whether your match window is wide enough. |
| `transport.frames_received` (operator) against `frames_sent` (robot) | Whether you are losing frames in transport or missing the window. |
| `sync.match_delta_us_p95` against your window | How much of the window you are actually using. |
| `buffers.video_fill` and `buffers.state_fill` against `slack` | Whether buffers are running near capacity. |
| `sync.last_blocker_track` | Which camera is holding sync up. |

If drops are high and `frames_received` roughly equals `frames_sent`, transport
is fine and your window is too tight. Raise `tolerance`.

If `frames_received` is well below `frames_sent`, the problem sits upstream of
sync. Look at the network, the encoder, and the publish queue.

Every field is documented in [Metrics](07-metrics.md). The shipped examples
print a live summary every two seconds. Lift `periodic_metrics` from
[`examples/python/basic/_common.py`](../examples/python/basic/_common.py) for
your own scripts.

## Next steps

- [Metrics](07-metrics.md). The full counter reference.
- [Troubleshooting](08-troubleshooting.md). Warning tags and their fixes.
- [Synchronization](reference/synchronization.md). Why these knobs exist.
