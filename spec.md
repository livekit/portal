# v0.2: multi-operator and HITL recording

Adds multi-controller support and operator-side action subscription for
HITL recording. Both behaviors are always-on whenever Portal is used,
regardless of which surface (`Robot`, `Operator`, or the unified
`Portal`) the caller picks.

## Motivation

Portal today is 1 robot to 1 operator. Real workloads need more:

- Human-in-the-loop teleop. A policy drives by default, a human takes over
  when needed.
- Ensemble or shadow policies. Multiple policies stream actions, one is
  authoritative, others run for evaluation or recording.
- Supervisor UIs. A non-controlling participant arbitrates who has control.

This delta keeps the existing two-role split (robot, operator) but allows
N operators per room with one designated as active.

## Mental model

> The robot is in charge of who it listens to. It exposes that decision as
> state, and accepts requests to change it.

One pointer on the robot, called `active_operator`, names the operator whose
actions are accepted. Everyone reads it. Anyone can ask the robot to change
it. The robot's attribute is the source of truth.

That is the entire model.

## Wire-level changes

### Identity decoupled from role

Today the role string is the LiveKit participant identity. With N operators
that breaks, since identity must be unique in a room. Going forward:

- **Robot identity.** Singleton, defaults to `"robot"`.
- **Operator identity.** Free-form string supplied via config. Defaults to
  a generated UUID if omitted. Stable values are useful so they can be
  named in `set_active_operator` calls. Examples: `"binh-teleop"`,
  `"policy-v2-shadow"`, `"supervisor-ui"`.

### Role as a participant attribute

Role moves into LiveKit participant attributes. Each Portal participant
publishes:

```
attributes.role = "robot" | "operator"
```

Portal sets this attribute itself on connect. We do not assume the
token-mint script knows about Portal-specific keys. Tokens for Portal
participants **must** include `canUpdateOwnMetadata`, otherwise the
self-set call fails and the participant cannot be discovered.

Token-mint may also set this key directly. If both paths are used, the
token value seeds the initial state and Portal's `setAttributes` is a
no-op for that key.

Discovery is server-synced. No first-message handshake.

### `active_operator` attribute on the robot

The robot publishes one additional attribute:

```
attributes.active_operator = "<operator identity>" | ""
```

Empty string means "drop everything". Can be seeded at token-mint time so
the robot has an active operator pinned before anyone connects.

The robot does **not** auto-clear `active_operator` when the named
participant disconnects. The pointer stays pinned. If that identity
reconnects, control resumes naturally. If it never returns, any other
operator can claim by calling `set_active_operator`.

### Action gate

The robot drops incoming actions where the sender's identity does not match
`active_operator`. Non-active operators can keep streaming. Their packets
are silently ignored at the gate.

This is a one-line filter at the existing dispatch site
(`livekit-portal/src/data.rs`).

### `set_active_operator` RPC

The robot registers one LiveKit RPC method:

```
portal.set_active_operator
  args:   { identity: string | null }
  result: { ok: bool }
```

Anyone in the room can call it. Robot's handler updates its own attribute,
which broadcasts via `ParticipantAttributesChanged`. No built-in gating in
v1. Apps that want to arbitrate wrap the handler.

## Python API

### Class split

The unified `Portal` class is replaced with role-specific classes in
`livekit.portal`. The `Role` enum drops from the public surface.

```python
from livekit.portal import Robot, Operator, RobotConfig, OperatorConfig
```

Both classes wrap the same Rust core. They expose only the methods relevant
to their role.

### Robot

```python
robot = Robot(config=RobotConfig(session_id="..."))
await robot.connect(url, token)

# data plane (unchanged)
robot.on_action(handler)
robot.on_action_chunk(name, handler)
robot.send_state(state)
robot.send_video_frame(track, frame)

# control plane (new)
robot.active_operator()                     # -> Optional[str]
robot.set_active_operator(identity)         # local set, broadcasts
robot.operators()                           # -> list[str]
robot.on_operator_joined(cb)
robot.on_operator_left(cb)
```

### Operator

