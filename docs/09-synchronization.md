# Synchronization

This document explains how `livekit-portal` fuses independently-streamed
video frames and state packets into coherent `Observation`s on the
operator side. It's written for engineers who need to reason about the
sync algorithm, tune its parameters, or extend it.

## The problem

A policy model expects a single tensor per step: _"at time T, here are
my camera frames and joint readings."_ LiveKit does not deliver data
that way. Each video track and each data stream is transported
independently with its own pacing, codec path, and retransmission
behaviour. The receiver sees them as four (or more) separate event
streams arriving out-of-phase.

The job of `SyncBuffer` is to take those uncoordinated streams and emit
an `Observation { state, frames: {track → frame}, timestamp_us }` in
which every component is "close enough" in sender time.

## Nature of a LiveKit stream

Understanding what guarantees LiveKit *does* and *doesn't* give you is
essential — the algorithm's shape is dictated by them.

- **Monotonic timestamps, not monotonic arrival.** The sender tags each
  frame and state packet with its own `timestamp_us` (we use LiveKit's
  [packet trailer](https://docs.livekit.io/home/cloud/data-messages/) for
  video, a little-endian `u64` prefix for data packets). Timestamps are
  produced by a single clock on the sender so they're globally ordered
  within a sender. Arrival order on the receiver **is not** guaranteed
  to match send order:
    - **Reliable data** (default): preserves order per-sender via SCTP.
    - **Unreliable data**: may reorder or drop.
    - **Video**: RTP packets are reassembled in order by the jitter
      buffer, but a frame can still surface late if a keyframe was
      recovered. Frame rate is noisy (NACK/FEC latency, codec buffering).
- **Different latencies per stream.** Video traverses an encoder, a
  congestion-controlled transport, and a decoder. Data packets don't.
  At typical network conditions video runs ~30–80 ms behind a data
  packet stamped at the same moment.
- **Stalls are common.** A camera can stutter; a reliable data stream
  can pause for seconds during reconnection.
- **We don't get a global clock.** Only the sender's monotonic clock.
  Receiver wall-clock is irrelevant to match quality — we match on
  sender timestamps.

Given all this, sync has to be **tolerant** (a search window, buffering
on both sides) and **latency-aware** (don't wait forever; if a stream
moves past a state's window, drop the state).

## Data model

Each operator session holds a `SyncBuffer` with:

- One `VecDeque<Arc<VideoFrameData>>` per registered video track,
  bounded by `video_buffer_size`.
- One `VecDeque<(u64, Vec<f64>)>` for incoming state packets, bounded
  by `state_buffer_size`.
- Per-track `cursors: Vec<usize>` — see [Two-pointer cursors](#1-two-pointer-cursors).
- A `blocker: Option<usize>` hint — see [Blocker-gated sync](#2-blocker-gated-sync).

`video_buffer_size` and `state_buffer_size` are not set directly. Both
come from the user-facing `slack` knob (default 5). `search_range_us`
comes from `tolerance / fps` (50 ms at the defaults of fps 30, tolerance
1.5). See [tuning](06-tuning.md) for the knobs and the math.

Both deques are time-sorted *in the common case* (monotonic sender
clock) and we lean on that fact throughout.

## The matching rule

For a given state with timestamp `S`, a frame at timestamp `F` on track
*k* is a **candidate match** iff `|S − F| < search_range_us` (50 000 µs =
50 ms at the defaults). Among candidates, we pick the **nearest**.

A state produces an `Observation` only when **every** registered track
has a candidate. If any track has no candidate in range, one of three
things happens:

| Track state relative to head state `S` and range `R` | Decision |
|------------------------------------------------------|----------|
| Newest frame has `ts >= S + R` | **Drop** the state — future frames are monotonic (`ts ≥ back`), so no match is ever possible. |
| No in-range frame *yet*, and `back < S + R` (newest still below the horizon, or buffer empty) | **Wait** — newer frames may still arrive in range. |
| At least one in-range frame exists (`|ts − S| < R`) | **Match** that track, check the others. |

The drop rule looks at the *newest* frame, not the oldest. Checking the
front instead would miss the case where an old frame sits below the
match window while the rest of the stream has already moved past — the
state stalls until eviction drags the old frame out. The `>=` matches
the strict `<` in the match rule: a frame landing exactly at `S + R`
is not a match and all future frames are ≥ that timestamp, so drop.

These three outcomes (match / wait / drop) are the state machine the
algorithm executes per head state on every push.

### Optional: stale-frame reuse

Enable `reuse_stale_frames` on `PortalConfig` to flip the drop outcome
into a **reuse** outcome once a track has emitted at least once. The
state falls back to that track's last-emitted frame, so the observation
still fires — video "freezes" on the last good frame during loss while
state keeps flowing. An empty buffer with a last-emitted frame emits
immediately without waiting for the next fresh frame.

| Track state, with `reuse_stale_frames = true` and `last_emitted.is_some()` | Decision |
|---------------------------------------------------------------------------|----------|
| Newest frame has `ts >= S + R` | **Reuse** last-emitted frame. Buffer is untouched — a later state can still consume the fresh frame. |
| Buffer empty | **Reuse** last-emitted frame immediately. |
| Fresh in-range match exists | **Match** (fresh match wins, `last_emitted` advances). |

During the startup window before the first emission, `last_emitted` is
`None` and there's no fallback — the strict drop rule still applies so
the state buffer stays bounded if video never arrives at all. Once the
first observation fires, reuse takes over and drops cease.

Use this when data-collection or logging pipelines prefer a transient
video freeze over a missing state. Leave it off for real-time control
where a stale frame would misalign the perception/action loop.

## Naïve algorithm (what we *don't* do)

The straightforward implementation does this on every push:

```text
for each head state S in buffer:
    for each track k:
        scan every frame in track_k.buffer for the closest to S
    aggregate results → match/wait/drop
```

Complexity: O(states × tracks × frames_per_track) per push. At 30 fps ×
3 cameras + 100 Hz state publishing, that's about 190 pushes/sec ×
(30 × 3) comparisons each. Small in absolute terms, but it scales
poorly with buffer size and there is a lot of redundant work: the same
frames are re-scanned for the same state every time *anything* pushes.

The real cost isn't the comparisons — it's the cache traffic and lock
contention that follows.

## The optimized algorithm

Four ideas, each addressing a specific waste.

### 1. Two-pointer cursors

Both buffers are monotonic streams. For a fixed head state `S`, the
best matching frame index on each track only moves **forward** as `S`
advances to the next head (since state timestamps are monotonic too).

We maintain `cursors[track_i]` = the largest index whose frame
timestamp is ≤ `S` (or 0 if all frames are newer). On each sync:

1. Advance: while `buf[cursor + 1].ts ≤ S`, increment cursor.
2. Compare cursor and cursor+1 against `S` to pick the closer one.

Across the whole stream, each frame is inspected a constant number of
times, giving amortized **O(N + M)** total work instead of O(N × M)
per call.

**Cursor rewind.** For unreliable data, states can arrive out of order.
If a new head state has an earlier timestamp than the last, we rewind
(`while buf[cursor].ts > S && cursor > 0: cursor -= 1`) before
advancing. For reliable transport (the default) the rewind never
triggers; it's insurance.

**Cursor adjustments on mutation:**

- On eviction (`pop_front` when buffer exceeds `video_buffer_size`):
  decrement cursor by the number of evictions, saturating at 0.
- On successful match with chosen index `idx`, we drain `0..=idx` from
  the buffer; the cursor becomes `saturating_sub(idx + 1)` (i.e. 0).
- On `clear()`, all cursors reset to 0.

### 2. Blocker-gated sync

Key observation: if the last `try_sync` stopped in a **wait** state on
track *k*, then a push to track *j* ≠ *k* can't *unblock* the head —
the head is waiting on *k*, not *j*. So we can skip the whole sync
attempt.

`SyncBuffer` records which track last caused a wait in
`blocker: Option<usize>`. Rules for running `try_sync`:

| Trigger | Blocker state | Run try_sync? |
|---------|---------------|---------------|
| `push_state` | any | **Yes** — a new head may be ready to match. |
| `push_frame` to blocker track | `Some(self_track)` | **Yes**. |
| `push_frame` to non-blocker, **no eviction** | `Some(other_track)` | **No** — skip. |
| `push_frame` to non-blocker, **eviction happened** | any | **Yes** — see below. |
| Any push | `None` | **Yes** — no hint available. |

`blocker` is updated at the end of every `try_sync`:
- Cleared to `None` on success (observation emitted) or when the state
  buffer is drained.
- Set to the first waiting track after a wait.

**Why the eviction escape hatch matters.** A non-blocker push usually
just appends a new frame — irrelevant to the head state. But if the
track is at capacity, `pop_front` evicts the oldest frame, and if that
frame was the one in-range for the head state, the track just
transitioned from "matching" to "unmatchable." Skipping sync would
silently stall the state. Running sync on eviction catches this and
either re-matches or drops.

At 30 fps × 3 cameras + 100 Hz states, ~80% of frame pushes become
no-ops under this rule once steady-state is reached.

### 3. O(1) drop detection

`try_sync` previously answered "are all frames newer than `S + R`?"
with `buf.iter().all(...)`. The correct question is stronger: **can a
future frame ever match?** Under monotonic delivery, every future
frame has `ts ≥ buf.back().ts`, so the state is permanently
unmatchable iff:

```text
buf.back().ts >= S + R
```

O(frames) → O(1). This also closes an asymmetry: the match rule is
strict (`d < range`), so a frame at exactly `S + R` is not a match,
and neither is any future frame — `>=` makes drop symmetric with
match. Checking the *front* instead (an earlier version) would only
flag the drop after eviction had dragged the old tail through the
horizon, with a latency of up to `video_buffer_size` frames.

### 4. Eager drop across tracks

The algorithm iterates tracks in order, classifying each as
`match`/`wait`/`drop` locally. Drop takes precedence over wait: if any
track reports `drop`, we drop the state, even if an earlier track in
iteration order said `wait`. Without this, an empty `cam1` (wait) would
shield a runaway `cam2` (oldest frame beyond `S + R`), stalling the
state forever until `cam1` eventually produced a frame — at which point
we'd drop anyway. Doing the cross-track drop check up front cuts
latency on disconnects and stream stalls.

## Full `try_sync` (annotated)

```text
loop:
    if state_buffer empty:
        blocker = None
        return output

    S = state_buffer.front().ts
    iter_blocker = None
    should_drop = false

    for track_i in 0..tracks:
        buf = video_buffers[track_i]
        if buf.empty():
            iter_blocker ??= track_i           # remember first waiter
            continue                            # keep scanning other tracks

        advance_cursor(track_i, S)              # with optional rewind

        pick closest of buf[cursor], buf[cursor+1] within range
        if found:
            matched_scratch[track_i] = (idx, frame)
            continue

        # unmatched
        if buf.back().ts >= S + range:          # no future frame can ever match
            should_drop = true
            break                               # drop wins over wait
        iter_blocker ??= track_i

    if should_drop:
        emit drop(state)
        pop state
        continue loop

    if iter_blocker.some():
        blocker = iter_blocker
        return output                           # wait

    # all tracks matched
    pop state
    for each track_i with a match:
        drain buf[0..=idx]
        cursors[track_i] = cursors[track_i].saturating_sub(idx + 1)
    emit observation
    # continue loop: maybe more states can now match
```

The outer `loop` drains as many backlogged states as possible in a
single call — important because state packets arrive faster than
observations and can pile up during a video stall.

## Dispatch — decoupled from the match loop

`try_sync` never calls user code. It returns a `SyncOutput`:

```rust
pub(crate) struct SyncOutput {
    pub observations: Vec<Observation>,
    pub drops: Vec<HashMap<String, f64>>,
}
```

The caller (e.g. the video receiver task or the data-received handler)
releases the `SyncBuffer` mutex and *then* hands the output to
`ObservationSink::dispatch`, which fires callbacks and updates a
latest-wins slot. This matters because:

- User callbacks cross FFI (Python, Swift, Kotlin) and can block for
  milliseconds. Running them under the sync lock would stall every
  frame receiver and the room event loop.
- The observation callback receives `&Observation` — no clone for
  callback-only consumers. The pull-based `get_observation()` reads the
  latest-wins slot, so a slow puller sees the freshest observation
  rather than a backlog.

## Complexity and guarantees

- **Amortized work:** O(N + M) where N = total frames received per
  track, M = total states received.
- **Per-push work:** O(1) amortized; worst-case O(tracks × rewind
  depth). Rewind is bounded in practice by how far out-of-order the
  sender's clock can go (typically ≤ 1 state).
- **Emit latency:** bounded by `search_range_us` + the slowest track's
  frame inter-arrival time. A state with timestamp `S` is either
  emitted once every track has a frame in `[S − R, S + R]`, or dropped
  once any track's oldest frame exceeds `S + R`.
- **Memory:** bounded by `video_buffer_size × tracks + state_buffer_size`
  frames/states, plus one latest-wins observation slot.

## Known limitations and design choices we *didn't* make

- **No deadline-based drop.** A state currently waits indefinitely if
  an older frame is present in the buffer (because a newer, matching
  frame *could* still arrive). If the video track stalls permanently,
  the state blocks the head forever or until its own capacity-driven
  eviction kicks in. A wall-clock deadline (`drop if S hasn't matched
  within N ms of arrival`) would be more aggressive. Not implemented
  because the existing eviction + drop rules cover most failure modes
  and a deadline adds a time source dependency.
- **No state interpolation.** For each head state `S` we pick the
  nearest frame per track, rather than interpolating between the two
  frames that bracket `S`. Nearest neighbour is cheaper and matches the
  semantics most policies expect; interpolation (or its mirror —
  interpolating state to a frame timestamp) would be a separate opt-in
  mode.
- **No coalescing of state callbacks.** Every state packet still fires
  `on_state` even if the consumer can't keep up. The observation path
  is already the place to get "synced + paced" data; `on_state` is a
  raw firehose by design.
- **Per-sender sync only.** The sync buffer doesn't attempt to
  synchronize across multiple remote participants. Each participant is
  expected to be a single sender (one robot).

## Tuning

You do not set these `SyncConfig` fields directly. They derive from three
user-facing knobs (`fps`, `slack`, `tolerance`) whose defaults target
30 fps video + ≤100 Hz state at typical WAN latencies (~50 ms RTT).
Full setters, asymmetric-rate math, and reliability options live in
[tuning](06-tuning.md).

| `SyncConfig` field | Derived from | Default | Effect |
|--------------------|--------------|---------|--------|
| `video_buffer_size` | `slack` | 5 | Frames buffered per track. Larger = more jitter and stall tolerance, more staleness. |
| `state_buffer_size` | `slack` | 5 | States buffered awaiting a match. Larger = longer video stalls tolerated before eviction. |
| `search_range_us` | `tolerance / fps` | 50 000 (50 ms) | Match window half-width. Wider = fewer drops under jitter, looser alignment. |

A useful rule of thumb: keep `tolerance ≥ 1`, so `search_range_us` covers
at least one inter-frame interval (`1e6 / fps`). Tighter than that and a
small amount of jitter will start producing drops.

## Where the code lives

- `src/sync_buffer.rs` — `SyncBuffer`, `SyncOutput`, and the match
  algorithm. All cursor/blocker bookkeeping lives here.
- `src/portal.rs` — `ObservationSink` (callback + pull buffer), the
  `EventContext` passed to the room event loop, Portal lifecycle.
- `src/video.rs` — `VideoReceiver` task; converts `VideoFrame` →
  `Arc<VideoFrameData>` and calls `push_frame` then `dispatch`.
- `src/data.rs` — `DataPublisher` (channel-backed) and
  `handle_data_received` (calls `push_state` then returns `SyncOutput`).

Tests covering the edge cases discussed above are in the `tests`
module at the bottom of `src/sync_buffer.rs`.
