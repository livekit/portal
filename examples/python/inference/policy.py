"""Inference-side policy: subscribes to obs, emits action chunks.

Stand-in for a real VLA. On every observation:
  - simulate inference latency (configurable)
  - emit an action chunk of shape `(horizon, 4)` correlated to the obs
    timestamp via `in_reply_to_ts_us`

The chunk goes back to the robot as a single LiveKit byte stream — no
per-timestep round trip, no 15 KB packet cap.

Run this against a `robot.py` in the same room (see `robot.py`'s docstring).
"""
from __future__ import annotations

import asyncio
import math
import os
import time
from collections import deque
from typing import Deque

import numpy as np

from livekit.portal import (
    DType,
    Observation,
    Operator,
    OperatorConfig,
)
from _common import env_float, env_int, load_env, mint_token, required_env

IDENTITY = "policy"
TRACK_NAME = "cam1"

JOINT_FIELDS = [
    ("j1", DType.F32),
    ("j2", DType.F32),
    ("j3", DType.F32),
    ("j4", DType.F32),
]


def _fake_inference(
    obs: Observation, horizon: int, latency_ms: float
) -> np.ndarray:
    """Return a `(horizon, 4)` float32 chunk. Stand-in for a VLA forward
    pass — we burn `latency_ms` of wall time to simulate inference, then
    return a smooth horizon shaped from the current joint angles.

    A real policy would feed `obs.frames["cam1"]` plus `obs.state` into
    its model and return the model's chunk output.
    """
    if latency_ms > 0:
        # Block-sleep deliberately. Inference is CPU-bound; using
        # asyncio.sleep would be a lie. Real VLAs run inference on a
        # GPU thread; we model it as wall-clock time.
        time.sleep(latency_ms / 1000.0)

    j1 = obs.state["j1"]
    j2 = obs.state["j2"]
    # Project a smooth horizon: continue a small forward sinusoid.
    t = np.arange(horizon, dtype=np.float32) / horizon
    chunk = np.zeros((horizon, 4), dtype=np.float32)
    chunk[:, 0] = j1 + 0.1 * np.sin(t * math.pi)
    chunk[:, 1] = j2 + 0.1 * np.cos(t * math.pi)
    chunk[:, 2] = 0.05 * np.sin(t * 2 * math.pi)
    chunk[:, 3] = 0.0
    return chunk


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    token = mint_token(IDENTITY, room)
    horizon = env_int("PORTAL_HORIZON", 20)
    duration = env_float("PORTAL_DURATION_SECONDS", 20.0)
    inference_latency_ms = env_float("PORTAL_INFERENCE_LATENCY_MS", 30.0)
    chunks_per_second = env_float("PORTAL_CHUNKS_PER_SECOND", 5.0)

    cfg = OperatorConfig(room, identity=IDENTITY)
    cfg.add_video(TRACK_NAME)
    cfg.add_state_typed(JOINT_FIELDS)
    cfg.add_action_chunk("act", horizon=horizon, fields=JOINT_FIELDS)
    cfg.set_fps(env_int("PORTAL_FPS", 30))

    op = Operator(cfg)

    # Inference is too slow to run synchronously inside the obs callback —
    # that would block the receive loop. Buffer the latest few obs in a
    # bounded deque and consume them from a worker.
    obs_queue: Deque[Observation] = deque(maxlen=4)
    obs_event = asyncio.Event()
    loop = asyncio.get_running_loop()

    def on_observation(obs: Observation) -> None:
        obs_queue.append(obs)
        loop.call_soon_threadsafe(obs_event.set)

    op.on_observation(on_observation)

    print(f"[policy] connecting to {url} as '{IDENTITY}' in room '{room}' ...")
    await op.connect(url, token)
    print(
        f"[policy] connected; emitting chunks horizon={horizon} "
        f"target {chunks_per_second:.1f} Hz, simulated inference {inference_latency_ms:.0f}ms"
    )

    # Self-claim control. Without this the robot drops every action chunk
    # because `active_operator` defaults to None. In an HITL setup a human
    # could later preempt with `await op.set_active_operator("human-id")`.
    await op.set_active_operator(op.local_identity())
    print(f"[policy] claimed control as '{op.local_identity()}'")

    chunks_sent = 0
    last_chunk_at = time.monotonic()
    chunk_interval = 1.0 / chunks_per_second
    stop_at = time.monotonic() + duration
    last_log = time.monotonic()

    try:
        while time.monotonic() < stop_at:
            try:
                # Wake at most once per chunk interval, or when an obs arrives.
                await asyncio.wait_for(obs_event.wait(), timeout=chunk_interval)
                obs_event.clear()
            except asyncio.TimeoutError:
                pass
            if not obs_queue:
                continue

            now = time.monotonic()
            if now - last_chunk_at < chunk_interval:
                continue

            obs = obs_queue[-1]   # always run on the freshest observation

            # Run "inference" on this obs's frame + state. Returns a
            # `(horizon, 4)` numpy array — Portal accepts that directly.
            chunk = _fake_inference(obs, horizon, inference_latency_ms)

            # The crucial line: passing in_reply_to_ts_us closes the
            # e2e latency loop. The robot side computes
            # `now_robot - obs.timestamp_us` and feeds it into
            # metrics.policy.e2e_us_*.
            op.send_action_chunk(
                "act", chunk, in_reply_to_ts_us=obs.timestamp_us
            )
            chunks_sent += 1
            last_chunk_at = now

            if now - last_log >= 1.0:
                m = op.metrics()
                print(
                    f"[policy] chunks_sent={chunks_sent} "
                    f"obs_seen={m.sync.observations_emitted} "
                    f"obs_dropped={m.sync.states_dropped}"
                )
                last_log = now
    finally:
        print(f"[policy] sent {chunks_sent} chunks; disconnecting...")
        await op.disconnect()
        op.close()


if __name__ == "__main__":
    asyncio.run(main())
