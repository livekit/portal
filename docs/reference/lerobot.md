# lerobot plugins

> Optional convenience wrappers, for stacks already on lerobot.

Portal's own API works with any robotics stack and is the surface everything else
is built on. If you happen to be on [lerobot](https://github.com/huggingface/lerobot)
already, two plugin packages wrap that API so you do not have to write the glue
yourself.

Your existing local `Robot` or `Teleoperator` goes in. The plugin introspects its
`*_features`, brokers the traffic over a LiveKit room, and presents the remote arm
as an ordinary local lerobot device.

These are a shortcut, not the canonical way to use Portal. Read
[Concepts](../02-concepts.md) first, because the plugins expose the same model with
different names.

| Package | Runs on | What it wraps | What it presents |
|---|---|---|---|
| `lerobot-teleoperator-livekit` | the robot host | your local lerobot `Robot` | a `Teleoperator` fed by the remote operator |
| `lerobot-robot-livekit` | the operator host | your local `Teleoperator` | the remote physical robot, as a local `Robot` |

The naming inverts on purpose. On the robot host, the thing you are missing is a
source of actions, which is a `Teleoperator`. On the operator host, the thing you
are missing is the robot.

Both plugins do Portal sync for you: timestamp-matched observations, reliable
state and action channels, and RTT and jitter metrics.

## Install

Both are on PyPI and pull in `livekit-portal` automatically.

```bash
pip install lerobot-teleoperator-livekit   # robot host
pip install lerobot-robot-livekit          # operator host
```

They require **Python 3.12 or newer**, because lerobot does. `livekit-portal`
alone still supports 3.10.

To develop against the source tree, the packages live in this repo's `python/` uv
workspace:

```bash
bash scripts/build_ffi_python.sh release   # build the cdylib
cd python && uv sync                       # resolve the whole workspace
```

`build_ffi_python.sh` compiles the Rust FFI crate and drops the cdylib into
`python/packages/livekit-portal/livekit/portal/`, where `ctypes` loads it at import
time. Skip it only if `LIVEKIT_PORTAL_FFI_LIB` points at a prebuilt binary.

## Minting tokens

Run a local `livekit-server` or use [LiveKit Cloud](https://cloud.livekit.io). Mint
one JWT per side on the same room name. Identities must be unique in the room.

```python
import datetime

from livekit import api
from livekit.protocol.room import RoomConfiguration


def mint(identity: str, room: str, api_key: str, api_secret: str) -> str:
    grants = api.VideoGrants(
        room_join=True,
        room=room,
        can_publish=True,
        can_subscribe=True,
        # Required. Both plugins self-set an `lk.portal.role` attribute on
        # connect, and that write fails without this grant.
        can_update_own_metadata=True,
    )
    return (
        api.AccessToken(api_key, api_secret)
        .with_identity(identity)
        .with_grants(grants)
        # 0/1 ms playout delay minimizes video latency for teleop.
        .with_room_config(
            RoomConfiguration(name=room, min_playout_delay=0, max_playout_delay=1)
        )
        .with_ttl(datetime.timedelta(hours=6))
        .to_jwt()
    )
```

> **Note.** `can_update_own_metadata=True` is not optional. Without it, `connect()`
> fails with `failed to publish role attribute`. See
> [Troubleshooting](../08-troubleshooting.md#connect-fails-with-a-metadata-error).

## Robot host

Runs on the physical hardware. Your existing lerobot `Robot` subclass talks to the
motors and cameras exactly as before. `LiveKitTeleoperator` introspects it, infers
the motor keys and camera list from `observation_features`, and handles everything
network-related.

```python
from lerobot.robots.so100 import SO100Robot, SO100RobotConfig
from lerobot_teleoperator_livekit import (
    LiveKitTeleoperator,
    LiveKitTeleoperatorConfig,
)

robot = SO100Robot(SO100RobotConfig(...))      # your existing physical robot
robot.connect()

teleop = LiveKitTeleoperator(
    LiveKitTeleoperatorConfig(
        url="wss://your-project.livekit.cloud",
        token=mint("robot", "session-1", API_KEY, API_SECRET),
        session="session-1",
        fps=30,
    ),
    robot=robot,                               # schema inferred from here
)
teleop.connect()

try:
    while running:
        obs = robot.get_observation()           # the physical robot stays in the loop
        teleop.send_feedback(obs)               # goes over the wire to the operator

        action = teleop.get_action()             # latest operator action, {} if none
        if action:
            robot.send_action(action)            # the physical robot executes

        sleep(1 / 30)
finally:
    teleop.disconnect()
    robot.disconnect()
```

### What gets inferred

- **Motor keys** are the scalar entries of `robot.observation_features` and
  `robot.action_features`, for example `"shoulder.pos"` and `"elbow.pos"`.
- **Camera names and shapes** are the tuple-valued entries of
  `robot.observation_features`.
- **Portal field names** use the bare motor name. The `.pos` suffix is stripped on
  the wire and reattached on both sides.

### CLI mode

lerobot's `--teleop.type=livekit` path instantiates the plugin from config only and
cannot pass a `Robot` reference. Fill in `motors` and `camera_names` yourself:

```python
LiveKitTeleoperatorConfig(
    url=..., token=..., session="session-1", fps=30,
    motors=("shoulder", "elbow", "wrist"),
    camera_names=("cam1",),
)
```

When `robot=` is passed to the constructor, those two fields are ignored.

## Operator host

Runs wherever you drive from: a workstation, a training loop, a recording script.
`LiveKitRobot` wraps your local teleoperator so motor names come from its
`action_features`. Camera names come from the config, because only the robot side
knows what cameras exist.

```python
from lerobot.teleoperators.leader import LeaderArmTeleop, LeaderArmTeleopConfig
from lerobot_robot_livekit import LiveKitRobot, LiveKitRobotConfig

leader = LeaderArmTeleop(LeaderArmTeleopConfig(...))
leader.connect()

robot = LiveKitRobot(
    LiveKitRobotConfig(
        url="wss://your-project.livekit.cloud",
        token=mint("operator", "session-1", API_KEY, API_SECRET),
        session="session-1",
        fps=30,
        camera_names=("cam1",),                # must match the robot side
        camera_height=480, camera_width=640,   # advertised in observation_features
    ),
    teleop=leader,                             # schema inferred from here
)
robot.connect()

try:
    while running:
        obs = robot.get_observation()          # synced state and frames from remote
        action = leader.get_action()           # local teleop produces the action
        robot.send_action(action)              # forwarded over LiveKit
        sleep(1 / 30)
finally:
    robot.disconnect()
    leader.disconnect()
```

Recording datasets, evaluating policies, and lerobot's built-in teleop loops all
work with `robot` here. None of them see that it is remote.

### Declaring extra state keys

By default the operator assumes the robot reports back exactly the motors it
commands, so state mirrors action.

If your robot also sends readings it does not accept as commands, like slider
positions or current sensors, set `observation_features` on the config. It becomes
the authoritative state schema and replaces the mirror assumption entirely.

```python
LiveKitRobotConfig(
    ...,
    observation_features={
        "shoulder.pos": float,
        "elbow.pos": float,
        "slider.pos": float,   # extra, not in the action schema
    },
)
```

The dict follows lerobot's own `observation_features` convention: scalar types for
motors, shape tuples for cameras. The robot side must declare and send the same
keys.

### CLI mode

With `--robot.type=livekit`, supply `motors` on the config:

```python
LiveKitRobotConfig(
    url=..., token=..., session="session-1", fps=30,
    motors=("shoulder", "elbow", "wrist"),
    camera_names=("cam1",),
)
```

`teleop=` overrides these when passed.

## Config reference

Shared by both plugin configs:

| Field | Default | Purpose |
|---|---|---|
| `url` | `""` | LiveKit server URL. Required. |
| `token` | `""` | JWT with grants for this side's identity and room. Required. |
| `session` | `"lerobot"` | Portal session label. Must match across both sides. |
| `fps` | `30` | Unified capture rate. Drives the sync match window. |
| `motors` | `()` | Fallback motor names, without `.pos`, when no local instance is passed. |
| `camera_names` | `()` | Camera names. On the operator side these must match the robot's. |
| `slack` | `None` | Passed to `set_slack(...)`. Bump under jitter or asymmetric rates. |
| `tolerance` | `None` | Passed to `set_tolerance(...)`. `1.5` widens to one frame either side, `0.5` drops on loss. |
| `state_reliable` | `True` | Reliable delivery for state. |
| `action_reliable` | `True` | Reliable delivery for actions. |
| `on_stall` | `DROP` | What to do with a moment a silent camera cannot cover. `FREEZE` re-emits its last matched frame instead of dropping the state. |

Operator-only, on `LiveKitRobotConfig`:

| Field | Default | Purpose |
|---|---|---|
| `auto_claim_control` | `True` | Claim the active-operator pointer on connect so the robot accepts our actions. Turn it off in HITL setups where another participant arbitrates. |
| `camera_height` | `480` | Shape advertised in `observation_features`. Metadata only, Portal accepts any resolution at runtime. |
| `camera_width` | `640` | As above. |
| `observation_features` | `None` | Full state schema when the robot reports more than the action keys. Replaces the mirror assumption. |

The room identity comes from the LiveKit token you mint, via `with_identity(...)`,
not from any config field.

See [Tuning](../04-tuning.md) for the math behind `fps`, `slack`, and `tolerance`.

## Frame formats

**Sending, robot host.** `robot.get_observation()["camera"]` must be an
`np.ndarray` of shape `(H, W, 3)`, dtype uint8, in RGB order. That is what every
stock lerobot `Robot` subclass already returns.

**Receiving, operator host.** `livekit_robot.get_observation()["camera"]` has the
same shape, dtype, and order.

If you already hold I420 bytes from a hardware pipeline, skip the plugin and call
`livekit.portal.Robot.send_video_frame(...)` directly. Note that it takes RGB, not
I420. The ergonomic NumPy path is the only thing the plugin adds here.

## Async internals

Portal's `connect` and `disconnect` are async. lerobot's `Robot` and
`Teleoperator` interfaces are synchronous.

Each plugin spins up a dedicated asyncio loop in a daemon thread on `connect()` and
tears it down in `disconnect()`. The `send_*` and `get_*` methods stay fully
synchronous, because they do not need the loop.

That loop also handles Portal's callback dispatch. To register Portal callbacks
directly, reach into `plugin._portal` from code running on that loop.

## Known limitations

**Python 3.12 or newer.** lerobot requires it. `livekit-portal` alone still works
on 3.10, but the plugin packages and the workspace root are 3.12 and up.

**Protobuf constraint.** The plugins pin `protobuf>=5,<6` because lerobot's
transitive dependencies cap there.
`packages/livekit-portal/scripts/generate_protos.sh` rewrites the `_pb2.py` gencode
version to `5.26.0` after each `protoc` run so it loads on that runtime.

**macOS libwebrtc linker flag.** `-ObjC` is set in `livekit-portal-ffi/build.rs` so
VideoToolbox's ObjC categories link correctly. Dropping it produces an
`NSInvalidArgumentException` at the first `PeerConnection` creation.

**Plugin discovery is import-time.** lerobot subclass registration fires on import.
Either the CLI's `--robot.type=livekit` mechanism needs the package on the import
path, or your script imports `lerobot_robot_livekit` or
`lerobot_teleoperator_livekit` before instantiating the config.

**Schema inference is shallow.** The plugin reads `robot.observation_features` and
`teleop.action_features` once, at construction. If your class mutates them later,
reconstruct the plugin. On the operator side, prefer declaring
`observation_features` explicitly over relying on inference from the teleop.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `ffi not initialized` | The cdylib did not load. Rerun `build_ffi_python.sh` or set `LIVEKIT_PORTAL_FFI_LIB`. |
| `LiveKit*Config.url and .token are required` | Token mint returned an empty string, or the fields were never set. |
| `failed to publish role attribute (token may be missing canUpdateOwnMetadata)` | The token omitted `can_update_own_metadata=True`. |
| Observations always empty | First sync has not happened yet. Confirm both sides joined the same room, camera names match, and `fps` is identical. |
| Observations always empty, state only | State schema mismatch. The two sides declared different motor keys. A `WARNING` fires on the first dropped sync naming the missing and unexpected fields. Declare `observation_features` explicitly. |
| High `states_dropped` | The encoder is throttling, or a camera stopped publishing. Compare `frames_received` on the operator against `frames_sent` on the robot. |
| `WrongRole` | You called `send_action` on the robot side, or `send_state` and `send_video_frame` on the operator side. |
| Robot receives no actions, no errors | `active_operator` is unset or pointing elsewhere. The plugin auto-claims by default. With `auto_claim_control=False`, claim via `plugin._portal.set_active_operator(...)` or have a peer do it. |
| `InvalidFrameDimensions` | Frame width or height is odd. Both must be even. |
| `ValueError: ... cannot infer schema` | The constructor got neither a local instance nor `motors` and `camera_names`. Pass one or the other. |

For anything not in that table, see [Troubleshooting](../08-troubleshooting.md).

## Reference

- [Concepts](../02-concepts.md). The model the plugins wrap.
- [Portal API](../03-portal-api.md). What they are built on, and what to drop down
  to when you need more control.
- [Tuning](../04-tuning.md). The knobs the config fields forward to.
- [`examples/python/so101/`](../../examples/python/so101). A working two-arm setup
  on real hardware.
