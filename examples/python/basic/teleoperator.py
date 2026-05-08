"""End-to-end test teleoperator.

Mirrors `robot.py` but in the Operator role. subscribes to the robot's video
and state, receives synchronized observations via `on_observation`, and
echoes a synthetic action back to the robot at `PORTAL_FPS`.

Usage:
    cp .env.example .env  # fill in API_KEY / API_SECRET
    uv run teleoperator.py
"""
from __future__ import annotations

import asyncio
import math
import time

from livekit.portal import DType, Observation, Operator, OperatorConfig
from _common import _dump_metrics, env_float, env_int, load_env, mint_token, periodic_metrics, required_env

IDENTITY = "teleoperator"
TRACK_NAME = "cam1"

# Must match `robot.py`'s schema exactly: same field order, same dtypes.
# Any disagreement (reorder, rename, dtype flip) changes the schema
# fingerprint and the peer's packets are dropped with a warning.
STATE_SCHEMA = [
    ("j1", DType.F32),
    ("j2", DType.F32),
    ("j3", DType.F32),
    ("gripper", DType.BOOL),
    ("mode", DType.I8),
]
STATE_FIELDS = [name for name, _ in STATE_SCHEMA]


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    token = mint_token(IDENTITY, room)
    fps = env_int("PORTAL_FPS", 30)
    duration = env_float("PORTAL_DURATION_SECONDS", 30.0)

    cfg = OperatorConfig(room, identity=IDENTITY)
    cfg.add_video(TRACK_NAME)
    cfg.add_state_typed(STATE_SCHEMA)
    cfg.add_action_typed(STATE_SCHEMA)
    cfg.set_fps(fps)

    op = Operator(cfg)

    observations = 0
    drops = 0
    last_log = time.monotonic()

    def on_observation(obs: Observation) -> None:
        nonlocal observations, last_log
        observations += 1
        now = time.monotonic()
        if now - last_log >= 1.0:
            frame = obs.frames.get(TRACK_NAME)
            frame_desc = f"{frame.width}x{frame.height}" if frame else "none"
            # `obs.state` is typed per the declared schema: gripper→bool,
            # mode→int, joints→float. `obs.raw_state` is the underlying
            # Dict[str, float] escape hatch.
            print(
                f"[operator] obs #{observations}: ts={obs.timestamp_us} "
                f"state={obs.state} frame={frame_desc}"
            )
            last_log = now

    def on_drop(dropped: list[dict]) -> None:
        nonlocal drops
        drops += len(dropped)
        print(f"[operator] {len(dropped)} state(s) dropped (total {drops})")

    op.on_observation(on_observation)
    op.on_drop(on_drop)

    print(f"[operator] connecting to {url} as '{IDENTITY}' in room '{room}' ...")
    await op.connect(url, token)
    print(f"[operator] connected; echoing actions at {fps} fps for {duration:.0f}s")

    # Self-claim control. The robot only accepts actions whose sender matches
    # `active_operator`, and the default is `None` (drop all). In a HITL or
    # multi-operator setup a supervisor or another operator might do this
    # call; here we are the only operator, so claim ourselves.
    await op.set_active_operator(op.local_identity())
    print(f"[operator] claimed control as '{op.local_identity()}'")

    metrics_task = asyncio.create_task(periodic_metrics(op, "[operator]", interval=2.0))

    await op.perform_rpc("say", payload="hello from the operator")

    try:
        # Echo a synthetic action back to the robot at the configured rate.
        interval = 1.0 / fps
        n_ticks = int(duration * fps)
        start = time.monotonic()
        next_tick = start
        for i in range(n_ticks):
            phase = i / fps
            ts_us = int(time.time() * 1_000_000)
            # Send Python floats; the publisher casts bool/int fields at
            # the wire boundary. `mode=5` fits in I8; out-of-range values
            # would saturate and log once per field.
            op.send_action(
                {
                    "j1": 0.5 * math.sin(phase * 2),
                    "j2": 0.5 * math.cos(phase * 2),
                    "j3": 0.0,
                    "gripper": int(phase) % 2 == 0,
                    "mode": i % 4,
                },
                timestamp_us=ts_us,
            )
            next_tick += interval
            sleep_for = next_tick - time.monotonic()
            if sleep_for > 0:
                await asyncio.sleep(sleep_for)

        print(f"[operator] observations={observations} drops={drops}")
        _dump_metrics("[operator]", op.metrics())
    finally:
        metrics_task.cancel()
        try:
            await metrics_task
        except asyncio.CancelledError:
            pass
        print("[operator] disconnecting...")
        await op.disconnect()
        op.close()


if __name__ == "__main__":
    asyncio.run(main())
