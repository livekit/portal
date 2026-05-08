"""Same as `robot.py`, but loads the wire contract from `portal.yaml`.

The schema (state / action fields, video tracks, fps) lives in the YAML
so robot and operator share one file. Only role, session, and runtime
behavior live in code.

Usage:
    cp .env.example .env   # fill in API_KEY / API_SECRET
    uv run robot_yaml.py
"""
from __future__ import annotations

import asyncio
import math
import pathlib
import time

from livekit.portal import Action, Robot, RobotConfig, RpcInvocationData
from _common import _dump_metrics, env_float, env_int, load_env, mint_token, periodic_metrics, required_env
from robot import _make_frame  # reuse the shared test-pattern generator

IDENTITY = "robot"
TRACK_NAME = "cam1"
CONFIG_PATH = pathlib.Path(__file__).parent / "portal.yaml"


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    token = mint_token(IDENTITY, room)
    width = env_int("PORTAL_FRAME_WIDTH", 320)
    height = env_int("PORTAL_FRAME_HEIGHT", 240)
    duration = env_float("PORTAL_DURATION_SECONDS", 30.0)

    # The whole declarative surface comes from YAML. `session` (the room
    # name) is supplied here; everything else is in the file.
    cfg = RobotConfig.from_yaml_file(CONFIG_PATH, room)
    fps = env_int("PORTAL_FPS", 30)  # matches the YAML default

    robot_portal = Robot(cfg)

    actions_received = 0

    def on_action(action: Action) -> None:
        nonlocal actions_received
        actions_received += 1
        if actions_received % max(1, fps) == 0:
            print(
                f"[robot] action #{actions_received}: ts={action.timestamp_us} "
                f"values={action.values} from={robot_portal.active_operator()}"
            )

    robot_portal.on_action(on_action)

    def say(data: RpcInvocationData) -> str:
        print(f"[robot] operator says: {data.payload}")
        return "ok"

    robot_portal.register_rpc_method("say", say)

    print(f"[robot] connecting to {url} as '{IDENTITY}' in room '{room}' ...")
    print(f"[robot] config loaded from {CONFIG_PATH}")
    await robot_portal.connect(url, token)
    print(f"[robot] connected; streaming at {fps} fps for {duration:.0f}s")

    metrics_task = asyncio.create_task(periodic_metrics(robot_portal, "[robot]", interval=2.0))

    try:
        n_frames = int(duration * fps)
        interval = 1.0 / fps
        start = time.monotonic()
        next_tick = start
        for i in range(n_frames):
            phase = i / fps
            frame = _make_frame(width, height, phase)
            ts_us = int(time.time() * 1_000_000)
            robot_portal.send_video_frame(TRACK_NAME, frame, timestamp_us=ts_us)
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
