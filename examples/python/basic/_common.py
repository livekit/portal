"""Everything in the example that is not Portal: settings, tokens, a fake camera
and arm, pacing, and printing."""
from __future__ import annotations

import asyncio
import datetime
import math
import os
import pathlib
import time
from dataclasses import dataclass

import numpy as np
from dotenv import load_dotenv
from livekit import api

HERE = pathlib.Path(__file__).parent
CONFIG = HERE / "portal.yaml"
WIDTH, HEIGHT = 320, 240


@dataclass(frozen=True)
class Settings:
    url: str
    room: str
    duration: float
    take_seconds: float
    recording_dir: str

    @classmethod
    def from_env(cls) -> Settings:
        load_dotenv(HERE / ".env")
        return cls(
            url=os.environ["LIVEKIT_URL"],
            room=os.environ["LIVEKIT_ROOM"],
            duration=float(os.environ.get("PORTAL_DURATION_SECONDS", 30)),
            take_seconds=float(os.environ.get("PORTAL_TAKE_SECONDS", 5)),
            recording_dir=os.environ.get("PORTAL_RECORDING_DIR", "data/sessions"),
        )

    def token(self, identity: str) -> str:
        grants = api.VideoGrants(
            room_join=True,
            room=self.room,
            can_publish=True,
            can_subscribe=True,
            # Portal advertises each peer's role in its participant attributes.
            can_update_own_metadata=True,
        )
        # Zero playout delay: don't let the receiver buffer video for smoothness.
        room_config = api.RoomConfiguration(name=self.room, min_playout_delay=0, max_playout_delay=1)
        return (
            api.AccessToken(os.environ["LIVEKIT_API_KEY"], os.environ["LIVEKIT_API_SECRET"])
            .with_identity(identity)
            .with_grants(grants)
            .with_room_config(room_config)
            .with_ttl(datetime.timedelta(hours=6))
            .to_jwt()
        )


async def ticks(fps: int, duration: float):
    """Yield the tick number at `fps` for `duration` seconds."""
    next_tick = time.monotonic()
    for i in range(int(duration * fps)):
        yield i
        next_tick += 1 / fps
        await asyncio.sleep(max(0.0, next_tick - time.monotonic()))


def camera_frame(t: float) -> np.ndarray:
    """A moving gradient, (H, W, 3) uint8 RGB."""
    x = np.arange(WIDTH, dtype=np.float32) / WIDTH
    y = np.arange(HEIGHT, dtype=np.float32)[:, None] / HEIGHT
    r = np.broadcast_to(np.sin(2 * math.pi * (x + t)), (HEIGHT, WIDTH))
    g = np.broadcast_to(np.sin(2 * math.pi * (y + t * 0.7)), (HEIGHT, WIDTH))
    b = np.sin(2 * math.pi * (x + y + t * 1.3) / 2)
    return ((np.stack([r, g, b], axis=-1) + 1) * 127.5).astype(np.uint8)


def arm_state(t: float) -> dict:
    return {
        "j1": math.sin(t),
        "j2": math.cos(t),
        "j3": 0.1 * t,
        "gripper": int(t) % 2 == 0,
        "mode": int(t) % 3,
    }


def print_metrics(name: str, m) -> None:
    """The headline numbers from `metrics()`, skipping ones this role doesn't measure."""
    latencies_us = {
        "rtt_p95": m.rtt.rtt_us_p95,
        "e2e_p95": m.policy.e2e_us_p95,
        "sync_delta_p95": m.sync.match_delta_us_p95,
    }
    shown = " ".join(f"{k}={v / 1000:.1f}ms" for k, v in latencies_us.items() if v is not None)
    print(f"[{name}] {shown}")
