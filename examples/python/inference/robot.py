"""Inference-side robot: publishes camera + state, consumes action chunks.

This mirrors a real VLA loop: the robot streams frames + joint state at FPS
to a remote policy. The policy emits an `action chunk` (a horizon of future
actions) per inference step; this script unrolls the chunk locally and
applies one timestep per tick until the next chunk arrives.

Run alongside `policy.py`:

    cp .env.example .env
    uv sync
    uv run robot.py        # terminal 1
    uv run policy.py       # terminal 2

Reports live policy latency (`metrics.policy.e2e_us_p50/p95`) — the actual
observation→action delay the robot perceives, not just network ping.
"""
from __future__ import annotations

import asyncio
import math
import time
from typing import Optional

import numpy as np

from livekit.portal import (
    ActionChunk,
    DType,
    Robot,
    RobotConfig,
)
from _common import env_float, env_int, fmt_us, load_env, mint_token, required_env

IDENTITY = "robot"
TRACK_NAME = "cam1"

# State + action share a 4-DoF joint schema. Keeping the same fields on
# both sides is the contract — Portal fingerprints the schema and silently
# drops mismatches.
JOINT_FIELDS = [
    ("j1", DType.F32),
    ("j2", DType.F32),
    ("j3", DType.F32),
    ("j4", DType.F32),
]


def _make_frame(width: int, height: int, phase: float) -> np.ndarray:
    """Cheap moving test pattern. Not the focus of this example."""
    x = np.arange(width, dtype=np.float32) / width
    y = np.arange(height, dtype=np.float32)[:, None] / height
    two_pi = 2.0 * math.pi
    r = (0.5 + 0.5 * np.sin(two_pi * (x + phase))) * 255.0
    g = (0.5 + 0.5 * np.sin(two_pi * (y + phase * 0.7))) * 255.0
    b = (0.5 + 0.5 * np.sin(two_pi * (x * 0.5 + y * 0.5 + phase * 1.3))) * 255.0
    return np.stack(
        [
            np.broadcast_to(r, (height, width)),
            np.broadcast_to(g, (height, width)),
            b,
        ],
        axis=-1,
    ).astype(np.uint8)


class ChunkPlayer:
    """Tiny helper: hold the latest chunk and yield one timestep per tick.

    Real VLA stacks usually want this same shape — the policy emits a
    horizon, the robot unrolls until the next chunk lands and overrides.
    """

    def __init__(self) -> None:
        self._chunk: Optional[ActionChunk] = None
        self._cursor = 0
        # Wall-clock receive time of the current chunk, for "chunk age" logging.
        self._received_at: Optional[float] = None

    def push(self, chunk: ActionChunk) -> None:
        self._chunk = chunk
        self._cursor = 0
        self._received_at = time.monotonic()

    def step(self) -> Optional[dict]:
        """Return the next per-timestep action, or None if no chunk yet
        or the current chunk is exhausted."""
        if self._chunk is None or self._cursor >= self._chunk.horizon:
            return None
        action = {
            field: float(column[self._cursor])
            for field, column in self._chunk.data.items()
        }
        self._cursor += 1
        return action

    def age_ms(self) -> Optional[float]:
        if self._received_at is None:
            return None
        return (time.monotonic() - self._received_at) * 1000.0


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    token = mint_token(IDENTITY, room)
    fps = env_int("PORTAL_FPS", 30)
    horizon = env_int("PORTAL_HORIZON", 20)
    duration = env_float("PORTAL_DURATION_SECONDS", 20.0)

    cfg = RobotConfig(room)
    cfg.add_video(TRACK_NAME)
    cfg.add_state_typed(JOINT_FIELDS)
    cfg.add_action_chunk("act", horizon=horizon, fields=JOINT_FIELDS)
    cfg.set_fps(fps)

    robot_portal = Robot(cfg)
    player = ChunkPlayer()

    chunks_received = 0

    def on_chunk(chunk: ActionChunk) -> None:
        nonlocal chunks_received
        chunks_received += 1
        player.push(chunk)

    robot_portal.on_action_chunk("act", on_chunk)

    print(f"[robot] connecting to {url} as '{IDENTITY}' in room '{room}' ...")
    await robot_portal.connect(url, token)
    print(
        f"[robot] connected; streaming {fps} fps for {duration:.0f}s, "
        f"playing back chunks of horizon {horizon}"
    )

    n_ticks = int(duration * fps)
    interval = 1.0 / fps
    start = time.monotonic()
    next_tick = start
    last_log = start

    try:
        for i in range(n_ticks):
            phase = i / fps
            ts_us = int(time.time() * 1_000_000)

            # Publish a frame and current joint state. The state's
            # timestamp_us becomes the operator's `obs.timestamp_us`,
            # which the policy passes back as `in_reply_to_ts_us` —
            # closing the e2e latency loop.
            robot_portal.send_video_frame(
                TRACK_NAME, _make_frame(320, 240, phase), timestamp_us=ts_us
            )
            robot_portal.send_state(
                {
                    "j1": math.sin(phase),
                    "j2": math.cos(phase),
                    "j3": 0.1 * math.sin(phase * 2),
                    "j4": 0.0,
                },
                timestamp_us=ts_us,
            )

            # Step the chunk player. In a real system this is where you'd
            # hand the joint commands off to your servo loop.
            cmd = player.step()
            if cmd is None:
                # No policy output yet, or chunk exhausted — hold position.
                pass

            now = time.monotonic()
            if now - last_log >= 1.0:
                m = robot_portal.metrics()
                age = player.age_ms()
                age_str = "-" if age is None else f"{age:.0f}ms"
                print(
                    f"[robot] t={i // fps:>2}s "
                    f"chunks={chunks_received} chunk_age={age_str} "
                    f"active={robot_portal.active_operator()} "
                    f"e2e={fmt_us(m.policy.e2e_us_p50)}/{fmt_us(m.policy.e2e_us_p95)} "
                    f"(p50/p95) correlated={m.policy.correlated_received} "
                    f"rtt={fmt_us(m.rtt.rtt_us_last)}"
                )
                last_log = now

            next_tick += interval
            sleep_for = next_tick - time.monotonic()
            if sleep_for > 0:
                await asyncio.sleep(sleep_for)

        m = robot_portal.metrics()
        print()
        print("[robot] final policy metrics:")
        print(f"  e2e_us_p50:           {fmt_us(m.policy.e2e_us_p50)}")
        print(f"  e2e_us_p95:           {fmt_us(m.policy.e2e_us_p95)}")
        print(f"  correlated_received:  {m.policy.correlated_received}")
        print(f"  action_chunks_recv:   {m.transport.action_chunks_received}")
        print(f"  chunk_jitter:         {fmt_us(m.transport.action_chunk_jitter_us)}")
    finally:
        print("[robot] disconnecting...")
        await robot_portal.disconnect()
        robot_portal.close()


if __name__ == "__main__":
    asyncio.run(main())