```python
op = Operator(config=OperatorConfig(
    session_id="...",
    identity="binh-teleop",                 # optional, defaults to a UUID
))
await op.connect(url, token)

# data plane (unchanged)
op.send_action(action)
op.send_action_chunk(name, chunk)
op.on_state(handler)
op.on_observation(handler)
op.on_video_frame(track, handler)

# control plane (new)
op.identity()                               # own identity
op.active_operator()                        # mirrors robot's attribute
op.set_active_operator(identity)            # RPC under the hood
op.operators()                              # other operators in the room
op.on_operator_joined(cb)
op.on_operator_left(cb)
op.on_active_operator_changed(cb)
```

The operator-side `set_active_operator` is fully general. It can claim
self, hand to a peer, or clear with `None`. Same method name as on the
robot, different transport.

### Direct `Portal` usage

`Portal`, `PortalConfig`, and `Role` stay in `livekit.portal` for callers
that want the unified surface (advanced use, the FFI host's typed entry
point). They get the same multi-controller behavior `Robot` / `Operator`
do — there is no opt-in flag. Choosing the unified class only affects
which methods the type system exposes; the wire and runtime behavior are
identical.

## Defaults

- `active_operator` defaults to `None`. Robot drops every action until set.
- When the active operator disconnects, the pointer stays pinned. Reconnect
  with the same identity resumes control. Anyone else who wants control
  must claim explicitly via `set_active_operator`.
- No first-to-join auto-claim in the core. The lerobot wrappers do auto-claim
  on connect for single-op convenience.
- The `set_active_operator` RPC handler always accepts. Apps wrap if they
  want gating.
- Portal self-sets the `role` attribute on connect via `setAttributes`. The
  token must include `canUpdateOwnMetadata`.

## Rust core

Single `Portal` struct. Multi-controller is always-on; there is no
opt-in flag.

### Public surface additions

```rust
// PortalConfig
fn set_action_subscription(&mut self, enable: bool);   // default false
fn action_subscription(&self) -> bool;

// Portal (post-connect)
fn local_identity(&self) -> Option<String>;
fn active_operator(&self) -> Option<String>;
fn set_active_operator(&self, id: Option<String>) -> PortalResult<()>;
fn operators(&self) -> Vec<String>;
fn robot_identity(&self) -> Option<String>;

// Callbacks
fn on_operator_joined(&self, cb: impl Fn(&str) + Send + Sync + 'static);
fn on_operator_left(&self, cb: impl Fn(&str) + Send + Sync + 'static);
fn on_active_operator_changed(&self, cb: impl Fn(Option<&str>) + Send + Sync + 'static);

// Action / ActionChunk records gain `sender`
pub struct Action {
    pub values: HashMap<String, TypedValue>,
    pub raw_values: HashMap<String, f64>,
    pub timestamp_us: u64,
    pub in_reply_to_ts_us: Option<u64>,
    pub sender: String,                   // identity at gate time / echo
}

pub struct ActionChunk {
    // ...existing fields...
    pub sender: String,
}
```

### Internal wiring

- Constants: `ROLE_ATTR_KEY`, `ACTIVE_OPERATOR_ATTR_KEY`,
  `SET_ACTIVE_OPERATOR_RPC` (`portal.set_active_operator`).
- Self-set of `lk.portal.role` attribute on connect (requires
  `canUpdateOwnMetadata` in the token).
- `ParticipantConnected` / `ParticipantDisconnected` /
  `ParticipantAttributesChanged` events update the controller mirror
  via `classify_and_update`.
- `set_active_operator` RPC handler registered automatically on
  `Role::Robot` connect.
- Action gate at the receive site in `portal.rs::handle_room_event`
  (`DataReceived ACTION_TOPIC` and `ByteStreamOpened ACTION_CHUNK_TOPIC`)
  drops when `sender != active_operator`. Sender identity is stamped
  into the delivered record so recorders can label rows without
  consulting any room state.
- `data.rs::handle_data_received` accepts `Role::Operator, ACTION_TOPIC`
  packets too, behind the `action_subscription` flag, for HITL
  recording / shadow eval / live monitoring.
- `Portal::send_action` and `Portal::send_action_chunk`: when
  `action_subscription` is on AND `local_identity == active_operator`,
  call `action.deliver(...)` / the chunk slot's deliver path locally
  after publishing. Closes the LiveKit "no echo to publisher" gap so
  `on_action` is uniform regardless of who produced the action.
