# Using livekit-portal with lerobot

Two plugin packages expose livekit-portal to [lerobot](https://github.com/huggingface/lerobot). The physical hardware stays in the loop. the plugins wrap around whatever local `Robot` or `Teleoperator` class you already use, introspect its `*_features`, and brokers the traffic over a LiveKit room.

- `lerobot-teleoperator-livekit` — **robot side**. `LiveKitTeleoperator` wraps your local lerobot `Robot` (e.g. a SO-100 over USB). You keep calling `robot.get_observation()` and `robot.send_action(...)`; the plugin just adds the network tier.
- `lerobot-robot-livekit` — **operator side**. `LiveKitRobot` wraps your local `Teleoperator` (leader arm, gamepad, etc.) for shape inference, and presents the remote physical robot as a local `Robot` to any lerobot workflow (teleoperation, dataset recording, policy eval).

Both plugins do Portal sync for you: timestamp-matched observations, reliable state/action channels, RTT/jitter metrics.

## Install

The packages live in this repo's `python/` uv workspace. For a consumer repo, depend on them directly; for local development:

```bash
bash scripts/build_ffi_python.sh release                 # build the cdylib
cd python && uv sync                                     # resolves everything
```

`build_ffi_python.sh` compiles the Rust FFI crate and drops the cdylib into `python/packages/livekit-portal/livekit/portal/`, where `ctypes` loads it at import time. Skip it only if you've set `LIVEKIT_PORTAL_FFI_LIB` to a prebuilt binary.

Standalone install (once published):

```bash
uv pip install lerobot-teleoperator-livekit   # robot-side
uv pip install lerobot-robot-livekit          # operator-side
```

## LiveKit setup

Run a local `livekit-server` or use [LiveKit Cloud](https://cloud.livekit.io). Mint two JWTs — one per side — on the same room name. Both grants need `room_join`, `can_publish`, `can_subscribe`. Identities must be unique within the room.

```python
import datetime
from livekit import api
from livekit.protocol.room import RoomConfiguration

def mint(identity: str, room: str, api_key: str, api_secret: str) -> str:
    grants = api.VideoGrants(
        room_join=True, room=room, can_publish=True, can_subscribe=True
    )
    return (
        api.AccessToken(api_key, api_secret)
        .with_identity(identity)
        .with_grants(grants)
        # min/max playout delay in ms; 0/1 minimizes video latency for teleop.
        .with_room_config(
            RoomConfiguration(name=room, min_playout_delay=0, max_playout_delay=1)
        )
        .with_ttl(datetime.timedelta(hours=6))
        .to_jwt()
    )
```

## Robot side: wrapping a local lerobot Robot

Runs on the physical hardware. The user's existing lerobot Robot subclass talks to the motors and cameras as usual. `LiveKitTeleoperator` introspects it, infers motor keys + camera list from `observation_features`, and handles everything network-related.

```python
from lerobot.robots.so100 import SO100Robot, SO100RobotConfig  # or whatever class you already use
from lerobot_teleoperator_livekit import (
    LiveKitTeleoperator, LiveKitTeleoperatorConfig,
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
    robot=robot,                               # schema inferred from robot
)
teleop.connect()

try:
    while running:
        obs = robot.get_observation()          # physical robot stays in the loop
        teleop.send_feedback(obs)              # obs goes over the wire to operator

        action = teleop.get_action()           # latest operator action (empty dict if none)
        if action:
            robot.send_action(action)          # physical robot executes

        sleep(1 / 30)
finally:
    teleop.disconnect()
    robot.disconnect()
```

### What gets inferred from `robot`

- Motor keys are the scalar entries of `robot.observation_features`/`robot.action_features` (e.g. `"shoulder.pos"`, `"elbow.pos"`).
- Camera names + shapes are the tuple-valued entries of `robot.observation_features`.
- Portal's declared state/action fields use the bare motor name (the `.pos` suffix is stripped on the wire and reattached on both sides).

### CLI mode

If you're using lerobot's `--teleop.type=livekit` CLI path (which instantiates the plugin from config only and can't pass a Robot reference), fill in `motors` and `camera_names` on the config:

```python
LiveKitTeleoperatorConfig(
    url=..., token=..., session="session-1", fps=30,
    motors=("shoulder", "elbow", "wrist"),
    camera_names=("cam1",),
)
```

Whenever a `robot=` is passed to the constructor, those fields are ignored.

## Operator side: wrapping a local Teleoperator

Runs where you want to drive the robot — a workstation, a training loop, a data-recording script. `LiveKitRobot` wraps your local teleoperator (leader arm, gamepad, policy output) so motor names are inferred from its `action_features`; camera names come from the config because only the robot side knows what cameras it has.

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
    teleop=leader,                             # schema inferred from leader
)
robot.connect()

try:
    while running:
        obs = robot.get_observation()          # synced state + camera frames from the remote robot
        action = leader.get_action()           # local teleop generates the action
        robot.send_action(action)              # forwarded over LiveKit
        sleep(1 / 30)
finally:
    robot.disconnect()
    leader.disconnect()
```

Recording datasets, evaluating policies, or invoking lerobot's built-in teleop loops all work with `robot` here — they never see that it's remote.

### Declaring extra state keys

By default the operator assumes the robot reports back exactly the motors it commands (state mirrors action). If your robot also sends readings that aren't commanded — slider positions, current sensors, anything else — set `observation_features` on the config. It is the authoritative state schema and replaces the mirror assumption entirely.

```python
LiveKitRobotConfig(
    ...,
    observation_features={
        "shoulder.pos": float,
        "elbow.pos": float,
        "slider.pos": float,   # extra — not in the action schema
    },
)
```

The dict follows lerobot's own `observation_features` convention: scalar types for motors, shape tuples for cameras. The robot side must declare and send the same keys via its `send_feedback` call.

### CLI mode

If you instantiate via `--robot.type=livekit`, supply `motors` on the config:

```python
LiveKitRobotConfig(
    url=..., token=..., session="session-1", fps=30,
    motors=("shoulder", "elbow", "wrist"),
    camera_names=("cam1",),
)
```

`teleop=` overrides these when passed.

## Config reference

Shared between both plugin configs:

| Field | Default | Purpose |
|---|---|---|
| `url` | `""` | LiveKit server URL (`wss://...`). Required. |
| `token` | `""` | JWT with grants for this side's identity + room. Required. |
| `session` | `"lerobot"` | Portal session = LiveKit room name. Must match across both sides. |
| `fps` | `30` | Unified capture rate. Drives the sync match window. |
| `motors` | `()` | Fallback motor names (no `.pos`) when no local instance is passed. |
| `camera_names` | `()` | Camera names; on the operator side must match the robot side's publisher. |
| `slack` | `None` | `set_slack(...)`. Bump under jitter or asymmetric rates. |
| `tolerance` | `None` | `set_tolerance(...)`. `1.5` widens to ±1 frame; `0.5` drops on loss. |
| `state_reliable` | `True` | SCTP reliable delivery for state. |
| `action_reliable` | `True` | SCTP reliable delivery for action. |
| `reuse_stale_frames` | `False` | Re-emit the last matched frame when a newer one hasn't arrived yet. |

Operator-only (`LiveKitRobotConfig`):

| Field | Default | Purpose |
|---|---|---|
| `identity` | `"operator"` | Portal operator identity. Set per-instance when running multiple operators (HITL, ensemble policies). |
| `auto_claim_control` | `True` | Self-claim the active-operator pointer on connect so the robot accepts our actions. Disable in HITL setups where another participant arbitrates. |
| `camera_height` | `480` | Camera shape advertised in `observation_features` (metadata only — Portal accepts any resolution at runtime). |
| `camera_width` | `640` | See above. |
| `observation_features` | `None` | Full state schema when the robot reports state beyond the action keys (e.g. `{"shoulder.pos": float, "slider.pos": float}`). When set, replaces the default "state mirrors action" assumption. Follows lerobot's `observation_features` convention: scalar types for motors, shape tuples for cameras. |

See [tuning.md](tuning.md) for the math behind `fps`, `slack`, and `tolerance`.

## Frame formats

- **send (robot side)**: `robot.get_observation()["camera"]` must be `np.ndarray` of shape `(H, W, 3)` dtype uint8 in RGB order. That's what every stock lerobot Robot subclass already returns.
- **receive (operator side)**: `livekit_robot.get_observation()["camera"]` is the same shape/dtype/order. Portal delivers I420 on the wire; the plugin converts to RGB on the way out.

If you already have I420 bytes from a hardware pipeline, bypass the plugin and call `livekit.portal.Robot.send_video_frame(...)` directly (it takes RGB bytes, not I420). The ergonomic numpy path is the only thing the plugin adds on top.

## Async internals

Portal's `connect` / `disconnect` are async; lerobot's `Robot` / `Teleoperator` interfaces are synchronous. Each plugin spins up a dedicated asyncio loop in a daemon thread on `connect()` and tears it down in `disconnect()`. `send_*` / `get_*` stay fully synchronous — they don't need the loop.

The loop also handles Portal's callback dispatch, so if you ever want to register Portal callbacks (`on_observation`, `on_drop`, etc.) directly, reach into `plugin._portal` from code that runs on that loop.

## Known limitations

- **Python ≥ 3.12.** lerobot itself requires it. `livekit-portal` alone still works on 3.10+, but the plugin packages and workspace root are 3.12+.
- **Protobuf constraint.** The plugins pin to `protobuf>=5,<6` because lerobot's transitive deps cap there. `packages/livekit-portal/scripts/generate_protos.sh` rewrites the `_pb2.py` gencode version to `5.26.0` after each `protoc` run to make it load on that runtime.
- **macOS libwebrtc linker.** `-ObjC` is set in `livekit-portal-ffi/build.rs` so VideoToolbox's ObjC categories link correctly. Don't drop it or you'll hit `NSInvalidArgumentException` at first `PeerConnection` creation.
- **Plugin discovery.** lerobot subclass registration fires at import time. Either the CLI's `--robot.type=livekit` / `--teleop.type=livekit` mechanism needs the package on the import path, or your script imports `lerobot_robot_livekit` / `lerobot_teleoperator_livekit` before instantiating the config.
- **Schema inference is shallow.** The plugin reads `robot.observation_features` / `teleop.action_features` once at construction. If your local class mutates those later, reconstruct the plugin. On the operator side, use `observation_features` on `LiveKitRobotConfig` to declare the schema explicitly rather than relying on inference from the teleop.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `ffi not initialized` | cdylib didn't load. Rerun `build_ffi_python.sh` or set `LIVEKIT_PORTAL_FFI_LIB`. |
| `LiveKit*Config.url and .token are required` | Token mint returned empty string, or you forgot to set them in the config. |
| Observations always empty | First sync hasn't happened yet. Confirm both sides joined the same room, camera names match, and `fps` is identical. |
| Observations always empty (state only) | State schema mismatch — the operator and robot declared different motor keys. A `WARNING` log fires on the first dropped sync naming the missing and unexpected fields. Check `logging` output or set `logging.basicConfig(level=logging.WARNING)` to surface it. Use `observation_features` on `LiveKitRobotConfig` to declare the exact schema the robot sends. |
| High `states_dropped` | Encoder is throttling or a camera stopped publishing. Compare `portal.metrics().transport.frames_received` (operator) with `frames_sent` (robot). |
| `WrongRole` `PortalError` | You're calling `send_action` on the robot side or `send_state`/`send_video_frame` on the operator side. Role is fixed by which class you instantiated (`Robot` vs `Operator`). |
| Robot receives no actions, no errors | `active_operator` is unset or pointing somewhere else. The plugin auto-claims on connect by default; if you turned `auto_claim_control` off, claim explicitly via `plugin._portal.set_active_operator(...)` or have a peer (supervisor, human) do it. |
| `failed to publish role attribute (token may be missing canUpdateOwnMetadata)` | Token-mint omitted `can_update_own_metadata=True`. The new `Robot` / `Operator` classes self-set the role attribute on connect; the grant must allow it. |
| `InvalidFrameDimensions` | Frame width or height is odd. Portal requires even dimensions for I420 chroma subsampling. |
| `ValueError: ... cannot infer schema` | Constructor got neither a local instance nor `motors`/`camera_names` on the config. Pass one or the other. |
