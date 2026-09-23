"""Hand control between operators and mark each take with keypoints.

An observer never drives, but it may move the active operator. Every
`PORTAL_TAKE_SECONDS` this one hands control to the next operator in the room
and brackets the take with `recording` / `idle` keypoints, which the recorder
writes into the archive on the robot's clock.

Usage:
    cp .env.example .env  # fill in API_KEY / API_SECRET
    uv run orchestrator.py
"""
from __future__ import annotations

import asyncio
import itertools
import pathlib

from livekit.portal import Observer, ObserverConfig
from _common import env_float, load_env, mint_token, required_env

IDENTITY = "orchestrator"
CONFIG_PATH = pathlib.Path(__file__).parent / "portal.yaml"


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    take_s = env_float("PORTAL_TAKE_SECONDS", 5.0)
    duration = env_float("PORTAL_DURATION_SECONDS", 30.0)

    cfg = ObserverConfig.from_yaml_file(CONFIG_PATH, room)
    cfg.set_action_subscription("none")  # this observer only steers
    obs = Observer(cfg)
    obs.on_active_operator_changed(lambda who: print(f"[orchestrator] active operator: {who}"))
    await obs.connect(url, mint_token(IDENTITY, room))

    try:
        for take in itertools.count(1):
            if take * take_s > duration:
                break
            operators = obs.operators()
            if obs.robot_identity() is None or not operators:
                print("[orchestrator] waiting for the robot and an operator")
                await asyncio.sleep(1)
                continue
            driver = operators[(take - 1) % len(operators)]
            await obs.set_active_operator(driver)
            recorders = await obs.send_keypoint(
                "recording", {"task_description": f"take {take} by {driver}"}
            )
            if not recorders:
                print("[orchestrator] no observer is recording this take")
            await asyncio.sleep(take_s)
            await obs.send_keypoint("idle")
        if obs.robot_identity() is not None:
            await obs.set_active_operator(None)  # take control from everyone
    finally:
        await obs.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
