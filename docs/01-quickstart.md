# Quickstart

> Run a robot and an operator against each other in about five minutes.

You will write two files. `robot.py` publishes video and joint state.
`operator_app.py` receives them fused into observations and sends actions back.
Both run on your own machine for now, talking through a real LiveKit server.

You do not need a physical robot. The robot script publishes a synthetic test
pattern.

## Prerequisites

- **Python 3.12.** Prebuilt wheels target 3.12. The library itself supports
  3.10 and up, but on older versions you have to
  [build from source](#build-from-source).
- **A LiveKit server.** [LiveKit Cloud](https://cloud.livekit.io) has a free
  tier that works. A local `livekit-server --dev` also works.
- **Your credentials.** `LIVEKIT_URL`, `LIVEKIT_API_KEY`, and
  `LIVEKIT_API_SECRET`, from the LiveKit Cloud dashboard or your dev server.

## 1. Install

```bash
pip install livekit-portal livekit-api numpy
```

Or with [uv](https://docs.astral.sh/uv/):

```bash
uv add livekit-portal livekit-api numpy
```

`livekit-portal` is the library. `livekit-api` is only used here to mint
access tokens, which is normally a server-side job. `numpy` carries the frames.

Prebuilt wheels cover CPython 3.12 on Linux x86\_64 (glibc 2.35 and newer),
Linux aarch64 (glibc 2.39 and newer), and macOS Apple Silicon. Anything else
needs a [source build](#build-from-source).

## 2. Set your credentials

```bash
export LIVEKIT_URL="wss://your-project.livekit.cloud"
export LIVEKIT_API_KEY="APIxxxxxxxx"
export LIVEKIT_API_SECRET="xxxxxxxxxxxx"
```

## 3. Write a shared token helper

Both scripts need a JWT for the same LiveKit room. Save this as
`portal_token.py`.

> **Note.** The filenames in this guide are chosen to avoid shadowing Python
> standard library modules. Do not name these files `token.py` or `operator.py`.
> A module in your working directory takes precedence over the stdlib, and
> shadowing either of those breaks imports across the interpreter.

```python
# portal_token.py
import datetime
import os

from livekit import api
from livekit.protocol.room import RoomConfiguration

ROOM = "portal-quickstart"


def mint(identity: str) -> str:
    grants = api.VideoGrants(
        room_join=True,
        room=ROOM,
        can_publish=True,
        can_subscribe=True,
        # Required. Robot and Operator both self-set an `lk.portal.role`
        # attribute on connect so peers can discover them. Without this
        # grant, connect fails with a clear error.
        can_update_own_metadata=True,
    )
    return (
        api.AccessToken(os.environ["LIVEKIT_API_KEY"], os.environ["LIVEKIT_API_SECRET"])
        .with_identity(identity)
        .with_grants(grants)
        # Tight playout delay bounds keep teleop latency low.
        .with_room_config(
            RoomConfiguration(name=ROOM, min_playout_delay=0, max_playout_delay=1)
        )
        .with_ttl(datetime.timedelta(hours=6))
        .to_jwt()
    )
```

Identities must be unique inside a room. There is one robot per session, so
`"robot"` is fine. Operators pick their own name, like `"policy-v1"` or
`"binh-teleop"`.

> **Note.** Minting tokens with your API secret belongs on a server, not in a
> robot or a browser. It is inline here to keep the quickstart to two files.

## 4. Write the robot

This runs next to the hardware. It declares what it publishes (one camera, five
state fields) and what it accepts (the same five as actions). Then it pumps
frames and state at 30 fps.

Save it as `robot.py`.

```python
# robot.py
import asyncio
import math
import os
import time

import numpy as np
from livekit.portal import DType, Robot, RobotConfig

from portal_token import ROOM, mint

FPS = 30
WIDTH, HEIGHT = 320, 240

# Both sides must declare the same fields, in the same order, with the
# same dtypes. Mixed dtypes are normal: joints as floats, a gripper as a
# bool, a control mode as a small int.
SCHEMA = [
    ("j1", DType.F32),
    ("j2", DType.F32),
    ("j3", DType.F32),
    ("gripper", DType.BOOL),
    ("mode", DType.I8),
]


def make_frame(phase: float) -> np.ndarray:
    """A moving test pattern. Returns (H, W, 3) uint8 RGB."""
    x = np.arange(WIDTH, dtype=np.float32) / WIDTH
    y = np.arange(HEIGHT, dtype=np.float32)[:, None] / HEIGHT
    r = np.broadcast_to((0.5 + 0.5 * np.sin(2 * math.pi * (x + phase))) * 255, (HEIGHT, WIDTH))
    g = np.broadcast_to((0.5 + 0.5 * np.sin(2 * math.pi * (y + phase))) * 255, (HEIGHT, WIDTH))
    b = np.full((HEIGHT, WIDTH), 128, dtype=np.float32)
    return np.stack([r, g, b], axis=-1).astype(np.uint8)


async def main() -> None:
    cfg = RobotConfig(ROOM)
    cfg.add_video("cam1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    cfg.set_fps(FPS)

    robot = Robot(cfg)

    # Actions arrive here from whichever operator currently holds control.
    # Actions from every other operator are dropped before this fires.
    def on_action(action) -> None:
        print(f"[robot] action from {action.sender}: {action.values}")

    robot.on_action(on_action)

    # A one-shot command. Either side can register, either side can invoke.
    robot.register_rpc_method("home", lambda data: "homed")

    await robot.connect(os.environ["LIVEKIT_URL"], mint("robot"))
    print("[robot] connected")

    try:
        for i in range(FPS * 60):
            phase = i / FPS
            # One clock for both the frame and the state. This is what lets
            # the operator match them back together.
            ts = int(time.time() * 1_000_000)
            robot.send_video_frame("cam1", make_frame(phase), timestamp_us=ts)
            robot.send_state(
                {
                    "j1": math.sin(phase),
                    "j2": math.cos(phase),
                    "j3": 0.1 * phase,
                    "gripper": int(phase) % 2 == 0,
                    "mode": int(phase) % 3,
                },
                timestamp_us=ts,
            )
            await asyncio.sleep(1 / FPS)
    finally:
        await robot.disconnect()
        robot.close()


if __name__ == "__main__":
    asyncio.run(main())
```

Frames must be `uint8` NumPy arrays of shape `(H, W, 3)` in RGB order. Width
and height must both be even. See
[Concepts: frame format](02-concepts.md#video-frame-format).

## 5. Write the operator

This runs wherever your policy or teleop UI lives. It declares the same schema,
consumes fused observations, and publishes actions.

Save it as `operator_app.py`.

```python
# operator_app.py
import asyncio
import os

from livekit.portal import DType, Operator, OperatorConfig, frame_bytes_to_numpy_rgb

from portal_token import ROOM, mint

FPS = 30

# Identical to the robot's schema. Same fields, same order, same dtypes.
SCHEMA = [
    ("j1", DType.F32),
    ("j2", DType.F32),
    ("j3", DType.F32),
    ("gripper", DType.BOOL),
    ("mode", DType.I8),
]


async def main() -> None:
    cfg = OperatorConfig(ROOM)
    cfg.add_video("cam1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    cfg.set_fps(FPS)

    op = Operator(cfg)
    seen = 0

    def on_observation(obs) -> None:
        nonlocal seen
        seen += 1

        # obs.frames["cam1"] is a VideoFrameData holding packed RGB24 bytes.
        frame = obs.frames["cam1"]
        rgb = frame_bytes_to_numpy_rgb(bytes(frame.data), frame.width, frame.height)

        if seen % FPS == 0:
            print(f"[operator] obs #{seen} frame={rgb.shape} state={obs.state}")

        # Your policy goes here. This one just mirrors the state back.
        action = dict(obs.state)

        # in_reply_to_ts_us tells the robot which observation this answers,
        # which is what makes metrics.policy.e2e_us_* a real latency number
        # instead of a ping.
        op.send_action(action, in_reply_to_ts_us=obs.timestamp_us)

    op.on_observation(on_observation)

    await op.connect(os.environ["LIVEKIT_URL"], mint("policy-v1"))
    print("[operator] connected")

    # The robot starts with no active operator and drops every action.
    # Claim control so ours are accepted.
    await op.set_active_operator(op.local_identity())

    print("[operator] home ->", await op.perform_rpc("home"))

    try:
        await asyncio.sleep(60)
    finally:
        await op.disconnect()
        op.close()


if __name__ == "__main__":
    asyncio.run(main())
```

## 6. Run both

Two terminals, same directory.

```bash
python robot.py       # terminal 1
```

```bash
python operator_app.py    # terminal 2
```

The operator prints an observation roughly once a second. The robot prints the
actions coming back. If that is what you see, your credentials, your native
build, and the sync path all work.

```
[operator] connected
[operator] home -> homed
[operator] obs #30 frame=(240, 320, 3) state={'j1': 0.84, 'j2': 0.54, 'j3': 0.1, 'gripper': True, 'mode': 1}
```

You will also see a `[state-overflow]` and a `[sync-drop]` warning in the first
second or so. That is expected. State starts flowing before the video track has
warmed up, so the earliest states have nothing to match against. Once video is
running they stop.

Nothing printing at all? Go to
[Troubleshooting](08-troubleshooting.md#nothing-is-arriving).

## What just happened

The robot stamped every frame and every state packet with one clock. The
operator buffered both streams and matched them by that timestamp, then fired
`on_observation` once per matched pair. Actions went back on a separate
reliable channel, gated so only the active operator's arrive.

That gate is why step 5 calls `set_active_operator`. Without it the robot
drops everything, which is the single most common first-run surprise.

Read [Concepts](02-concepts.md) next for the model behind all of that.

## Try the shipped examples

The examples do the same thing with the rough edges sanded off, including
`.env` loading and a live metrics printout.

- [`examples/python/basic/`](../examples/python/basic). What you just built,
  plus a YAML-config variant. No hardware.
- [`examples/python/inference/`](../examples/python/inference). A VLA-shaped
  loop using action chunks and end-to-end latency metrics.
- [`examples/python/modal-mock-inference/`](../examples/python/modal-mock-inference).
  Runs the policy on [Modal](https://modal.com) and measures true
  glass-to-glass latency.
- [`examples/python/so101/`](../examples/python/so101). Real hardware. A
  physical SO-101 follower driven by a remote SO-101 leader.

## Build from source

Build from source when there is no wheel for your platform (Windows, Intel
macOS, Python 3.10 or 3.11) or when you are changing the Rust core.

You need a [Rust toolchain](https://rustup.rs/) and
[`uv`](https://docs.astral.sh/uv/).

```bash
git clone https://github.com/livekit/portal.git
cd portal

bash scripts/build_ffi_python.sh release
cd python && uv sync
```

`build_ffi_python.sh` runs `cargo build -p livekit-portal-ffi`, drops the
platform cdylib next to the Python package, and generates the UniFFI bindings.
The first build takes a few minutes. Later builds are incremental. Rerun it
whenever the Rust code changes.

To depend on that build from another project, install it by path:

```bash
uv add --editable /abs/path/to/portal/python/packages/livekit-portal
# or
pip install -e /abs/path/to/portal/python/packages/livekit-portal
```

If the cdylib lives somewhere else, point `LIVEKIT_PORTAL_FFI_LIB` at it.

## Already on lerobot?

Two optional plugin packages wrap everything above so you do not write it
yourself. Your existing lerobot `Robot` or `Teleoperator` goes in, and the
remote arm comes out looking like a local lerobot device.

```bash
pip install lerobot-teleoperator-livekit   # robot side
pip install lerobot-robot-livekit          # operator side
```

The plugins are a convenience layer over the API in this page, not a
replacement for it. Read [Concepts](02-concepts.md) first, then
[lerobot plugins](reference/lerobot.md).

## Next steps

- [Concepts](02-concepts.md). Roles, the observation model, control handoff.
- [Portal API](03-portal-api.md). The full surface.
- [Tuning](04-tuning.md). `fps`, `slack`, and `tolerance`.
- [Config from YAML](reference/config-file.md). Move the schema out of code so
  both sides load one file.
