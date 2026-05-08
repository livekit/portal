"""Shared helpers for the inference example: env + token minting.

Mirrors the smaller version in the basic example so this directory can be
read on its own. Reads LiveKit creds from `.env` (or process env) and mints
a JWT scoped to a single room.
"""
from __future__ import annotations

import datetime
import os
import pathlib
import sys
from typing import Optional

try:
    from dotenv import load_dotenv
except ImportError as exc:  # pragma: no cover
    print(
        "examples require python-dotenv. Install with:\n"
        "    uv pip install livekit-api python-dotenv",
        file=sys.stderr,
    )
    raise SystemExit(1) from exc

try:
    from livekit import api
except ImportError as exc:  # pragma: no cover
    print(
        "examples require livekit-api. Install with:\n"
        "    uv pip install livekit-api python-dotenv",
        file=sys.stderr,
    )
    raise SystemExit(1) from exc


def load_env(search_from: Optional[pathlib.Path] = None) -> None:
    start = search_from or pathlib.Path(__file__).parent
    for d in (start, start.parent, pathlib.Path.cwd()):
        env = d / ".env"
        if env.exists():
            load_dotenv(env, override=False)
        env_local = d / ".env.local"
        if env_local.exists():
            load_dotenv(env_local, override=True)


def mint_token(identity: str, room: str, ttl_hours: int = 1) -> str:
    key = os.environ.get("LIVEKIT_API_KEY")
    secret = os.environ.get("LIVEKIT_API_SECRET")
    if not key or not secret:
        raise RuntimeError(
            "LIVEKIT_API_KEY and LIVEKIT_API_SECRET must be set (see .env.example)"
        )
    grants = api.VideoGrants(
        room_join=True,
        room=room,
        can_publish=True,
        can_subscribe=True,
        # `Robot` and `Operator` self-set the `lk.portal.role` attribute on
        # connect so participants can discover one another. The grant must
        # permit it.
        can_update_own_metadata=True,
    )
    return (
        api.AccessToken(key, secret)
        .with_identity(identity)
        .with_grants(grants)
        .with_ttl(datetime.timedelta(hours=ttl_hours))
        .to_jwt()
    )


def required_env(name: str) -> str:
    v = os.environ.get(name)
    if not v:
        raise RuntimeError(f"{name} must be set (see .env.example)")
    return v


def env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    return int(raw) if raw else default


def env_float(name: str, default: float) -> float:
    raw = os.environ.get(name)
    return float(raw) if raw else default


def fmt_us(value) -> str:
    """Format µs as `NNNus` or `N.NNms`, or `-` for None / 0."""
    if value is None or value == 0:
        return "-"
    if value < 1000:
        return f"{value}us"
    return f"{value / 1000:.2f}ms"
