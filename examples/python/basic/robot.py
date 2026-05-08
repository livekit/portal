"""End-to-end test robot.

Reads LIVEKIT_URL + LIVEKIT_API_KEY + LIVEKIT_API_SECRET from .env (or env),
mints a token for identity=robot, joins room=LIVEKIT_ROOM, publishes one
video track ("cam1") and one state stream with a mixed-dtype schema
(three F32 joints, a BOOL gripper, an I8 mode) at PORTAL_FPS. Prints any
action it receives from the operator (`action.values` is already
typed per the declared schema). Runs for PORTAL_DURATION_SECONDS
(default 30) then cleanly disconnects.

Usage:
    cp .env.example .env   # fill in API_KEY / API_SECRET
    uv run robot.py
"""
from __future__ import annotations

import asyncio
import math
import time

import numpy as np

from livekit.portal import Action, DType, Robot, RobotConfig, RpcInvocationData
from _common import _dump_metrics, env_float, env_int, load_env, mint_token, periodic_metrics, required_env

IDENTITY = "robot"
TRACK_NAME = "cam1"

# Mixed-dtype schema: three float joints, a gripper bool, and a discrete
# control mode. Mirrors the typical shape of a real robot's observation /
# action vector — joints as floats, gripper as a binary signal, mode as a
# small enum. Both sides (robot + teleoperator) must declare the same
# schema in the same order.
STATE_SCHEMA = [
    ("j1", DType.F32),
    ("j2", DType.F32),
    ("j3", DType.F32),
    ("gripper", DType.BOOL),
    ("mode", DType.I8),
]
STATE_FIELDS = [name for name, _ in STATE_SCHEMA]


_DOT_COLOR = np.array([255, 255, 255], dtype=np.uint8)


def _make_frame(width: int, height: int, phase: float) -> np.ndarray:
    """High-entropy test pattern stressing the encoder like a real camera feed.

    Full-screen moving sinusoidal gradients on R and G channels, plus a moving
    white dot to visually confirm sync. Every pixel changes every frame, so
    the encoder can't coast on inter-frame prediction. Cheap to generate but
    gives a realistic bitrate workload.

    Returns (H, W, 3) uint8 RGB.
    """
    x = np.arange(width, dtype=np.float32) / width
    y = np.arange(height, dtype=np.float32)[:, None] / height
    two_pi = 2.0 * math.pi
    r = (0.5 + 0.5 * np.sin(two_pi * (x + phase))) * 255.0
    g = (0.5 + 0.5 * np.sin(two_pi * (y + phase * 0.7))) * 255.0
    b = (0.5 + 0.5 * np.sin(two_pi * (x * 0.5 + y * 0.5 + phase * 1.3))) * 255.0

    r_full = np.broadcast_to(r, (height, width))
    g_full = np.broadcast_to(g, (height, width))
    frame = np.stack([r_full, g_full, b], axis=-1).astype(np.uint8)

    # Moving dot overlay: completes one orbit per 2s. Gives the operator a
    # clear visual anchor for sync without meaningfully changing entropy.
    radius = min(width, height) // 3
    cx = width // 2 + int(radius * math.cos(math.pi * phase))
    cy = height // 2 + int(radius * math.sin(math.pi * phase))
    size = max(4, min(width, height) // 20)
    y0, y1 = max(0, cy - size), min(height, cy + size)
    x0, x1 = max(0, cx - size), min(width, cx + size)
    frame[y0:y1, x0:x1] = _DOT_COLOR
    return frame


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    token = mint_token(IDENTITY, room)
    fps = env_int("PORTAL_FPS", 30)
    width = env_int("PORTAL_FRAME_WIDTH", 320)
    height = env_int("PORTAL_FRAME_HEIGHT", 240)
    duration = env_float("PORTAL_DURATION_SECONDS", 30.0)

    cfg = RobotConfig(room)
    cfg.add_video(TRACK_NAME)
    cfg.add_state_typed(STATE_SCHEMA)
    cfg.add_action_typed(STATE_SCHEMA)
    cfg.set_fps(fps)

    robot_portal = Robot(cfg)

    actions_received = 0

    def on_action(action: Action) -> None:
        nonlocal actions_received
        actions_received += 1
        if actions_received % max(1, fps) == 0:
            # `action.values` is a typed dict per the declared schema:
            # gripper→bool, mode→int, joints→float. `action.raw_values`
            # is the underlying Dict[str, float] if you need to write
            # into a numpy buffer without per-field casting.
            print(
                f"[robot] action #{actions_received}: ts={action.timestamp_us} "
                f"values={action.values} from={robot_portal.active_operator()}"
            )

    robot_portal.on_action(on_action)

    # Multi-controller awareness: log when an operator joins/leaves and when
    # control changes hands. Useful for HITL scenarios with several operators
    # in the room. None of these callbacks are required for the basic loop.
    robot_portal.on_operator_joined(lambda i: print(f"[robot] operator joined: {i}"))
    robot_portal.on_operator_left(lambda i: print(f"[robot] operator left: {i}"))
    robot_portal.on_active_operator_changed(
        lambda i: print(f"[robot] active operator now: {i}")
    )

    def say(data: RpcInvocationData) -> str:
        print(f"[robot] operator says: {data.payload}")
        return "ok"

    robot_portal.register_rpc_method("say", say)

    print(f"[robot] connecting to {url} as '{IDENTITY}' in room '{room}' ...")
    await robot_portal.connect(url, token)
    print(f"[robot] connected; streaming at {fps} fps for {duration:.0f}s")

    metrics_task = asyncio.create_task(periodic_metrics(robot_portal, "[robot]", interval=2.0))

    try:
        n_frames = int(duration * fps)
        interval = 1.0 / fps
        start = time.monotonic()
        next_tick = start
        for i in range(n_frames):
            phase = i / fps  # seconds
            frame = _make_frame(width, height, phase)
            ts_us = int(time.time() * 1_000_000)
            robot_portal.send_video_frame(TRACK_NAME, frame, timestamp_us=ts_us)
            # Send Python-native values; the publisher casts to the declared
            # dtype at the wire boundary. `gripper=True` becomes one byte,
            # `mode=2` becomes one signed byte.
            robot_portal.send_state(
                {
                    "j1": math.sin(phase),
                    "j2": math.cos(phase),
                    "j3": 0.1 * phase,
                    "gripper": int(phase) % 2 == 0,
                    "mode": int(phase) % 3,
                },
                timestamp_us=ts_us,
            )
            next_tick += interval
            sleep_for = next_tick - time.monotonic()
            if sleep_for > 0:
                await asyncio.sleep(sleep_for)

        _dump_metrics("[robot]", robot_portal.metrics())
    finally:
        metrics_task.cancel()
        try:
            await metrics_task
        except asyncio.CancelledError:
            pass
        print("[robot] disconnecting...")
        await robot_portal.disconnect()
        robot_portal.close()


if __name__ == "__main__":
    asyncio.run(main())