- Existing `ActionSlot` / `ChunkSlot` structures are reused as-is —
  they already power `on_action` / `on_action_chunk` / `get_action` /
  `get_action_chunk`. The operator side becomes a second writer.

### What stays unchanged

- All non-multi-controller code paths: transport, codec, sync buffer,
  RPC plumbing, metrics.
- Schema fingerprinting, `WrongRole` errors, dtype validation.

The Python class split (`Robot` / `Operator` / `RobotConfig` /
`OperatorConfig`) lives in the binding layer; the Rust core stays
unified.

## Token requirements

All Portal participants self-set the `lk.portal.role` attribute on
connect, so every token-mint must include `canUpdateOwnMetadata`.
Connecting with a token missing this grant fails at `set_attributes`
time with a clear error.

## lerobot wrappers

`lerobot-robot-livekit` switches its core to `Robot`.
`lerobot-teleoperator-livekit` switches to `Operator` and exposes an
`identity` config field. Both wrappers default to auto-claim on connect
for single-op use cases.

## Operator-side action subscription (HITL recording)

Recording HITL sessions, shadow-evaluating policies, and live-monitoring a
robot all share the same need: the operator wants to see the actions the
robot is actually executing. Today only the robot sees those. v0.2 adds
an opt-in subscription on the operator side, gated by the same
active-operator filter.

### Behavior

When the flag is on, an operator participates as a **passive observer of
executed actions** in addition to whatever else it does:

- Receives `(Role::Operator, ACTION_TOPIC)` packets via the LiveKit fanout,
  applies the same `sender == active_operator` gate the robot applies, and
  fires `on_action`. Same for `(Role::Operator, ACTION_CHUNK_TOPIC)` byte
  streams firing `on_action_chunk`.
- When the operator itself is the active operator, `send_action` and
  `send_action_chunk` also fire the local callback after publishing
  ("echo"). LiveKit does not fan out a publisher's own data packets, so
  without echo the active operator would be the only one in the room who
  cannot see what the robot is executing.
- `get_action()` and `get_action_chunk(name)` return the latest received
  (or echoed) value, mirroring the robot-side pull API.

When the flag is off (default), none of the above happens on the operator
side. Robot behavior is unchanged regardless of the flag.

### Default

Off. Most operators are pure controllers and do not want the bandwidth or
the callback noise. Recorders, shadow-eval policies, and monitoring UIs
opt in.

### Config

```python
cfg = OperatorConfig("session", identity="recorder")
cfg.add_video("front")
cfg.add_state_typed([...])
cfg.add_action_typed([...])     # required to deserialize incoming actions
cfg.set_action_subscription(True)
```

One flag covers both subscription and echo by design. Splitting them
gives a partial view ("sees others' actions but not own when active") that
is rarely what users want. Apps that genuinely need only one half ignore
the half they don't want at the callback site.

### Sender attribution

`on_action` fires for actions the gate accepted, but the active operator
may already have changed by the time the callback runs (handoff is an
attribute write that races with action delivery). Reading
`active_operator()` inside the callback can label the action incorrectly.

Resolution: every `Action` and `ActionChunk` record carries a `sender`
field set at gate time, when the sender identity is known and matches
`active_operator`. The callback labels actions by their actual producer
without consulting any room state.

```
Action {
    values: dict,
    raw_values: dict,
    timestamp_us: int,
    sender: str,                   # gate-time identity (or self on echo)
    in_reply_to_ts_us: Optional[int],
}

ActionChunk {
    name, horizon, data, raw_data,
    timestamp_us, sender, in_reply_to_ts_us,
}
```

The local echo path stamps `sender` with the publisher's own identity,
so an active operator with subscription on sees its own actions on
`on_action` with `sender = self.local_identity()`.

### Recording recipe

The canonical recorder is a separate operator participant:

