# Quickstart

Get a robot host and a control host talking over LiveKit in about 5 minutes
using the Portal API directly.

If you are already on lerobot, there is a one-line shortcut at the bottom of
this page that wraps the same code.

## What you need

- A [Rust toolchain](https://rustup.rs/) (stable `cargo`)
- Python 3.10+ and [`uv`](https://docs.astral.sh/uv/)
- A LiveKit server: [LiveKit Cloud](https://cloud.livekit.io) (free tier
  works) or a local `livekit-server --dev`
- Your `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET`

You do **not** need a physical robot to try this. The first example publishes
a synthetic test pattern.

## 1. Install

Portal is not on PyPI yet, and there are no prebuilt native binaries. You
build from source.

```bash
git clone https://github.com/livekit/livekit-portal.git
cd livekit-portal

bash scripts/build_ffi_python.sh release   # compile cdylib + generate UniFFI bindings
cd python/packages/livekit-portal && uv sync   # install Python deps into .venv
```

`build_ffi_python.sh` runs `cargo build -p livekit-portal-ffi`, drops the
platform cdylib (`liblivekit_portal_ffi.{dylib,so,dll}`) next to the
Python package, and emits the matching UniFFI Python module. First build
takes a couple of minutes. Subsequent builds are incremental.

### Use from another project

After the native build, depend on the package by path. The
[shipped examples](../examples/python/basic/pyproject.toml) do this with
relative paths because they sit inside the repo. From another project,
use an absolute path:

```bash
# uv
uv add --editable /absolute/path/to/livekit-portal/python/packages/livekit-portal

# pip
pip install -e /absolute/path/to/livekit-portal/python/packages/livekit-portal
```

Or wire it directly into your `pyproject.toml`:

```toml
[project]
dependencies = ["livekit-portal"]

[tool.uv.sources]
livekit-portal = { path = "/absolute/path/to/livekit-portal/python/packages/livekit-portal", editable = true }
```

Rerun `bash scripts/build_ffi_python.sh release` (in the Portal repo)
whenever the Rust code changes. The editable install picks up the new
cdylib on next import. Prebuilt wheels are on the roadmap.

## 2. Mint tokens

Both sides need a JWT for the same LiveKit room. Minimal helper:

```python
import datetime
from livekit import api
from livekit.protocol.room import RoomConfiguration

def mint(identity: str, room: str, key: str, secret: str) -> str:
    grants = api.VideoGrants(
        room_join=True,
        room=room,
        can_publish=True,
        can_subscribe=True,
        # `Robot` and `Operator` self-set the `lk.portal.role` attribute on
        # connect so other participants can discover them. The grant must
        # include this; tokens that omit it fail at connect with a clear
        # error.
        can_update_own_metadata=True,
    )
    return (
        api.AccessToken(key, secret)
        .with_identity(identity)
        .with_grants(grants)
        # tight playout delay bounds minimize teleop latency
        .with_room_config(
            RoomConfiguration(name=room, min_playout_delay=0, max_playout_delay=1)
        )
        .with_ttl(datetime.timedelta(hours=6))
        .to_jwt()
    )
```

Identities must be unique within the room. The robot is a singleton so
`"robot"` is fine; operators get their own free-form identity per
participant (e.g. `"policy-v1"`, `"binh-teleop"`, `"supervisor-ui"`).

## 3. Robot host

Runs next to the hardware.

It declares what it will publish (video tracks, state fields) and what it
will receive (action fields). Then it pumps frames and state at your
capture rate.

```python
import asyncio, time
from livekit.portal import DType, Robot, RobotConfig

async def main():
    cfg = RobotConfig("session-1")
    cfg.add_video("cam1")
    cfg.add_state_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.add_action_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.set_fps(30)

    portal = Robot(cfg)

    def on_action(a):
        # a.values is the action dict.
        # a.timestamp_us is the sender's clock.
        # a comes from whichever operator currently holds control. Other
        # operators in the room are silently dropped at the gate.
        robot.send_action(a.values)

    portal.on_action(on_action)
    await portal.connect(URL, mint("robot", "session-1", API_KEY, API_SECRET))

    while running:
        obs = robot.get_observation()
        ts = int(time.time() * 1_000_000)
        portal.send_video_frame("cam1", obs.image, width, height, timestamp_us=ts)
        portal.send_state(obs.state, timestamp_us=ts)
        await asyncio.sleep(1 / 30)

asyncio.run(main())
```

`obs.image` must be a NumPy `uint8` array of shape `(H, W, 3)` in RGB.

## 4. Control host

Runs wherever your operator, trainer, or policy lives.

It declares the same schema as the robot host. Then it consumes
synchronized observations and publishes actions.

```python
import asyncio
from livekit.portal import DType, Operator, OperatorConfig

async def main():
    cfg = OperatorConfig("session-1")
    cfg.add_video("cam1")
    cfg.add_state_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.add_action_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.set_fps(30)

    portal = Operator(cfg)

    def on_observation(obs):
        # obs.frames: dict[str, np.ndarray]      # one per registered video track
        # obs.state:  dict[str, float]
        # obs.timestamp_us: int                  # sender clock
        action = policy(obs)                     # or teleop.get_action(), etc.
        portal.send_action(action)

    portal.on_observation(on_observation)
    await portal.connect(URL, mint("policy-v1", "session-1", API_KEY, API_SECRET))

    # Robot starts with `active_operator=None` and drops every action.
    # Self-claim so this operator's actions are accepted. In a HITL setup
    # a human or supervisor could later call
    # `await portal.set_active_operator("human-id")` to preempt.
    await portal.set_active_operator(portal.local_identity())

    while running:
        await asyncio.sleep(1)

asyncio.run(main())
```

`policy(obs)` here is any function that turns an observation into an
action dict. Teleoperation, imitation learning, VLA inference, a hand-written
P controller: Portal does not care.

## 5. Try the shipped examples

Before wiring Portal into your real stack, run the basic example. It uses
the exact API above, with synthetic video and a token minter already wired
up.

- [`examples/python/basic/`](../examples/python/basic): no hardware needed.
  Ten-minute sanity check that your LiveKit credentials and native build
  work.
- [`examples/python/so101/`](../examples/python/so101): real hardware. Drive
  a physical SO-101 follower from a remote SO-101 leader, with the camera
  and joint state rendered in [rerun](https://rerun.io). Uses the lerobot
  plugin shortcut (see below). Full calibration + wiring walkthrough in its
  [README](../examples/python/so101/README.md).

## Shortcut: lerobot users

Already using the [lerobot](https://github.com/huggingface/lerobot)
`Robot` / `Teleoperator` interfaces? Two optional plugin packages wrap
the Portal code above so you don't have to write it yourself.

```python
# robot host: wraps a local lerobot Robot
from lerobot_teleoperator_livekit import LiveKitTeleoperator, LiveKitTeleoperatorConfig

teleop = LiveKitTeleoperator(
    LiveKitTeleoperatorConfig(url=URL, token=token, session="session-1", fps=30),
    robot=my_robot,
)
teleop.connect()
```

```python
# control host: wraps a local lerobot Teleoperator (or a policy)
from lerobot_robot_livekit import LiveKitRobot, LiveKitRobotConfig

robot = LiveKitRobot(
    LiveKitRobotConfig(
        url=URL, token=token, session="session-1", fps=30,
        camera_names=("cam1",), camera_height=480, camera_width=640,
    ),
    teleop=my_leader,
)
robot.connect()
```

The plugins are syntactic sugar over the Portal API above. Full reference
and CLI mode: [lerobot integration](lerobot.md).

## Next steps

- [Portal API](portal-api.md). The full surface. All callbacks, send
  methods, role semantics.
- [Concepts](concepts.md). Roles, the observation model, frame format.
- [Config from YAML](config-file.md). Build the same configs from a
  shareable file so the wire contract lives in one place.
- [Tuning](tuning.md). `fps`, `slack`, `tolerance`, asymmetric rates,
  reliability.
