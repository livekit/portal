"""Inference-side policy: subscribes to obs, plans a horizon, streams actions.

Stand-in for a real VLA. Each inference round:
  - takes the freshest observation
  - simulates inference latency (configurable)
  - produces a horizon of future actions, shape `(horizon, 4)`

The horizon stays on this side. A control loop steps through it at the
robot's fps and sends one `send_action` per tick, each tagged with the
observation it came from via `in_reply_to_ts_us`. The next inference round
replaces whatever is left of the current horizon.

Run this against a `robot.py` in the same room (see `robot.py`'s docstring).
"""
from __future__ import annotations

import asyncio
import math
import time
from collections import deque
from dataclasses import dataclass
from typing import Deque, Optional

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
JOINT_NAMES = [name for name, _ in JOINT_FIELDS]


@dataclass
class Plan:
    """A horizon of actions and the observation it was planned from."""

    actions: np.ndarray
    obs_ts_us: int
    cursor: int = 0

    def step(self) -> Optional[dict]:
        """Next per-tick action, or None once the horizon is used up."""
        if self.cursor >= len(self.actions):
            return None
        row = self.actions[self.cursor]
        self.cursor += 1
        return {name: float(v) for name, v in zip(JOINT_NAMES, row)}


def _fake_inference(
    obs: Observation, horizon: int, latency_ms: float
) -> np.ndarray:
    """Return a `(horizon, 4)` float32 array. Stand-in for a VLA forward
    pass: we burn `latency_ms` of wall time to simulate inference, then
    return a smooth horizon shaped from the current joint angles.

    A real policy would feed `obs.frames["cam1"]` plus `obs.state` into
    its model and return the model's action horizon.
    """
    if latency_ms > 0:
        # Block-sleep deliberately: inference is CPU/GPU-bound. The caller
        # runs this in a worker thread so the control loop keeps ticking.
        time.sleep(latency_ms / 1000.0)

    j1 = obs.state["j1"]
    j2 = obs.state["j2"]
    # Project a smooth horizon: continue a small forward sinusoid.
    t = np.arange(horizon, dtype=np.float32) / horizon
    actions = np.zeros((horizon, 4), dtype=np.float32)
    actions[:, 0] = j1 + 0.1 * np.sin(t * math.pi)
    actions[:, 1] = j2 + 0.1 * np.cos(t * math.pi)
    actions[:, 2] = 0.05 * np.sin(t * 2 * math.pi)
    actions[:, 3] = 0.0
    return actions


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    token = mint_token(IDENTITY, room)
    fps = env_int("PORTAL_FPS", 30)
    horizon = env_int("PORTAL_HORIZON", 20)
    duration = env_float("PORTAL_DURATION_SECONDS", 20.0)
    inference_latency_ms = env_float("PORTAL_INFERENCE_LATENCY_MS", 30.0)
    inference_hz = env_float("PORTAL_INFERENCE_HZ", 5.0)

    cfg = OperatorConfig(room)
    cfg.add_video(TRACK_NAME)
    cfg.add_state_typed(JOINT_FIELDS)
    cfg.add_action_typed(JOINT_FIELDS)
    cfg.set_fps(fps)

    op = Operator(cfg)

    # Inference is too slow to run inside the obs callback, which would
    # block the receive loop. Keep the latest few obs in a bounded deque
    # and consume them from the inference task.
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
        f"[policy] connected; planning horizon={horizon} at {inference_hz:.1f} Hz, "
        f"stepping at {fps} fps, simulated inference {inference_latency_ms:.0f}ms"
    )

    # Self-claim control. Without this the robot drops every action because
    # `active_operator` defaults to None. In an HITL setup a human could
    # later preempt with `await op.set_active_operator("human-id")`.
    await op.set_active_operator(op.local_identity())
    print(f"[policy] claimed control as '{op.local_identity()}'")

    plan: Optional[Plan] = None
    plans_made = 0
    actions_sent = 0
    stop_at = time.monotonic() + duration

    async def inference_loop() -> None:
        nonlocal plan, plans_made
        interval = 1.0 / inference_hz
        while time.monotonic() < stop_at:
            await obs_event.wait()
            obs_event.clear()
            obs = obs_queue[-1]  # always plan from the freshest observation
            started = time.monotonic()
            actions = await asyncio.to_thread(
                _fake_inference, obs, horizon, inference_latency_ms
            )
            plan = Plan(actions=actions, obs_ts_us=obs.timestamp_us)
            plans_made += 1
            await asyncio.sleep(max(0.0, interval - (time.monotonic() - started)))

    inference_task = asyncio.create_task(inference_loop())

    tick = 1.0 / fps
    next_tick = time.monotonic()
    last_log = next_tick

    try:
        while time.monotonic() < stop_at:
            cmd = plan.step() if plan is not None else None
            if cmd is not None:
                # The crucial argument: `in_reply_to_ts_us` closes the e2e
                # latency loop. The robot computes `now - obs.timestamp_us`
                # and feeds it into `metrics.policy.e2e_us_*`.
                op.send_action(cmd, in_reply_to_ts_us=plan.obs_ts_us)
                actions_sent += 1

            now = time.monotonic()
            if now - last_log >= 1.0:
                m = op.metrics()
                print(
                    f"[policy] plans={plans_made} actions_sent={actions_sent} "
                    f"obs_seen={m.sync.observations_emitted} "
                    f"obs_dropped={m.sync.states_dropped}"
                )
                last_log = now

            next_tick += tick
            await asyncio.sleep(max(0.0, next_tick - time.monotonic()))
    finally:
        inference_task.cancel()
        print(
            f"[policy] made {plans_made} plans, sent {actions_sent} actions; "
            "disconnecting..."
        )
        await op.disconnect()
        op.close()


if __name__ == "__main__":
    asyncio.run(main())
