"""Shared env loading, token minting, and loop pacing for the SO-101 examples."""
from __future__ import annotations

import datetime
import os
import pathlib
import time
from typing import Iterator

from dotenv import load_dotenv
from livekit import api
from livekit.protocol.room import RoomConfiguration


def load_env() -> None:
    """Load `.env` then `.env.local` from the script directory."""
    d = pathlib.Path(__file__).parent
    for name, override in ((".env", False), (".env.local", True)):
        p = d / name
        if p.exists():
            load_dotenv(p, override=override)


def required_env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"{name} must be set (see .env.example)")
    return value


def env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    return int(raw) if raw else default


def env_str(name: str, default: str) -> str:
    return os.environ.get(name) or default


def mint_token(identity: str, room: str) -> str:
    """Mint a LiveKit JWT for `identity` in `room` with low-latency playout."""
    key = required_env("LIVEKIT_API_KEY")
    secret = required_env("LIVEKIT_API_SECRET")
    grants = api.VideoGrants(
        room_join=True,
        room=room,
        can_publish=True,
        can_subscribe=True,
        can_update_own_metadata=True,
    )
    room_cfg = RoomConfiguration(name=room, min_playout_delay=0, max_playout_delay=1)
    return (
        api.AccessToken(key, secret)
        .with_identity(identity)
        .with_grants(grants)
        .with_room_config(room_cfg)
        .with_ttl(datetime.timedelta(hours=6))
        .to_jwt()
    )


def pace(fps: int) -> Iterator[int]:
    """Yield tick indices at `fps`, sleeping between ticks. Resets on overrun."""
    interval = 1.0 / fps
    next_tick = time.monotonic()
    i = 0
    while True:
        yield i
        i += 1
        next_tick += interval
        sleep_for = next_tick - time.monotonic()
        if sleep_for > 0:
            time.sleep(sleep_for)
        else:
            next_tick = time.monotonic()
