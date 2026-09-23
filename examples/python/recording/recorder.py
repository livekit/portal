"""Record everything in the room to a Rerun `.rrd` archive.

An observer sees the robot's state and video and the actions the robot
executes, plus every keypoint. It never drives. Run it next to
`../basic/robot.py` and `../basic/teleoperator.py`.

Usage:
    cp .env.example .env  # fill in API_KEY / API_SECRET
    uv run recorder.py
"""
from __future__ import annotations

import asyncio
import os
import pathlib

from livekit.portal import Observer, ObserverConfig
from livekit.portal.recording import RrdSink
from _common import env_float, load_env, mint_token, required_env

IDENTITY = "recorder"
CONFIG_PATH = pathlib.Path(__file__).parent / "portal.yaml"


async def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    out_dir = os.environ.get("PORTAL_RECORDING_DIR", "data/sessions")
    duration = env_float("PORTAL_DURATION_SECONDS", 30.0)

    obs = Observer(ObserverConfig.from_yaml_file(CONFIG_PATH, room))
    obs.on_keypoint(lambda kp: print(f"[recorder] keypoint {kp.type} {kp.payload} from {kp.sender}"))

    await obs.connect(url, mint_token(IDENTITY, room))
    sink = RrdSink(out_dir)
    obs.record_to(sink)
    print(f"[recorder] recording to {sink.path} for {duration:.0f}s")

    try:
        for _ in range(int(duration / 2)):
            await asyncio.sleep(2)
            m = obs.metrics()
            print(
                f"[recorder] frames written={m.observer.frames_written} "
                f"dropped={m.observer.frames_dropped} states={m.transport.states_received} "
                f"clock offset={m.time_sync.offset_us / 1000:.1f}ms"
            )
    finally:
        await obs.disconnect()  # writes what is queued, then closes the file
    print(f"[recorder] done: rerun {sink.path}")


if __name__ == "__main__":
    asyncio.run(main())
