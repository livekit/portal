# Portal API

The primary surface for using livekit-portal from any robotics stack.

You construct a `RobotConfig` or `OperatorConfig`, hand it to a `Robot` or
`Operator`, register callbacks, and push frames and state or actions.
Everything else in this repository, including the optional lerobot plugins,
is built on top of this API.

## Installation

Portal is not on PyPI yet. You build from source. See the
[Quickstart](quickstart.md#1-install) for the full flow. Summary:

```bash
git clone https://github.com/livekit/livekit-portal.git
cd livekit-portal

bash scripts/build_ffi_python.sh release   # or `debug` for faster iteration
cd python/packages/livekit-portal && uv sync
```

If the cdylib lives elsewhere (e.g. during Rust-side dev), point
`LIVEKIT_PORTAL_FFI_LIB` at it and skip the copy step.

### Rust

The core crate is usable directly without going through Python. From
another Cargo workspace, depend on the path:

```toml
[dependencies]
livekit-portal = { path = "path/to/livekit-portal/livekit-portal" }
```

Python bindings ship via the `livekit-portal-ffi` crate (UniFFI + C ABI)
and a pure-Python package in `python/packages/livekit-portal/`.

## Roles

Portal is a two-role system: one **robot** and one or more **operators**
per session.

| Role | Publishes | Subscribes |
|------|-----------|------------|
| `Robot` | video frames, state | actions |
| `Operator` | actions | video frames + state, merged into synced observations |

Roles are baked into the class you instantiate (`Robot` or `Operator`).
There can be at most one robot per session. There can be any number of
operators (humans teleoperating, policies running inference, supervisors
arbitrating control). The robot listens to one operator at a time, named
by the `active_operator` pointer; everyone else streams silently and the
robot drops their actions at the gate. See
[Multi-controller](#multi-controller-and-active_operator) below.

Both sides must register the same schema via `add_video` /
`add_state_typed` / `add_action_typed`. Camera names, field names, and
per-field dtypes must match across sides.

State and action schemas are typed. Each field declares a `DType` that
drives its on-wire width. `DType.F64` is the lossless default. `F32`
halves the bytes per field for joint angles. `I8`, `I16`, `U8`, `U16`,
`U32` suit discrete indices or counters. `Bool` is one byte for binary
signals like gripper open or estop. Values you send through `send_state`
/ `send_action` stay as Python floats. Saturation applies at the wire
boundary for out-of-range integer values.

## Robot side

```python
import asyncio
from livekit.portal import DType, Robot, RobotConfig

async def main():
    cfg = RobotConfig("session")
    cfg.add_video("camera1")
    cfg.add_video("camera2")
    cfg.add_state_typed([
        ("joint1", DType.F32),
        ("joint2", DType.F32),
        ("joint3", DType.F32),
    ])
    cfg.add_action_typed([
        ("joint1", DType.F32),
        ("joint2", DType.F32),
        ("joint3", DType.F32),
    ])

    portal = Robot(cfg)

    def on_action(action):
        # action.values is the dict.
        # action.timestamp_us is the sender's clock.
        # Only actions from the active operator reach this callback;
        # everyone else is dropped silently at the gate.
        robot.send_action(action.values)

    portal.on_action(on_action)
    await portal.connect(url, token)

    while running:
        obs = robot.get_observation()
        portal.send_video_frame("camera1", obs.image.cam1, width, height)
        portal.send_video_frame("camera2", obs.image.cam2, width, height)
        portal.send_state(obs.state)
        await asyncio.sleep(1 / fps)

asyncio.run(main())
```

## Operator side

```python
import asyncio
from livekit.portal import DType, Operator, OperatorConfig

async def main():
    cfg = OperatorConfig("session", identity="policy-v1")
    cfg.add_video("camera1")
    cfg.add_video("camera2")
    cfg.add_state_typed([
        ("joint1", DType.F32),
        ("joint2", DType.F32),
        ("joint3", DType.F32),
    ])
    cfg.add_action_typed([
        ("joint1", DType.F32),
        ("joint2", DType.F32),
        ("joint3", DType.F32),
    ])

    portal = Operator(cfg)

    def on_observation(obs):
        # obs.frames: dict[str, np.ndarray]   # one per registered video track
        # obs.state:  dict[str, float]
        # obs.timestamp_us: int               # sender clock
        action = model.select_action(obs)
        portal.send_action(action)

    portal.on_observation(on_observation)
    await portal.connect(url, token)

    # Robot starts with `active_operator=None` and drops every action.
    # Claim control to be the one whose actions are accepted.
    await portal.set_active_operator(portal.local_identity())

asyncio.run(main())
```

Callbacks fire on the asyncio loop that was running when you registered
them. User code never runs on the tokio worker thread.

## Multi-controller and `active_operator`

The robot exposes one piece of state, `active_operator`, naming the
operator whose actions are accepted. Anyone in the room can read or
change it. The robot's attribute is the source of truth.

```python
# Robot side
portal.active_operator()            # -> Optional[str]
await portal.set_active_operator("policy-v1")
portal.operators()                  # currently-connected operator identities
portal.local_identity()             # this robot's identity (after connect)

# Operator side
portal.active_operator()            # mirrors the robot's attribute
await portal.set_active_operator("policy-v1")   # RPC under the hood
portal.operators()                  # peer operators in the room
portal.robot_identity()             # the robot's identity (once discovered)
portal.local_identity()             # this operator's own identity
```

`set_active_operator` is symmetric. The robot writes its own attribute
directly; the operator dispatches a `portal.set_active_operator` RPC and
the robot's handler does the write. Pass `None` to clear the pointer
(robot will drop everything until something sets it again).

Three callbacks let you react to room changes:

```python
portal.on_operator_joined(lambda identity: ...)
portal.on_operator_left(lambda identity: ...)
portal.on_active_operator_changed(lambda new_identity: ...)
```

**Defaults and edge cases.**

- `active_operator` defaults to `None`. A robot with no active operator
  drops every action.
- When the active operator disconnects, the pointer **stays pinned** at
  the disconnected identity. A reconnect with the same identity resumes
  control. To reassign, anyone in the room can call
  `set_active_operator("...")`.
- Tokens may seed the robot's `lk.portal.active_operator` attribute at
  mint time so the pointer is set before anyone connects:

  ```python
  api.AccessToken(...)
     .with_attributes({"lk.portal.active_operator": "policy-v1"})
  ```

- Tokens for `Robot` and `Operator` participants must include
  `can_update_own_metadata=True`. Both classes self-set the
  `lk.portal.role` attribute on connect; without the grant the call
  fails.

For HITL patterns and the full design rationale, see
[`spec.md`](../spec.md).

## Typed values on receive

`Action`, `State`, and `Observation` are typed by default. `.values`
(and `observation.state`) hold Python-native types per the declared
schema: `DType.BOOL` fields are `bool`, integer dtypes are `int`, float
dtypes are `float`. `.raw_values` (and `observation.raw_state`) keep
the lossless `f64` view if you want to write into a numpy buffer
without a per-field cast.

```python
def on_action(action):
    # action.values["gripper"] is True (bool)
    # action.values["mode"] is 3 (int)
    # action.values["shoulder"] is 0.5 (float)
    # action.raw_values is the underlying Dict[str, float]
    ...
```

The Rust SDK mirrors this: `Action` / `State` / `Observation` carry
`values: HashMap<String, TypedValue>` alongside `raw_values:
HashMap<String, f64>`. The mental model is identical across languages:
declare a dtype, send whatever you want, receive back as the declared
type.

## Gotchas

- **Send-time dtype mismatch raises immediately.** If you send a
  `float` into a `BOOL` field, a `bool` into a `F32` field, or any
  other type that doesn't match the declared dtype, `send_state` /
  `send_action` raises `PortalError::DtypeMismatch` before the packet
  is constructed. No silent cast. Python follows the same rule via
  `isinstance` checks on each value. `int` is accepted for float
  dtypes (standard numeric promotion); `bool` is rejected everywhere
  except `BOOL` fields.
- **Saturation is silent except for a one-time log.** Saturation
  happens after the dtype check passes — e.g., sending `9999` as an
  `i8` in Rust (or `9999` as an int for an `I8` field in Python)
  clips to `127`. The publisher emits a single `WARN` per (topic,
  field) on first saturation, then stays quiet. The peer receives
  the clipped value and never sees the original.
- **Schema mismatch is detected but not raised.** Every packet carries a
  `u32` fingerprint derived from the ordered field names and dtypes. A
  peer whose schema disagrees (any rename, dtype flip, or reorder) sees
  its packets dropped with a `WARN` per unique offending fingerprint. The
  healthy side keeps running. No exception is raised.
- **Unknown field names on send are dropped.** Keys in the dict you pass
  to `send_action` / `send_state` that are not in the declared schema get
  a one-time `WARN` and are then silently ignored. Check `portal.metrics()`
  and your logs if a field appears to not arrive — the typo is the usual
  cause.
- **Inactive operators stream into the void.** The robot drops actions
  whose sender is not the active operator. There is no error or callback
  on the operator side. Read `op.active_operator()` (or watch
  `on_active_operator_changed`) to know whether your `send_action` is
  actually being honored.
- **NaN into `Bool` becomes `false`.** NaN into integer dtypes becomes
  `0`. Both count as saturation and log once per field.
- **Boundary values do not saturate.** `127.0` into `I8`, `-128.0` into
  `I8`, `65535.0` into `U16`, and `0.0` into any unsigned type are
  representable and silent.

## Frame format

`send_video_frame` expects packed RGB24 NumPy arrays of shape `(H, W, 3)`
uint8. Width and height must both be even. Full details in
[concepts.md](concepts.md#video-frame-format).

## Frame video (lossless or codec-of-your-choice)

`add_video(name)` defaults to `VideoCodec.H264`, the WebRTC media path
(lossy). For policies that read the pixels — VLA inference, behavior
cloning, any case where colorspace shift breaks the policy distribution
— pass a non-H264 codec on the same call:

```python
from livekit.portal import VideoCodec

cfg.add_video("front", codec=VideoCodec.MJPEG, quality=90)
cfg.add_video("wrist", codec=VideoCodec.PNG)
cfg.add_video("debug", codec=VideoCodec.RAW)
```

The user-facing API is identical — `send_video_frame`, `on_video_frame`,
`get_video_frame`, observations all work the same way. The frames travel
over a reliable byte stream (not WebRTC media), encoded with the chosen
codec, and arrive as RGB on the other end.

Latency scales with encoded payload size: the byte-stream path costs
roughly `1 ms + 2 ms × ⌈encoded_size / 15 KB⌉` per frame on localhost.
Pick a codec whose output fits in one chunk for low-latency inference.
At typical inference resolutions (224×224 to 480p) MJPEG q=80–95 fits.

See [frame-video.md](frame-video.md) for the codec/fps tables, wire
format, and metrics surface.

## Surface summary

**Robot**

```text
# data plane
robot.send_video_frame(track, frame, [width, height,] timestamp_us=...)
robot.send_state(values, timestamp_us=...)
robot.on_action(cb)                      # only fires for the active operator
robot.on_action_chunk(name, cb)
robot.get_action() / robot.get_action_chunk(name)

# control plane
robot.active_operator() / await robot.set_active_operator(id)
robot.operators()
robot.local_identity()
robot.on_operator_joined(cb) / robot.on_operator_left(cb)
robot.on_active_operator_changed(cb)

# rpc, metrics, lifecycle
robot.register_rpc_method(name, handler) / robot.unregister_rpc_method(name)
await robot.perform_rpc(name, payload, destination=None)
robot.metrics() / robot.reset_metrics()
await robot.connect(url, token) / await robot.disconnect()
```

**Operator**

```text
# data plane
op.send_action(values, timestamp_us=..., in_reply_to_ts_us=...)
op.send_action_chunk(name, data, timestamp_us=..., in_reply_to_ts_us=...)
op.on_state(cb) / op.on_observation(cb) / op.on_drop(cb)
op.on_video_frame(track, cb)
op.get_state() / op.get_observation() / op.get_video_frame(track)

# control plane
op.active_operator() / await op.set_active_operator(id)   # RPC under the hood
op.operators()
op.robot_identity() / op.local_identity()
op.on_operator_joined(cb) / op.on_operator_left(cb)
op.on_active_operator_changed(cb)

# rpc, metrics, lifecycle
op.register_rpc_method(name, handler) / op.unregister_rpc_method(name)
await op.perform_rpc(name, payload, destination=None)
op.metrics() / op.reset_metrics()
await op.connect(url, token) / await op.disconnect()
```

## End-to-end encryption

Call `cfg.set_e2ee_key(key: bytes)` before `connect`. Both peers must use the
same key. All media tracks and data channels (state, actions, RPC) are
encrypted with AES-GCM.

```python
import os

cfg.set_e2ee_key(os.environ["PORTAL_E2EE_KEY"].encode())
```

See [e2ee.md](e2ee.md) for key generation, distribution patterns, and coverage
details.

## Direct `Portal` usage

`Robot` and `Operator` are role-specific facades around a unified
`Portal` class that also ships in `livekit.portal` for advanced use:

```python
from livekit.portal import DType, Portal, PortalConfig, Role
cfg = PortalConfig("session", Role.ROBOT)
portal = Portal(cfg)
```

The unified surface gets the same multi-controller behavior `Robot` /
`Operator` do (gate, role attribute, RPC handler, etc.) — there is no
opt-in flag. The class choice only affects which methods the type system
exposes; runtime behavior is identical. New code should usually pick
`Robot` or `Operator` for the role-correct surface.

## Reference

- [Concepts](concepts.md). Roles, observation model, frame format.
- [Frame video](frame-video.md). Codec choice, latency math, wire format
  for byte-stream-based per-frame video.
- [Tuning](tuning.md). `fps`, `slack`, `tolerance`, asymmetric rates.
- [Synchronization deep dive](synchronization.md). The match algorithm.
- [RPC](rpc.md). Imperative commands on top of LiveKit RPC.
- [E2EE](e2ee.md). Shared-key end-to-end encryption.
- [`examples/python/basic/`](../examples/python/basic). The smallest
  end-to-end script using this API, with synthetic video.
- [lerobot integration](lerobot.md). The optional convenience plugins that
  wrap this API for lerobot users.
