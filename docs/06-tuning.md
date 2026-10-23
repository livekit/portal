# Tuning

Tune observation matching when cameras arrive late, observations are dropped,
or a camera stops streaming. Start by setting `fps` to your camera rate. Then
adjust how far apart captures may be (`tolerance`) and how much data Portal
buffers (`slack`).

For example, at 30 fps the default tolerance of 1.5 ticks allows a frame less
than 50 ms from a state sample. A narrower window gives closer capture times
but can drop more observations. A larger buffer can absorb delays but retains
older data for longer.

The following fragment shows the main settings on an existing config:

```python
cfg.set_fps(30)                      # capture rate
cfg.set_slack(5)                     # ticks of buffer headroom
cfg.set_tolerance(1.5)               # match window, in ticks

cfg.set_state_reliable(True)
cfg.set_action_reliable(True)

cfg.set_stall_behavior(StallBehavior.DROP)   # what to do about a silent track
cfg.set_max_lag_ms(166)              # how long to wait first; default slack/fps
```

## Matching settings

| Setting | Default | Controls | Change it when |
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

Choose `tolerance` based on how much capture-time difference your application
accepts. A wider window can match more observations, including frames from
neighboring ticks. A narrower window rejects those pairs.

| Your situation | Pick | Why |
|---|---|---|
| Real-time inference or control | `0.5` | A misaligned observation is silently wrong. A drop is an explicit signal you can count. |
| Recording data for training | Disable observation sync | Preserve raw timestamps and choose alignment during dataset conversion. |
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
cfg.set_slack(7)        # ceil(60 / 10) + 1
cfg.set_tolerance(1.5)  # ticks are video ticks now, so about 25 ms
```

Under asymmetric rates the overall drop rate scales with
`state_rate x video_loss_rate`, not with the video rate. Fewer states means
fewer things that can fail to match.

## Stalled tracks

If a wrist camera stops sending while the front camera continues, decide whether
your application can use an observation without a fresh wrist image. Set
`max_lag_ms` to limit how far the streams advance before resolving that
observation, and `stall_behavior` to choose the result.

**Set these on the side that consumes observations, normally the operator.**
Observations are assembled where they are received, so a stall is detected and
resolved locally by the subscriber; nothing about it crosses the wire. Setting
them on a `RobotConfig` is a no-op. In particular, `OMIT` does not send a
placeholder frame: a silent camera is by definition sending nothing, so the
receiver synthesizes the stand-in itself, using the geometry of the last frame
that track actually delivered.

```python
cfg.set_max_lag_ms(150)                 # wait this long, in stream time
cfg.set_stall_behavior(StallBehavior.OMIT)      # then resolve this way
```

`max_lag` is how far the fastest-advancing stream may run past a moment before
that moment resolves without the silent track. It is **sender-clock time, not
wall-clock** — a statement about stream position, evaluated when a packet
arrives rather than on a timer. It defaults to `slack / fps`, which is where
state-buffer capacity would have evicted the moment anyway.

`stall_behavior` picks the outcome:

| | What the consumer sees |
|---|---|
| `DROP` (default) | No observation at all. The state still reaches the drop callback, but the healthy cameras in that moment go with it. |
| `FREEZE` | The silent track's last good frame. Video freezes, state keeps flowing. |
| `OMIT` | A magenta "NO SIGNAL" placeholder naming the track, so the healthy cameras stay visible. |

Every frame carries a `FrameSource` saying which of the three it is:

```python
def on_observation(obs):
    wrist = obs.frames["wrist"]          # always present, whatever happened
    if wrist.source is not FrameSource.LIVE:
        return                            # not a measurement of this moment
```

`OMIT` never removes the key, so turning it on cannot start raising `KeyError`
in code written before it existed.

### Per track

A policy may require a fresh wrist image while a monitoring view can tolerate a
missing scene image. Override the behavior and lag limit for each camera:

```python
cfg.set_stall_behavior(StallBehavior.OMIT)      # defaults for every track
cfg.set_max_lag_ms(150)

cfg.add_video("wrist", stall_behavior=StallBehavior.DROP, max_lag_ms=40)
cfg.add_video("scene")                          # inherits OMIT / 150ms
```

Both are keyword-only and `None` inherits, so a track can override the
behavior, the budget, or both. Setters exist for the cases where the config is
built elsewhere, such as after `from_yaml`:

```python
cfg.set_track_stall_behavior("wrist", StallBehavior.DROP)
cfg.set_track_max_lag_ms("wrist", 40)
```

Rust uses the setters throughout, keeping `add_video` at its existing arity:

```rust
cfg.set_stall_behavior(StallBehavior::Omit);
cfg.set_max_lag_ms(150);
cfg.set_track_stall_behavior("wrist", StallBehavior::Drop);
cfg.set_track_max_lag_ms("wrist", 40);
```

| Your situation | Pick |
|---|---|
| Real-time inference or control | `DROP` on the tracks the policy depends on. A substituted frame silently misaligns the perception and action loop, and no observation beats a confidently wrong one. |
| Data collection or logging | Record raw streams with observation sync disabled. Preserve timestamps and choose alignment later. |
| Teleop viewer | `OMIT`. One dead camera should not blank the other three, and the operator can see which one died. |

Before a track's first frame, `FREEZE` has nothing to reuse and `OMIT` has no
geometry to synthesize from, so both wait through that startup window and then
fall back to `DROP`. That keeps the state buffer bounded if video never starts
at all.

`set_reuse_stale_frames(True)` still works, as a deprecated alias for
`set_stall_behavior(StallBehavior.FREEZE)` with `set_max_lag_ms(0)`.

**Watch `metrics.sync.stale_observations_emitted`.** That counter rising while
`observations_emitted` holds steady is the signal that a track is silently
frozen. Per frame, `frame.source` says the same thing: `STALE` for a reused
frame, `LIVE` for a fresh one.

Two metrics become less useful once reuse is on. `match_delta_us_p95` tracks the
drift between a state and its stale frame, so it grows unbounded during a
freeze. Any alert keyed on it needs rescoping. `last_blocker_track` is sticky:
it names the last track that held matching up, so it shows that a track blocked,
not that it is frozen now. With `max_lag_ms=0` (or `set_reuse_stale_frames`) it
only updates while a track is waiting for its first frame.

## Transport reliability

State and actions use reliable, ordered delivery over SCTP by default.
WebRTC video uses RTP; RAW, PNG, and MJPEG frame video use reliable byte streams.

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

## Time sync

Not tunable. Each peer pings the robot at 4 Hz until synced, then 1 Hz, with
packets of 13 and 29 bytes. RTT comes from the same exchange.

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

Every field is documented in [Metrics](09-metrics.md). For a per-stream latency and
jitter breakdown on your own network, run the
[benchmark example](../examples/python/benchmark).

## Next steps

- [Metrics](09-metrics.md). The full counter reference.
- [Troubleshooting](10-troubleshooting.md). Warning tags and their fixes.
- [Synchronization](reference/synchronization.md). Why these knobs exist.