```python
rec = Operator(OperatorConfig("session", identity="recorder"))
rec.add_video("front")
rec.add_state_typed([...])
rec.add_action_typed([...])
rec.set_action_subscription(True)

log = []
last_obs = None

def on_obs(obs):
    nonlocal last_obs
    last_obs = obs
    log.append({"kind": "obs", "ts_us": obs.timestamp_us, "obs": obs})

def on_action(action):
    log.append({
        "kind": "action",
        "ts_us": action.timestamp_us,
        "in_reply_to": action.in_reply_to_ts_us,
        "sender": action.sender,
        "values": action.values,
    })

rec.on_observation(on_obs)
rec.on_action(on_action)
await rec.connect(URL, recorder_token)
# never call set_active_operator — pure observer.
```

Same shape works for shadow eval (replace logging with a model.compare
call) and live monitoring (push to a UI websocket).

### Operator-as-recorder (HITL self-record)

A single operator that wants to log its own session enables the flag and
relies on echo. `on_action` fires for every executed action, whether
produced by another operator or by self while active. No separate teeing
in user code.

```python
op = Operator(OperatorConfig("session", identity="binh-teleop"))
op.set_action_subscription(True)
op.on_action(lambda a: log.append(a))
# both incoming (other operator) and own sends (when active) reach `log`.
```

### Out-of-scope variations (kept simple)

- **Per-stream subscription flags.** A single combined flag is the v0.2
  shape. If a future use case wants subscription without echo or vice
  versa, add a second flag rather than expanding this one.
