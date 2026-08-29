# Synchronization

> How Portal fuses independently-streamed video frames and state packets into
> coherent observations.

This is background reading. You do not need it to use Portal. It is here for
people who need to reason about the algorithm, tune it against a hard latency
budget, or extend it.

For the knobs, see [Tuning](../04-tuning.md). For the short version, see
[Concepts](../02-concepts.md#the-observation-model).

## The problem

A policy expects one bundle per step: at time T, here are my camera frames and
my joint readings.

LiveKit does not deliver data that way. Each video track and each data stream is
transported independently, with its own pacing, codec path, and retransmission
behavior. The receiver sees four or more separate event streams arriving out of
phase.

`SyncBuffer` takes those uncoordinated streams and emits an
`Observation { state, frames, timestamp_us }` in which every component is close
enough in **sender time**.

## What LiveKit does and does not guarantee

The algorithm's shape is dictated by these properties, so they are worth stating
plainly.

**Timestamps are monotonic. Arrival is not.** The sender stamps each frame and
state packet from its own clock, so they are globally ordered within that
sender. Arrival order is a different matter.

- Reliable data, the default, preserves per-sender order via SCTP.
- Unreliable data may reorder or drop.
- Video packets are reassembled in order by the jitter buffer, but a frame can
  still surface late if a keyframe had to be recovered. Frame rate is noisy.

**Latency differs per stream.** Video traverses an encoder, a
congestion-controlled transport, and a decoder. Data packets traverse none of
that. Under typical conditions, video runs 30 to 80 ms behind a data packet
stamped at the same instant.

**Stalls are common.** A camera stutters. A reliable data stream pauses for
seconds during reconnection.

**There is no global clock.** Only the sender's. Receiver wall-clock time is
irrelevant to match quality, so Portal matches purely on sender timestamps.

Given all that, sync has to be **tolerant**, using a search window and buffering
on both sides. It also has to be **latency-aware**, so it never waits forever. If
a stream moves past a state's window, that state is dropped.

## Data model

Each operator session holds one `SyncBuffer` containing:

- One `VecDeque<Arc<VideoFrameData>>` per registered video track, bounded by
  `video_buffer_size`.
- One `VecDeque<(u64, Vec<f64>)>` for incoming state packets, bounded by
  `state_buffer_size`.
- Per-track `cursors: Vec<usize>`. See [two-pointer cursors](#1-two-pointer-cursors).
- A `blocker: Option<usize>` hint. See [blocker-gated sync](#2-blocker-gated-sync).
- Per-track `last_emitted_frames`, used only by
  [stale-frame reuse](#optional-stale-frame-reuse).

You do not set the buffer sizes directly. Both come from `slack`, which defaults
to 5. `search_range_us` comes from `tolerance / fps`, which is 50 ms at the
defaults.

Both deques are time-sorted in the common case, because the sender's clock is
monotonic. The algorithm leans on that throughout.

## The matching rule

For a state at timestamp `S`, a frame at `F` on track *k* is a **candidate** if
`|S - F| < search_range_us`. Among candidates, Portal picks the **nearest**,
subject to the [fair-share check](#5-fair-share).

A state produces an observation only when **every** registered track has a
candidate. If any track does not, one of three things happens.

| Track state, relative to head state `S` and range `R` | Decision |
|---|---|
| Newest frame has `ts >= S + R` | **Drop** the state. Frame timestamps are monotonic, so every future frame is at least as late. No match is ever possible. |
| No in-range frame yet, and the newest is still below `S + R`, or the buffer is empty | **Wait.** Newer frames may still land in range. |
| At least one in-range frame exists | **Match** that track, then check the others. |

The drop rule looks at the **newest** frame, not the oldest. Checking the front
would miss the case where an old frame sits below the window while the rest of the
stream has already moved past it. The state would stall until eviction dragged
that old frame out.

The `>=` is deliberate. It mirrors the strict `<` in the match rule. A frame
landing exactly at `S + R` is not a match, and no future frame can be either, so
the state drops.

Those three outcomes are the state machine the algorithm runs per head state on
every push.

## Naive version, for contrast

The straightforward implementation does this on every push:

```text
for each head state S in buffer:
    for each track k:
        scan every frame in track_k.buffer for the closest to S
    aggregate results -> match / wait / drop
```

That is `O(states × tracks × frames_per_track)` per push. At 30 fps across 3
cameras plus 100 Hz state, roughly 190 pushes per second each doing about 90
comparisons. Small in absolute terms, but it scales badly with buffer size and
it redoes the same work constantly. The same frames are rescanned for the same
state every time anything pushes.

The real cost is not the comparisons. It is the cache traffic and the lock
contention behind them.

## The optimized algorithm

Five ideas, each addressing one specific waste.

### 1. Two-pointer cursors

Both buffers are monotonic streams. For a fixed head state `S`, the best matching
frame index on each track only ever moves **forward** as `S` advances, because
state timestamps are monotonic too.

Portal keeps `cursors[track_i]` at the largest index whose frame timestamp is at
or below `S`. On each sync:

1. Advance while `buf[cursor + 1].ts <= S`.
2. Compare `buf[cursor]` and `buf[cursor + 1]` against `S`, and take the closer.

Across the whole stream each frame is inspected a constant number of times. Total
work becomes amortized `O(N + M)` instead of `O(N × M)` per call.

**Cursor rewind.** On unreliable transport, states can arrive out of order. If a
new head state has an earlier timestamp than the last, the cursor rewinds before
advancing. On reliable transport, the default, this never triggers. It is
insurance.

**Cursor adjustments on mutation.**

- On eviction, decrement by the number of evictions, saturating at 0.
- On a successful match at index `idx`, Portal drains `0..=idx` and the cursor
  becomes `saturating_sub(idx + 1)`.
- On `clear()`, all cursors reset to 0.

### 2. Blocker-gated sync

If the last `try_sync` stopped waiting on track *k*, then a push to some other
track *j* cannot unblock the head. The head is waiting on *k*, not *j*. So the
whole sync attempt can be skipped.

`SyncBuffer` records the track that last caused a wait in `blocker`.

| Trigger | Blocker state | Run `try_sync`? |
|---|---|---|
| `push_state` | any | **Yes.** A new head may be ready. |
| `push_frame` to the blocker track | `Some(self)` | **Yes.** |
| `push_frame` to a non-blocker, no eviction | `Some(other)` | **No.** Skip. |
| `push_frame` to a non-blocker, eviction happened | any | **Yes.** See below. |
| Any push | `None` | **Yes.** No hint available. |

`blocker` updates at the end of every `try_sync`. It clears on success or when
the state buffer drains. It is set to the first waiting track after a wait.

**Why the eviction escape hatch matters.** A non-blocker push usually just
appends a frame, irrelevant to the head state. But if that track is at capacity,
the append evicts the oldest frame. If that frame was the one in range for the
head state, the track just went from matching to unmatchable. Skipping sync would
stall the state silently. Running it on eviction catches the transition and
either rematches or drops.

At 30 fps across 3 cameras plus 100 Hz state, roughly 80% of frame pushes become
no-ops under this rule once steady state is reached.

### 3. O(1) drop detection

An earlier version answered "are all frames newer than `S + R`?" with
`buf.iter().all(...)`. The correct question is stronger: **can any future frame
ever match?**

Under monotonic delivery every future frame has `ts >= buf.back().ts`, so the
state is permanently unmatchable exactly when:

```text
buf.back().ts >= S + R
```

That takes `O(frames)` down to `O(1)`. It also fixes an asymmetry. The match rule
is strict, so a frame at exactly `S + R` is not a match, and `>=` makes drop
symmetric with it.

Checking the **front** instead, as an even earlier version did, only flagged the
drop once eviction had dragged the old tail through the horizon. That added
latency of up to `video_buffer_size` frames.

### 4. Eager drop across tracks

The loop classifies each track as match, wait, or drop. **Drop takes precedence
over wait.** If any track reports drop, the state drops, even if an earlier track
in iteration order said wait.

Without that, an empty `cam1` reporting wait would shield a runaway `cam2` whose
newest frame is already past the horizon. The state would stall until `cam1`
finally produced a frame, at which point it would drop anyway. Checking across
all tracks up front cuts latency on disconnects and stalls.

### 5. Fair-share

This one exists because `tolerance` defaults above 1 tick.

With a window wider than one frame interval, the head state can reach a frame
that actually belongs to the **next** state in the buffer. A greedy nearest match
would let the head steal it, leaving the later state to drop even though a
perfectly good frame had been available for it.

So before accepting a candidate frame `F` for head state `S`, Portal checks the
next buffered state `S_next`:

```text
if S_next exists and |S_next - F| < |S - F|:
    skip F      # the later state has a strictly better claim
```

The comparison is strict, so ties go to the head state, which keeps the buffer
draining.

This only matters when `tolerance > 1`. At `tolerance = 0.5` the window is
narrower than a frame interval and two states can never contend for one frame.

The test covering it is `fair_share_prevents_stealing` in `sync_buffer.rs`.

### Optional: stale-frame reuse

`stall_behavior: freeze` turns the drop outcome into a **reuse** outcome, once a track
has emitted at least once. Video freezes on a recent frame while state keeps
flowing.

There are two distinct fallbacks, and Portal prefers the first.

**Below-horizon buffered frame.** If the cursor's frame is at or before `S` and
out of range (`S - ts >= R`), that frame is used and drained. This is preferred
because it tracks forward with `S`, which keeps `match_delta` bounded, and
draining it stops the buffer wedging at capacity while a track runs
systematically behind. It is safe to consume because `S` only advances, so no
future state could fresh-match a frame that old.

**Stored last-emitted frame.** Otherwise Portal falls back to the frame that
track last emitted. The buffer, the cursor, and `last_emitted` are all left
untouched, so a later state can still claim a fresh frame that is sitting past
the horizon.

| Track state, with reuse on and `last_emitted` set | Decision |
|---|---|
| Fresh in-range match exists | **Match.** Fresh always wins, and `last_emitted` advances. |
| Cursor frame is at or before `S` and out of range | **Reuse it** and drain. `last_emitted` advances. |
| Newest frame is past the horizon | **Reuse `last_emitted`.** Buffer untouched. |
| Buffer empty | **Reuse `last_emitted`** immediately. |

During startup, before the first emission, `last_emitted` is `None` and there is
no fallback. The strict drop rule still applies, which keeps the state buffer
bounded if video never arrives at all. Once the first observation fires, reuse
takes over and drops stop.

Observations that used any stale frame increment
`metrics.sync.stale_observations_emitted`. That counter is the only way to tell a
frozen track from a healthy one, because everything else looks identical.

## Annotated `try_sync`

```text
loop:
    if state_buffer empty:
        blocker = None
        return output

    S         = state_buffer.front().ts
    S_next    = state_buffer.get(1).ts        # for fair-share
    iter_blocker = None
    should_drop  = false

    for track_i in 0..tracks:
        buf = video_buffers[track_i]

        best = None
        if not buf.empty():
            advance_cursor(track_i, S)         # with rewind if S went backwards
            for candidate in [cursor, cursor + 1]:
                d = |S - buf[candidate].ts|
                if d >= range: continue        # out of window
                if d >= best_delta: continue   # already have a closer one
                if S_next and |S_next - buf[candidate].ts| < d:
                    continue                   # fair-share: leave it
                best = candidate

        if best:
            matched[track_i] = best
            continue

        # Waiting cannot help once the newest buffered frame is past the
        # horizon; otherwise wait until the stream clock passes max_lag.
        unmatchable = buf and buf.back().ts >= S + R
        lagged = logical_now() - S >= max_lag[track_i]
        if not unmatchable and not lagged:
            iter_blocker ??= track_i
            continue

        if stall_behavior[track_i] == freeze:
            if cursor frame is <= S and out of range:
                matched[track_i] = (cursor, Stale)       # drains
                continue
            if last_emitted[track_i]:
                matched[track_i] = (last_emitted, Stale) # no drain
                continue
        elif stall_behavior[track_i] == omit:
            if last_emitted[track_i]:                    # geometry source
                matched[track_i] = (placeholder, Omitted)
                continue

        # freeze/omit reach here only before the track's first frame, with
        # nothing to substitute. Keep waiting if a frame could still match.
        if not unmatchable and stall_behavior[track_i] != drop:
            iter_blocker ??= track_i
            continue

        if buf.back().ts >= S + range:         # no future frame can match
            should_drop = true
            break                              # drop beats wait
        iter_blocker ??= track_i

    if should_drop:
        emit drop(state); pop state; continue loop

    if iter_blocker:
        blocker = iter_blocker
        return output                          # wait

    # every track matched
    pop state
    for each track with a drainable match:
        drain buf[0..=idx]
        cursors[track] -= idx + 1
        last_emitted[track] = frame
    emit observation
    continue loop                              # more states may now match
```

The outer loop drains as many backlogged states as it can in one call. That
matters because state packets arrive faster than observations and pile up during a
video stall.

## Dispatch is decoupled from matching

`try_sync` never calls user code. It returns a `SyncOutput`:

```rust
pub(crate) struct SyncOutput {
    pub observations: Vec<Observation>,
    pub drops: Vec<HashMap<String, TypedValue>>,
}
```

The caller releases the `SyncBuffer` mutex and **then** hands the output to
`ObservationSink::dispatch`, which fires callbacks and updates a latest-wins slot.

Two reasons this matters.

User callbacks cross an FFI boundary into Python, Swift, or Kotlin, and can block
for milliseconds. Running them under the sync lock would stall every frame
receiver and the room event loop.

The observation callback receives a reference, so callback-only consumers pay no
clone. The pull-based `get_observation()` reads the latest-wins slot, so a slow
poller sees the freshest observation instead of a backlog.

## Complexity and guarantees

**Amortized work.** `O(N + M)`, where N is total frames received per track and M
is total states received.

**Per-push work.** `O(1)` amortized. Worst case is `O(tracks × rewind depth)`, and
rewind depth is bounded in practice by how far out of order the sender's clock can
go, typically at most one state.

**Emit latency.** Bounded by `search_range_us` plus the slowest track's frame
inter-arrival time. A state at `S` is either emitted once every track has a frame
in `[S - R, S + R]`, or dropped once any track's newest frame passes `S + R`.

**Memory.** Bounded by `video_buffer_size × tracks + state_buffer_size`, plus one
latest-wins observation slot and one last-emitted frame per track.

## Stalled tracks

A track that stops sending would otherwise strand the head state. Two per-track
knobs decide what happens: `max_lag` is how long to wait, `stall_behavior` is what to
do when the wait is over.

Both are read by the receiving side, since `SyncBuffer` lives there (it is built
in `setup_operator` and fed from the receive paths). Nothing about a stall is
transmitted: a silent track is sending nothing by definition, so `omit`
synthesizes its stand-in locally rather than moving pixels over the wire.

**`max_lag`** is measured against the *stream clock* — the largest sender
timestamp buffered across every stream, states and all video tracks. It advances
whenever any stream is still flowing, which is how a silent track is detected
through the clocks of the tracks that are not silent. It defaults to
`slack / fps`, the point at which state-buffer capacity would have evicted the
moment anyway, so the default timing matches earlier versions.

The one case that genuinely changes: capacity eviction only runs on `push_state`,
so if state output also paused, the head previously waited forever. It now
resolves as soon as any other stream advances past the budget.

**`stall_behavior`** picks the outcome. All three are terminal.

| | result | frame tagged |
|---|---|---|
| `drop` | No observation. The state still reaches the drop callback, but the healthy tracks in that moment are discarded with it. | — |
| `freeze` | That track's last good frame. Video freezes while state keeps flowing. | `Stale` |
| `omit` | A synthesized placeholder — magenta diagonals with the track name — so the healthy tracks stay visible. | `Omitted` |

`omit` does **not** remove the map key. `frames[name]` is still present for every
declared track, which is why enabling it cannot start raising `KeyError` in code
that was written before it existed.

Before a track's first frame, `freeze` has nothing to reuse and `omit` has no
geometry to synthesize from, so both wait through that startup window and then
fall back to `drop`.

**Tell the three apart at the frame.** Every frame in an observation carries a
`FrameSource` of `Live`, `Stale`, or `Omitted`. Only `Live` is a measurement of
the observation's timestamp; the other two are substitutes attached to it. Check
it before feeding an observation to a policy or writing it to a dataset. `Stale`
and `Omitted` frames keep the *real* frame's timestamp, so the true age is
`observation.timestamp_us - frame.timestamp_us`.

Per-track policies are the point: a wrist camera a policy depends on may warrant
`drop`, since no observation beats a confidently wrong one, while a scene camera
warrants `omit` so its failure does not take the rest of the frame set down with
it.

```yaml
stall_behavior: omit          # default for every track
max_lag_ms: 150
videos:
  - { name: wrist, codec: h264, stall_behavior: drop, max_lag_ms: 40 }
  - { name: scene, codec: h264 }
```

`reuse_stale_frames` is retained as a deprecated alias for `stall_behavior: freeze`
with `max_lag_ms: 0`.

## Design choices not made

**No interpolation.** For each head state, Portal picks the nearest frame per
track rather than interpolating between the two frames that bracket it. Nearest
neighbour is cheaper and matches what most policies expect. Interpolation, or its
mirror of interpolating state to a frame timestamp, would be a further
`stall_behavior` variant rather than a separate knob.

**No wall clock.** `max_lag` is measured in sender-clock time, never wall-clock,
so a given packet sequence always produces the same sync decisions regardless of
machine or scheduling. The consequence is that it is not a watchdog: it is
evaluated when a packet arrives, so a burst of buffered frames can cross the
budget in far less real time than the number suggests, and if *every* stream goes
silent nothing resolves at all. That last case is harmless — with no stream
advancing there is nothing to emit either.

**No coalescing of state callbacks.** Every state packet fires `on_state` even if
the consumer cannot keep up. The observation path is where synced and paced data
lives. `on_state` is a firehose by design.

**Per-sender sync only.** The buffer does not synchronize across multiple remote
participants. Each participant is assumed to be a single sender, and there is one
robot.

## Config derivation

You do not set `SyncConfig` fields directly. They derive from the three
user-facing knobs.

| `SyncConfig` field | Derived from | Default | Effect |
|---|---|---|---|
| `video_buffer_size` | `slack` | 5 | Frames buffered per track. Larger tolerates more jitter and longer stalls, at the cost of staleness. |
| `state_buffer_size` | `slack` | 5 | States buffered awaiting a match. Larger tolerates longer video stalls before eviction. |
| `search_range_us` | `tolerance / fps` | 50 000 | Match window half-width. Wider means fewer drops under jitter and looser alignment. |
| `default_stall.max_lag_us` | `slack / fps` | 166 666 | How far the stream clock may run past a moment before it resolves without a silent track. |
| `default_stall.behavior` | `stall_behavior` | `drop` | How that moment resolves: `drop`, `freeze`, or `omit`. |

Keep `tolerance` at 1 or above so the window covers at least one inter-frame
interval. Tighter than that and ordinary jitter starts producing drops.

Full setters and the asymmetric-rate math are in [Tuning](../04-tuning.md).

## Where the code lives

Paths are relative to the repository root.

| File | What is in it |
|---|---|
| `livekit-portal/src/sync_buffer.rs` | `SyncBuffer`, `SyncOutput`, the match algorithm, all cursor and blocker bookkeeping. |
| `livekit-portal/src/portal.rs` | `ObservationSink`, the `EventContext` handed to the room event loop, Portal lifecycle. |
| `livekit-portal/src/video.rs` | `VideoReceiver`. Converts a `VideoFrame` into `Arc<VideoFrameData>`, then calls `push_frame` and dispatches. |
| `livekit-portal/src/data.rs` | `DataPublisher` and `handle_data_received`, which calls `push_state` and returns the `SyncOutput`. |

Tests for every edge case above are in the `tests` module at the bottom of
`sync_buffer.rs`.

## Reference

- [Tuning](../04-tuning.md). The knobs.
- [Metrics](../07-metrics.md). What the algorithm reports about itself.
- [Concepts](../02-concepts.md#the-observation-model). The short version.
