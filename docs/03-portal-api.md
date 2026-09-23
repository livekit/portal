# Portal API

> The main surface. Build a config, construct a `Robot` or an `Operator`,
> register callbacks, push data.

This is the API everything else in the repository is built on, including the
optional lerobot plugins. It works with any robotics stack, because it has no
opinion about where your frames or your actions come from.

The shape is always the same. Build a config. Construct the role. Register
callbacks. Connect. Push data.

## Install

```bash
pip install livekit-portal      # or: uv add livekit-portal
```

Prebuilt wheels cover CPython 3.12 on Linux x86\_64 and aarch64 (glibc 2.35 and
newer) and macOS Apple Silicon. On any other
platform or Python version, [build from
source](01-quickstart.md#build-from-source).

### Rust

The core crate works directly, with no Python involved:

```toml
[dependencies]
livekit-portal = { path = "path/to/portal/livekit-portal" }
```

The Python bindings ship as the `livekit-portal-ffi` crate (UniFFI over a C
ABI) plus a pure-Python package under `python/packages/livekit-portal/`.

## Declaring the schema

Both sides must declare the same schema before connecting. Field names, field
order, and per-field dtypes all participate.

```python
from livekit.portal import DType, RobotConfig

cfg = RobotConfig("session-1")

cfg.add_video("camera1")
cfg.add_video("wrist")

cfg.add_state_typed([
    ("joint1", DType.F32),
    ("joint2", DType.F32),
    ("gripper", DType.BOOL),
    ("mode", DType.I8),
])
cfg.add_action_typed([
    ("joint1", DType.F32),
    ("joint2", DType.F32),
    ("gripper", DType.BOOL),
    ("mode", DType.I8),
])

cfg.set_fps(30)
```

The string passed to `RobotConfig` is the **session name**. It is a local label
used in logs. It is not compared against the LiveKit room name, which comes
from your token.

### Choosing dtypes

Each field declares a dtype that fixes its width on the wire.

| dtype | Bytes | Use it for |
|---|---|---|
| `F64` | 8 | The lossless default. |
| `F32` | 4 | Joint angles. Halves the bytes per field, and the precision is almost always fine. |
| `I32`, `I16`, `I8` | 4, 2, 1 | Signed indices, modes, counters. |
| `U32`, `U16`, `U8` | 4, 2, 1 | Unsigned indices and counters. |
| `Bool` | 1 | Gripper open, estop, any binary signal. |

Values you pass to `send_state` and `send_action` stay ordinary Python values.
Conversion happens at the wire boundary.

> **Note.** Field order is part of the contract, not just the names. Reordering
> two fields on one side changes the schema fingerprint and all traffic drops.
> A [shared YAML file](reference/config-file.md) is the reliable way to keep
> both sides in step.

### Full config surface

Both `RobotConfig` and `OperatorConfig` accept all of these. Anything
role-specific is a no-op on the wrong side.

| Method | Default | What it does |
|---|---|---|
| `add_video(name, codec=..., quality=..., max_bitrate_kbps=..., simulcast=..., screencast=...)` | H264 | Declare a camera track. See [Frame video](05-frame-video.md). |
| `add_state_typed([(name, dtype), ...])` | none | Declare the state schema. |
| `add_action_typed([(name, dtype), ...])` | none | Declare the action schema. |
| `set_fps(int)` | 30 | Capture rate. Drives the match window. |
| `set_slack(int)` | 5 | Ticks of buffer headroom. |
| `set_tolerance(float)` | 1.5 | Match window, in ticks. |
| `set_state_reliable(bool)` | `True` | Reliable delivery for state. |
| `set_action_reliable(bool)` | `True` | Reliable delivery for actions. |
| `set_stall_behavior(StallBehavior)` | `DROP` | What to do about a moment a silent track cannot cover: `DROP`, `FREEZE`, or `OMIT`. |
| `set_max_lag_ms(int)` | `slack / fps` | How long to wait for a silent track first, in sender-clock ms. |
| `set_track_stall_behavior(str, StallBehavior)` | — | Per-track override of `set_stall_behavior`. In Python, `add_video(..., stall_behavior=...)` is the shorter equivalent. |
| `set_track_max_lag_ms(str, int)` | — | Per-track override of `set_max_lag_ms`. |
| `set_observation_sync(bool)` | `True` | Operator-only. Bundle state and frames into observations. Off skips the sync buffer entirely. |
| `set_time_sync_source(TimeSyncSource)` | `PORTAL` | Where `now_us()` comes from: `PORTAL` syncs to the robot, `SYSTEM` trusts the host clock. |
| `set_reuse_stale_frames(bool)` | `False` | Deprecated. Alias for `set_stall_behavior(FREEZE)` with `set_max_lag_ms(0)`. |
| `set_action_subscription(str)` | `"none"` | Operator-only. Which received actions reach `on_action`: `"none"`, `"active"` or `"all"`. |
| `set_e2ee_key(bytes)` | none | Shared-key encryption. See [E2EE](reference/e2ee.md). |

`set_fps`, `set_slack`, and `set_tolerance` are covered in
[Tuning](04-tuning.md).

Every field is also readable back as a property, which is how you inspect a
config you loaded from YAML instead of building by hand:

| Property | Type | Reads back |
|---|---|---|
| `session` | `str` | Session name. |
| `role` | `Role` | Pinned role. |
| `video_tracks` | `list[str]` | Names of every declared track, in declaration order, whatever the codec. |
| `video_track_specs` | `list[VideoTrackSpec]` | Those same tracks with codec, quality, and encoder options. |
| `frame_video_tracks` | `list[VideoTrackSpec]` | The byte-stream subset of `video_track_specs`. A filter, not a rival list. |
| `state_schema` / `action_schema` | `list[FieldSpec]` | Declared schemas, in order. |
| `fps` | `int` | `set_fps`. |
| `slack` | `int` | `set_slack`. |
| `tolerance` | `float` | `set_tolerance`. |
| `state_reliable` / `action_reliable` | `bool` | The reliability flags. |
| `reuse_stale_frames` | `bool` | `set_reuse_stale_frames`. Deprecated. |
| `action_subscription` | `ActionSubscription` | `set_action_subscription`. |
| `has_e2ee_key` | `bool` | Whether a key was set. The bytes are not readable back. |

```python
cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
fps = cfg.fps                    # whatever the file declared
period = 1.0 / cfg.fps
```

You can also build the whole config from a file with
`RobotConfig.from_yaml_file("portal.yaml", "session-1")`. See
[Config from YAML](reference/config-file.md).

## Robot side

```python
import asyncio
import time

from livekit.portal import DType, Robot, RobotConfig

SCHEMA = [("joint1", DType.F32), ("joint2", DType.F32), ("gripper", DType.BOOL)]


async def main(url: str, token: str, hardware) -> None:
    cfg = RobotConfig("session-1")
    cfg.add_video("camera1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    cfg.set_fps(30)

    robot = Robot(cfg)

    def on_action(action) -> None:
        # action.values is the typed dict.
        # action.timestamp_us is the operator's clock.
        # action.sender is the operator identity, stamped at the gate.
        # Only the active operator reaches here.
        hardware.apply(action.values)

    robot.on_action(on_action)
    await robot.connect(url, token)

    try:
        while hardware.running:
            reading = hardware.read()
            ts = int(time.time() * 1_000_000)
            robot.send_video_frame("camera1", reading.rgb, timestamp_us=ts)
            robot.send_state(reading.joints, timestamp_us=ts)
            await asyncio.sleep(1 / 30)
    finally:
        await robot.disconnect()
        robot.close()
```

Stamp the frame and the state from the **same** `ts`. That shared timestamp is
what lets the operator pair them.

If you omit `timestamp_us`, Portal stamps with the current time at the moment
of the call. That is fine when the two calls are adjacent, and wrong if you
capture, do 40 ms of processing, then send.

`send_video_frame` infers width and height from the NumPy array. Pass them
explicitly only when handing over raw bytes.

## Operator side

```python
import asyncio

from livekit.portal import DType, Operator, OperatorConfig, frame_bytes_to_numpy_rgb

SCHEMA = [("joint1", DType.F32), ("joint2", DType.F32), ("gripper", DType.BOOL)]


async def main(url: str, token: str, policy) -> None:
    cfg = OperatorConfig("session-1")
    cfg.add_video("camera1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    cfg.set_fps(30)

    op = Operator(cfg)

    def on_observation(obs) -> None:
        frame = obs.frames["camera1"]
        rgb = frame_bytes_to_numpy_rgb(bytes(frame.data), frame.width, frame.height)
        action = policy(rgb, obs.state)
        op.send_action(action, in_reply_to_ts_us=obs.timestamp_us)

    op.on_observation(on_observation)
    await op.connect(url, token)

    # Without this the robot drops every action we send.
    await op.set_active_operator(op.local_identity())

    try:
        await asyncio.Event().wait()
    finally:
        await op.disconnect()
        op.close()
```

Passing `in_reply_to_ts_us` is optional but cheap, and it is what turns
`metrics.policy.e2e_us_p50` into a real observation-to-action latency instead
of a network ping. See [Metrics](07-metrics.md#policy).

## Lifecycle

```python
await portal.connect(url, token)   # join the room
await portal.disconnect()          # leave it
portal.close()                     # release the native handle
```

Call `close()` when you are finished with the object. It frees the underlying
Rust handle. Skipping it leaks until the process exits, which is harmless for
a script and not for a long-running service that builds portals repeatedly.

`connect` raises `PortalError.AlreadyConnected` if you call it twice.

## Sending data

**Robot only.**

```python
robot.send_video_frame(track, frame, width=None, height=None, timestamp_us=None)
robot.send_state(values, timestamp_us=None)
```

**Operator only.**

```python
op.send_action(values, timestamp_us=None, in_reply_to_ts_us=None)
```

`timestamp_us` defaults to `now_us()`, which is on the robot's clock (see
[Time sync](#time-sync)). Pass your own to stamp a sample where it was taken,
for example a camera's capture time. `in_reply_to_ts_us` is never touched:
pass the robot's exact state or frame timestamp.

## Receiving data

Every callback has a matching pull accessor. Use callbacks for push-driven
loops and the accessors when your own loop sets the pace. Accessors are
latest-wins, so a slow reader sees the freshest value rather than a backlog.

**Operator.**

| Callback | Pull | Fires with |
|---|---|---|
| `op.on_observation(cb)` | `op.get_observation()` | `Observation` |
| `op.on_state(cb)` | `op.get_state()` | `State`, every packet, unmatched |
| `op.on_video_frame(track, cb)` | `op.get_video_frame(track)` | `VideoFrameData`, unmatched |
| `op.on_drop(cb)` | none | `List[Dict[str, ...]]` |

**Robot.**

| Callback | Pull | Fires with |
|---|---|---|
| `robot.on_action(cb)` | `robot.get_action()` | `Action` |

`on_state` and `on_video_frame` are raw firehoses. They fire on arrival with no
matching at all. Use them for a preview pane or a debug log. Use
`on_observation` for anything that needs frames and state to agree.

**Bundling is optional.** Only a peer that consumes bundles live needs it, in
practice a policy. A teleoperator flies on the newest frame, and a recorder
stores raw streams and aligns them offline. Turn it off on those:

```python
cfg.set_observation_sync(False)   # default: True
```

| Peer | Needs bundling? | Why |
|---|---|---|
| Policy | yes | It was trained on matched rows and has to be fed matched rows. |
| Teleoperator | no | It flies on the newest frame and anchors actions with `in_reply_to_ts_us`. |
| Recorder | no | It records raw streams, which are aligned offline. |

Off means the sync buffer never runs. `on_observation` and `get_observation`
raise `ObservationSyncDisabled`, `on_drop` never fires, and `metrics().sync`
stays empty. `slack`, `tolerance`, `stall_behavior` and `max_lag_ms` only apply
while it is on, and Portal warns once if they are set while it is off. It is a
local preference, not part of the wire contract, so peers with and without it
share a room.

`on_drop` receives a **list** of state dicts, not a single state:

```python
def on_drop(dropped):
    # dropped is List[Dict[str, bool | int | float]], no timestamps
    metrics_counter += len(dropped)

op.on_drop(on_drop)
```

### Typed values on receive

`Action`, `State`, and `Observation` are all typed by default. Each carries the
declared-type view in `.values` plus a lossless all-`f64` view in
`.raw_values`.

```python
def on_action(action):
    action.values["gripper"]    # True, a real bool
    action.values["mode"]       # 3, a real int
    action.values["joint1"]     # 0.5, a float
    action.raw_values           # Dict[str, float], every field widened
```

For `Observation`, the same pair is named `.state` / `.raw_state`.

The Rust core mirrors this: `Action`, `State`, and `Observation`
carry `values: HashMap<String, TypedValue>` alongside `raw_values:
HashMap<String, f64>`. Declare a dtype, send an ordinary value, receive the
declared type.

## The active operator

The robot accepts actions from one operator at a time, named by its
`active_operator` pointer. Anyone in the room can read it or change it. The
robot's copy is authoritative.

```python
# Either role
portal.active_operator()                        # Optional[str]
await portal.set_active_operator("policy-v1")   # None clears it
portal.operators()                              # connected operator identities
portal.local_identity()                         # own identity, after connect

# Operator only
op.robot_identity()                             # the robot, once discovered
```

`set_active_operator` is symmetric. The robot writes its own attribute
directly. An operator sends a `portal.set_active_operator` RPC and the robot's
handler does the write. Either way the change propagates to everyone.

React to changes with three callbacks:

```python
portal.on_operator_joined(lambda identity: ...)
portal.on_operator_left(lambda identity: ...)
portal.on_active_operator_changed(lambda identity: ...)   # identity may be None
```

## Time sync

Every peer syncs to the robot's clock in the background, on the `portal_clock`
topic. It is always on.

```python
portal.now_us()                      # now on the robot's clock; on the robot, its own
portal.on_time_synced(lambda: ...)   # first sync, and after each resync

m = portal.metrics().time_sync
m.synced, m.offset_us, m.uncertainty_us
```

Before the first sync, `now_us()` is local time and `synced` is `False`.
`now_us()` never repeats and never goes backwards. It is the default
timestamp for everything you send. Details are in
[Metrics](07-metrics.md#time_sync) and the [wire protocol](reference/wire-protocol.md#clock).

**Opting out.** A lab whose hosts are already synced, with PTP or
GPS-disciplined clocks, can trust the system clock instead:

```python
cfg.set_time_sync_source(TimeSyncSource.SYSTEM)   # default: TimeSyncSource.PORTAL
```

`now_us()` is then the host's system time, still never repeating or going
backwards, and `synced` is `True` from the start. Portal keeps running the
exchange without applying it. A non-zero `measured_offset_us` shows a PTP
setup that has drifted or failed, before it shows up as bad data. It is a
per-peer choice, so `portal` and `system` peers share a room.

### Behavior worth knowing

**It starts unset.** A robot with no active operator drops every action. Your
first operator must claim control.

**It stays pinned on disconnect.** If the active operator leaves, the pointer
keeps naming it so a reconnect with the same identity resumes control. Reassign
explicitly to move on.

**It can be seeded at token-mint time**, so the robot is focused before anyone
connects:

```python
api.AccessToken(key, secret).with_attributes(
    {"lk.portal.active_operator": "policy-v1"}
)
```

**Both roles need `can_update_own_metadata=True`** in their token. `Robot` and
`Operator` self-set an `lk.portal.role` attribute on connect, and that write
fails without the grant. See
[Troubleshooting](08-troubleshooting.md#connect-fails-with-a-metadata-error).

## Multi-operator patterns

Because operators are just room participants, several useful setups fall out
without any extra API.

| Pattern | Who is in the room | What makes it work |
|---|---|---|
| **Single operator** | robot, 1 operator | Operator claims control at startup. |
| **Human in the loop** | robot, policy, human | Either side calls `set_active_operator`. Executed actions stay continuous across the cutover. |
| **Data recording** | robot, policy, human, recorder | Recorder joins with `set_action_subscription("active")` and logs every executed action with `action.sender`. |
| **Shadow evaluation** | robot, active policy, candidate policy | Candidate streams actions and the gate drops them. A recorder with `set_action_subscription("all")` gets both streams, with `action.active` telling them apart. |
| **Supervisor** | robot, N operators, supervisor UI | Supervisor never claims control. It only calls `set_active_operator` to route. |

Working versions of the last three live in the integration tests:
[action subscription](../python/packages/livekit-portal/tests/integration/test_action_subscription.py),
[handoff](../python/packages/livekit-portal/tests/integration/test_multi_operator.py).

## Operator-side action subscription

By default an operator only sends actions. It never sees what the robot
actually executed. Recorders, shadow policies, and monitoring UIs need that
view. `set_action_subscription` decides which actions reach `on_action`.

```python
cfg = OperatorConfig("session-1")
cfg.add_action_typed([("joint1", DType.F32)])   # needed to deserialize
cfg.set_action_subscription("active")           # "none" | "active" | "all"

op = Operator(cfg)
op.on_action(lambda action: log.append(action))
```

| Value | What reaches `on_action` |
|---|---|
| `none` (default) | nothing |
| `active` | only the active operator's actions, the same gate the robot runs |
| `all` | every operator's actions, including the ones the gate dropped |

Every action carries `sender` and `active`, both stamped at gate time.
`active=False` marks a shadow action the robot ignored. With `all` you can
record a candidate policy next to the one actually driving and compare them
offline. It means the action passed the gate, not that the robot carried it
out.

Actions are broadcast to the whole room either way, so this is a local filter
and `all` costs nothing extra on the wire. `none` is the default because most
operators are pure controllers that don't want the callback traffic.
`get_action()` mirrors the latest delivered action. v0.2's `True`/`False`
raise a `TypeError`: they are now `"active"`/`"none"`.

**Your own actions echo back.** LiveKit does not fan a publisher's own data
packets back to it, so a subscribed operator would otherwise miss its own
output. Portal fires the local callback after `send_action` whenever the
subscription would have delivered that action from anyone else: with `active`
only while you are the active operator, with `all` always, tagged with
`active` accordingly.

**Label rows with `action.sender`, not `active_operator()`.** Every `Action`
carries a `sender` stamped at gate time. Reading
`active_operator()` inside the callback can race a handoff that already moved
the pointer.

```python
def on_action(action):
    log.append({
        "ts_us": action.timestamp_us,
        "in_reply_to": action.in_reply_to_ts_us,
        "sender": action.sender,
        "active": action.active,
        "values": action.values,
    })
```

Swap the append for `model.compare(...)` and it is shadow evaluation. Swap it
for a websocket push and it is a monitoring UI.

## Video codecs

`add_video(name)` defaults to H.264 on the WebRTC media path. That is the right
choice for live preview and teleop, where a human is watching and bandwidth
should adapt.

```python
from livekit.portal import VideoCodec

cfg.add_video("front", max_bitrate_kbps=8000)                  # H264, 8 Mbps cap
cfg.add_video("wide", codec=VideoCodec.VP9, max_bitrate_kbps=4000)
```

`VP8`, `VP9`, `AV1`, and `H265` are also available. VP9 and AV1 compress better
at higher CPU cost. AV1 and H265 depend on both peers negotiating them, so
confirm before relying on either.

`max_bitrate_kbps` is a ceiling, not a target. libwebrtc still picks a lower
operating bitrate from the content. Omit it for the 10 Mbps default.

#### Simulcast and screencast

Two keyword-only toggles control encoder behavior. Both default to off and
both apply to the WebRTC codecs only.

```python
cfg.add_video("front", screencast=True)   # pin the resolution
cfg.add_video("wide", simulcast=True)     # publish several spatial layers
```

`simulcast=True` publishes several spatial layers at once. The SFU then hands
each subscriber the layer their link can carry. This costs encode CPU for
every extra layer. It only pays off when several operators subscribe over
links of differing quality. A single-operator teleop session gains nothing
from it, which is why it is off by default.

`screencast=True` marks the source as screen content. libwebrtc picks its
degradation preference from that flag, and that choice decides what gives way
under CPU or bandwidth pressure.

| Setting | libwebrtc preference | Under pressure |
|---|---|---|
| `screencast=False` (default) | `MAINTAIN_FRAMERATE` | Holds the frame rate, rescales the frame |
| `screencast=True` | `MAINTAIN_RESOLUTION` | Holds the resolution, drops frames |

The default is the reason a track can arrive at a resolution that shifts
during a session. Nothing in Portal resizes the frame. libwebrtc's adapter
scales it down before the encoder sees it, then scales back up once the
pressure clears.

Turn `screencast=True` on when a fixed frame shape matters more than smooth
motion. A policy that consumes pixels is the usual case. Leave it off for
human-watched teleop preview, where smooth motion is worth more than a
constant resolution.

**When a policy reads the pixels, H.264 is usually wrong.** It shifts
colorspace, adds block artifacts, and drifts in quality as the bitrate adapts.
Pass a byte-stream codec instead and each frame ships whole over a reliable
stream:

```python
cfg.add_video("front", codec=VideoCodec.MJPEG, quality=90)
cfg.add_video("wrist", codec=VideoCodec.PNG)
cfg.add_video("debug", codec=VideoCodec.RAW)
```

The user-facing API does not change. `send_video_frame`, `on_video_frame`,
`get_video_frame`, and observations all behave identically, and frames arrive
as RGB either way. Codec choice, the latency math, and per-track fps ceilings
are in [Frame video](05-frame-video.md).

## RPC

Either side can register methods and either side can invoke them. Use it for
one-shots that do not belong in a control loop.

```python
robot.register_rpc_method("home", lambda data: "ok")
robot.unregister_rpc_method("home")

reply = await op.perform_rpc("home", payload="{}")
```

Handlers must return a string. Full surface, error codes, and payload limits
are in [RPC](06-rpc.md).

## Metrics

```python
m = portal.metrics()
m.sync.observations_emitted
m.sync.states_dropped
m.rtt.rtt_us_p95
m.time_sync.offset_us
m.policy.e2e_us_p95

portal.reset_metrics()
```

Every field is documented in [Metrics](07-metrics.md).

## Gotchas

These are the behaviors that surprise people. Each one is deliberate.

**Dtype mismatch on send raises immediately.** Sending a `float` into a `BOOL`
field, or a `bool` into an `F32` field, raises
`PortalError.DtypeMismatch` before any packet is built. There is no silent
cast. `int` is accepted for float dtypes. `bool` is rejected everywhere except
`BOOL`.

**Schema mismatch is detected but never raises.** Every packet carries a `u32`
fingerprint over the ordered field names and dtypes. A peer whose schema
disagrees sees its packets dropped with one warning per offending fingerprint.
The healthy side keeps running. Nothing throws. See
[`schema-mismatch`](08-troubleshooting.md#schema-mismatch).

**Unknown field names on send are dropped.** Keys not in the declared schema
get one warning and are then ignored silently. A typo in a field name looks
exactly like a field that never arrives.

**Saturation is silent after one log line.** Sending `9999` into an `I8` clips
to `127`. The publisher warns once per field, then stays quiet. The peer only
ever sees the clipped value. `NaN` into an integer dtype becomes `0`, and into
`Bool` becomes `false`. Boundary values like `127` into `I8` do not saturate.

**Inactive operators stream into the void.** The robot drops actions from
anyone who is not the active operator, with no error and no callback on the
sender's side. Check `op.active_operator()`.

**Odd frame dimensions raise.** Both width and height must be even.

## Errors

`PortalError` variants you can catch:

| Variant | Cause |
|---|---|
| `AlreadyConnected` | `connect` called on a connected portal. |
| `NotConnected` | A send or RPC before `connect`, or after `disconnect`. |
| `NoPeer` | `perform_rpc` with no peer discovered and no `destination`. |
| `AmbiguousPeer` | Several remote participants and no peer identified. Pass `destination`. |
| `UnknownVideoTrack` | Track name was never declared with `add_video`. |
| `WrongFrameSize` | Buffer length is not `width * height * 3`. |
| `InvalidFrameDimensions` | Width or height is odd. |
| `WrongRole` | `send_action` on a robot, or `send_state` on an operator. |
| `ObservationSyncDisabled` | `on_observation` or `get_observation` on a peer with `set_observation_sync(False)`. |
| `DtypeMismatch` | A sent value's Python type disagrees with the declared dtype. |
| `Deserialization` | A received payload could not be parsed. |
| `Codec` | Frame encode or decode failed. |
| `Rpc` | The remote handler raised. Carries the `RpcError`. |

`ConfigFileError` is separate and only comes from the YAML loader. See
[Config from YAML](reference/config-file.md#errors).

## Surface summary

**Robot**

```text
# data
robot.send_video_frame(track, frame, width=None, height=None, timestamp_us=None)
robot.send_state(values, timestamp_us=None)
robot.on_action(cb)                          # active operator only
robot.get_action()

# control plane
robot.active_operator() / await robot.set_active_operator(identity)
robot.operators() / robot.local_identity()
robot.on_operator_joined(cb) / robot.on_operator_left(cb)
robot.on_active_operator_changed(cb)

# time sync
robot.now_us() / robot.on_time_synced(cb)

# rpc, metrics, lifecycle
robot.register_rpc_method(name, handler) / robot.unregister_rpc_method(name)
await robot.perform_rpc(method, payload, destination=None, response_timeout_ms=None)
robot.metrics() / robot.reset_metrics()
await robot.connect(url, token) / await robot.disconnect() / robot.close()
```

**Operator**

```text
# data
op.send_action(values, timestamp_us=None, in_reply_to_ts_us=None)
op.on_observation(cb) / op.on_state(cb) / op.on_drop(cb)
op.on_video_frame(track, cb)
op.get_observation() / op.get_state() / op.get_video_frame(track)
op.on_action(cb) / op.get_action()           # requires action subscription

# control plane
op.active_operator() / await op.set_active_operator(identity)
op.operators() / op.robot_identity() / op.local_identity()
op.on_operator_joined(cb) / op.on_operator_left(cb)
op.on_active_operator_changed(cb)

# time sync
op.now_us() / op.on_time_synced(cb)

# rpc, metrics, lifecycle
op.register_rpc_method(name, handler) / op.unregister_rpc_method(name)
await op.perform_rpc(method, payload, destination=None, response_timeout_ms=None)
op.metrics() / op.reset_metrics()
await op.connect(url, token) / await op.disconnect() / op.close()
```

## Using `Portal` directly

`Robot` and `Operator` are facades over a unified `Portal` class, which is also
exported:

```python
from livekit.portal import Portal, PortalConfig, Role

cfg = PortalConfig("session-1", Role.ROBOT)
portal = Portal(cfg)
```

`Portal` gets the same behavior the facades do. The gate, the role attribute,
and the built-in RPC handler are all present, with no opt-in flag. The only
difference is that the type system exposes every method regardless of role, so
you can call one that raises `WrongRole` at runtime.

Prefer `Robot` or `Operator` in new code. Reach for `Portal` when the role is
genuinely dynamic.

## Next steps

- [Tuning](04-tuning.md). `fps`, `slack`, `tolerance`, and reliability.
- [Frame video](05-frame-video.md). Pixel-exact frames for policies.
- [Metrics](07-metrics.md). Every counter, and which ones to alert on.
- [Config from YAML](reference/config-file.md). Move the schema into one shared
  file.