- **Filtering by sender at the callback level.** The gate is fixed at
  `sender == active_operator`. Apps that want "fire for all senders"
  (raw shadow capture of every operator's outputs) would need a separate
  unfiltered-firehose path; not in v0.2.

### Python wiring

The Python `Operator` class re-exposes `on_action`, `on_action_chunk`,
`get_action`, `get_action_chunk` (slots already on the underlying
Portal; the wrapper just forwards). `OperatorConfig` exposes
`set_action_subscription(bool)`. Default off — same as the Rust core.

## End-to-end testing plan

Tests below assume a real LiveKit server (Cloud or self-hosted dev). Mocks
are only used for Rust unit tests of the action gate. Each scenario
should land as an integration test in
`python/packages/livekit-portal/tests/`.

### Happy paths

1. **Single operator drives.** Robot starts, operator connects with
   identity `"op1"`, robot or operator sets active to `"op1"`, operator
   sends actions, robot's `on_action` fires. Verifies the basic v0.2
   loop end to end.
2. **Token-mint pre-seeds active operator.** Robot's token includes
   `attributes.active_operator = "op1"`. Operator `"op1"` connects and
   immediately drives without any RPC call.
3. **Two operators, explicit handoff.** Both connect. Operator A claims,
   sends, robot processes. Operator B calls `set_active_operator("opB")`.
   Robot now processes B, drops A. `on_active_operator_changed` fires on
   both.
4. **Supervisor pattern.** Three participants: robot, operator A,
   operator B. A third participant `"super"` connects as operator but
   never sends actions, only calls `set_active_operator`. Verify it can
   arbitrate without driving.
5. **Operator self-claim.** Operator calls
   `op.set_active_operator(op.identity())`. Robot's attribute updates
   correctly.

### Handoff edge cases

6. **Active operator disconnects.** Robot sees `participant_disconnected`.
   `active_operator` stays pinned at the disconnected identity. Other
   operators' actions still get dropped. No `on_active_operator_changed`
   fires.
7. **Disconnected operator reconnects with same identity.** Because the
   pointer stayed pinned, their actions are accepted again immediately
   on reconnect with no re-claim needed.
7b. **Different operator claims after a disconnect.** A is active, A
   disconnects, A's pointer stays pinned. B calls
   `set_active_operator("B")`. Pointer moves to B. Subsequent A
   reconnect does not steal control back, A would have to claim again.
8. **Set to non-existent identity.** Robot accepts the call,
   `active_operator` becomes `"ghost"`, all actions from real operators
   are dropped.
9. **Set to `None` mid-drive.** Robot stops processing actions
   immediately. In-flight chunks already delivered by LiveKit's byte
   stream are received as normal (delivery is reliable). What the
   robot's app does with a delivered chunk after the gate clears is
   app-level, not Portal's concern.
10. **Active operator set before any operator joins.** Robot starts with
    seeded `active_operator`. Operators all silently dropped until the
    matching identity connects.
11. **Concurrent claims.** A and B both call `set_active_operator` at the
    same time. LiveKit serializes attribute writes through the server.
    Final state is deterministic. Both clients see the same
    `on_active_operator_changed` value.
12. **Rapid thrash.** `A → B → C → A` within 50 ms. Each transition fires
    one `on_active_operator_changed`. No actions from the wrong operator
    sneak through during transitions.

### Action chunk interaction

Chunks are LiveKit byte streams. Once a chunk starts being sent by an
operator, LiveKit reliably delivers it. Portal does not interrupt or
truncate in-flight chunks based on `active_operator` changes. The gate
applies at the receive site: if `sender != active_operator` at delivery
time, the chunk is dropped. Anything past delivery is app-level.

13. **A's chunk is mid-stream during handoff.** Operator A starts sending
    a 30-step chunk. Mid-stream, B becomes active. A's chunk is fully
    received by LiveKit (byte stream guarantee). The gate on Portal's
    receive side checks the sender at the moment of full delivery: if A
    is no longer active, the chunk is dropped before reaching
    `on_action_chunk`.
14. **Multiple senders' chunks during overlap.** Both A and B send chunks
    around the same handoff window. Robot only fires `on_action_chunk`
    for chunks from the active operator at delivery time. Others are
    dropped silently.

### Lifecycle

15. **Robot disconnects.** All operators see `participant_disconnected`
    for the robot. New `set_active_operator` calls fail cleanly with a
    "no robot in room" error, not a hang.
16. **Operator re-connect during ongoing session.** Late joiner reads
    current `active_operator` from attribute sync, no stale state.
17. **Same operator identity twice.** LiveKit rejects the second
    connection. Verify the error surfaces as a clean exception, not a
    dangling handle.

### Token and permission

Portal sets the `role` attribute on connect via `setAttributes`. We do
not assume the token-mint script knows about Portal-specific keys. The
token grant **must** include `canUpdateOwnMetadata`.

18. **Token has `canUpdateOwnMetadata`, role not in token.** Portal calls
    `setAttributes({role: ...})` after connect. Other participants see
    the role within one round trip. This is the default path.
19. **Token has `canUpdateOwnMetadata` and role in token.** Role is
    available immediately at connect. Portal's `setAttributes` is a
    no-op for that key.
20. **Token without `canUpdateOwnMetadata`.** Portal's `setAttributes`
    fails. Surface a clear error at connect time, do not silently leave
    the participant unidentified. Same applies if the token also
    omits the role attribute.
21. **Operator without identity in config.** UUID is generated. Identity
    is non-empty and unique across the run.

### RPC

22. **`set_active_operator` RPC before robot is in room.** Operator
    connects first, robot has not joined yet. Calling the RPC fails with
    a clean error.
23. **RPC during high-frequency action stream.** Operator A is sending
    actions at 100 Hz. B calls `set_active_operator("B")`. No actions
    are reordered, no actions are lost on the wire (some are dropped at
    the robot's gate, which is correct).
24. **Bad arg type to RPC.** Passing a non-string identity returns a
    validation error. Robot's attribute is unchanged.

### Observation correctness under multi-operator load

25. **Non-active operator's stream does not affect robot's action sync.**
    With 5 operators streaming and 1 active, robot's `on_action` rate
    matches the active operator's send rate. CPU and bandwidth on the
    robot grow linearly with operator count (acceptable for now).
26. **`on_drop` semantics unchanged.** Multi-operator activity does not
    change observation drop behavior on the operator side. Each operator
    still sees its own observation stream from the robot.

### Active operator propagation

These tests measure that the `lk.portal.active_operator` attribute
reaches every participant within a bounded window after a write, and
that the action gate engages on the new value as soon as the mirror
updates.

27. **Robot-side write reaches every operator within 500 ms.** Three
    operators connected. Robot calls `set_active_operator("X")`. All
    three operators see `op.active_operator() == "X"` within 500 ms
    (LAN test); each operator's `on_active_operator_changed` fires
    exactly once with `"X"`.
28. **Operator-side write reaches every operator and the robot within
    500 ms.** Operator A calls `set_active_operator("B")`. The robot's
    own `active_operator()` returns `"B"`, all other operators' mirrors
    return `"B"`, all `on_active_operator_changed` callbacks fire
    exactly once. Pinning the bound at 500 ms covers the extra RPC hop.
29. **Action acceptance follows the mirror.** Robot calls
    `set_active_operator("policy")`. Within 500 ms, actions sent by
    `"policy"` reach the robot's `on_action` and actions sent by other
    operators do not. After a second `set_active_operator("human")`,
    `"human"` is accepted and `"policy"` is dropped.
30. **Idempotent write does not fire the change callback.** Robot calls
    `set_active_operator("X")` twice with the same value. The second
    call does not fire `on_active_operator_changed`.
31. **Late joiner reads the current value at connect.** Operator C
    connects after Robot already wrote `set_active_operator("policy")`.
    `c.active_operator()` returns `"policy"` immediately (no
    `_wait_for` polling).
32. **Cross-write race converges.** Operator A calls
    `set_active_operator("A")` and Operator B calls
    `set_active_operator("B")` within 50 ms of each other. Both writes
    succeed (both RPCs return `Ok`). Final state is deterministic and
    visible to everyone (LiveKit serializes attribute writes through
    the server). All `on_active_operator_changed` callbacks see the
    same final value.

### Operator-side action subscription

These tests cover the v0.2 HITL recording feature: operators with
`set_action_subscription(True)` receive the active operator's actions
via the same gate the robot uses, the active operator gets local echo,
and `Action.sender` / `ActionChunk.sender` is set at gate time.

33. **Default is off.** Operator without `set_action_subscription(True)`
    never fires `on_action` even when actions are flowing through the
    room. Verifies we don't accidentally subscribe everyone.
34. **Recorder receives the active operator's actions.** Recorder
    operator (subscription on) plus an active operator. Active sends
    actions; recorder's `on_action` fires for each, with
    `action.sender == active_operator_identity`.
35. **Non-active operators are dropped at the recorder.** Two
    operators streaming, only one active. Recorder's `on_action` fires
    only for the active one's actions; the non-active one's are
    silently dropped at the recorder (matching the robot's gate).
36. **Self-echo when active.** Active operator with subscription on
    calls `send_action`. Its own `on_action` fires locally with
    `action.sender == self.local_identity()`. The mirror in
    `get_action()` reflects the latest sent action.
37. **No echo when inactive.** Operator with subscription on but not
    active calls `send_action` (the gate at the robot will drop it).
    Local `on_action` does not fire — echo only triggers when self ==
    active. The action goes nowhere.
38. **Recorder sees handoff in action stream.** Recorder + two
    operators A and B. A is active; recorder receives A's actions with
    `sender == "A"`. Handoff to B; recorder now receives B's actions
    with `sender == "B"`. The `sender` field flips on the boundary,
    no race between handoff event and label.
39. **`sender` is set on every delivered action.** Every record fired
    on `on_action` has `sender` populated and equal to the operator
    who produced the action. Verifies the gate-time stamping path.
40. **Action chunks subscription works.** Recorder subscribes; active
    operator sends a chunk; recorder's `on_action_chunk` fires with
    `chunk.sender == active_operator_identity`.
41. **Pull surface populates on operator side.** Recorder calls
    `get_action()` after an action arrives; returns the latest action.
    `get_action_chunk(name)` returns the latest chunk. Both reflect
    the gate-passed values, not raw firehose.
42. **Subscription does not affect non-subscribers.** Three operators
    in the room; only one has `set_action_subscription(True)`. The
    other two never fire `on_action`. Verifies the flag is per-operator
    and does not leak.
43. **Subscription does not affect the robot.** Robot's `on_action`
    rate is unchanged whether 0, 1, or N operators have subscription
    on. Recorders are pure observers.

## Out of scope (deferred)

- Action blending across senders during handoff. Today's replace semantics
  give flush-on-handoff for free.
- Confidence-based auto-handoff. Policy publishes confidence, robot decides.
  App-level concern.
- `on_set_active_operator_request` gate for app-level arbitration. Default
  accept is fine for v1.
- Shadow-action callback for capturing dropped streams.
- Multiple robots per room.
