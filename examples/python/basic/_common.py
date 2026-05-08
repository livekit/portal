"""Shared helpers for the robot / operator example scripts.

Reads LiveKit API credentials from .env, mints a JWT for the given identity
and room. Keeps the example scripts focused on Portal usage.
"""
from __future__ import annotations

import datetime
import os
import pathlib
import sys
from typing import Optional

try:
    from dotenv import load_dotenv  # type: ignore
except ImportError as exc:  # pragma: no cover
    print(
        "examples require python-dotenv. Install with:\n"
        "    uv pip install livekit-api python-dotenv",
        file=sys.stderr,
    )
    raise SystemExit(1) from exc

try:
    from livekit import api  # type: ignore
except ImportError as exc:  # pragma: no cover
    print(
        "examples require livekit-api. Install with:\n"
        "    uv pip install livekit-api python-dotenv",
        file=sys.stderr,
    )
    raise SystemExit(1) from exc


def load_env(search_from: Optional[pathlib.Path] = None) -> None:
    """Load `.env` and `.env.local` from the script dir, its parent, or cwd.

    Files are loaded in this order (first match of each filename wins, later
    wins within the same directory so `.env.local` overrides `.env`):
      1. <script_dir>/.env         then <script_dir>/.env.local
      2. <script_dir>/../.env      then <script_dir>/../.env.local
      3. <cwd>/.env                then <cwd>/.env.local
    Non-fatal if nothing is found. caller can still rely on real env vars.
    """
    start = search_from or pathlib.Path(__file__).parent
    search_dirs = [start, start.parent, pathlib.Path.cwd()]
    loaded_any = False
    for d in search_dirs:
        env = d / ".env"
        if env.exists():
            load_dotenv(env, override=False)
            loaded_any = True
        env_local = d / ".env.local"
        if env_local.exists():
            load_dotenv(env_local, override=True)
            loaded_any = True
        if loaded_any:
            return


def mint_token(
    identity: str,
    room: str,
    ttl_hours: int = 6,
    min_playout_delay_ms: int = 0,
    max_playout_delay_ms: int = 1,
) -> str:
    """Mint a LiveKit JWT for `identity` scoped to `room`.

    Attaches a `RoomConfiguration` so the server creates the room (on first
    join) with tight playout delay bounds. Default `0..1 ms` minimizes video
    latency. matches LiveKit's recommendation for low-latency teleop. Pass
    larger bounds (e.g. `max_playout_delay_ms=150`) if you need smoother
    playback at the cost of latency.
    """
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
        # connect so other participants can discover them. The grant must
        # permit it. Tokens that omit this flag fail at connect with a clear
        # error message.
        can_update_own_metadata=True,
    )

    from livekit.protocol.room import RoomConfiguration
    room_config = RoomConfiguration(
        name=room,
        min_playout_delay=min_playout_delay_ms,
        max_playout_delay=max_playout_delay_ms,
    )

    token = (
        api.AccessToken(key, secret)
        .with_identity(identity)
        .with_grants(grants)
        .with_room_config(room_config)
        .with_ttl(datetime.timedelta(hours=ttl_hours))
    )
    return token.to_jwt()


def required_env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"{name} must be set (see .env.example)")
    return value


def env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    return int(raw) if raw else default


def env_float(name: str, default: float) -> float:
    raw = os.environ.get(name)
    return float(raw) if raw else default


def _dump_metrics(prefix: str, metrics) -> None:
    """Pretty-print the full PortalMetrics snapshot. Walks the UniFFI
    dataclasses directly — no protobuf `MessageToDict` anymore."""
    sections = {
        "sync": metrics.sync,
        "transport": metrics.transport,
        "buffers": metrics.buffers,
        "rtt": metrics.rtt,
    }
    print(f"{prefix} metrics:")
    for section, record in sections.items():
        print(f"  {section}:")
        # UniFFI records expose fields via dataclass attributes; we walk
        # `__dict__` to stay agnostic to naming.
        for k, v in record.__dict__.items():
            print(f"    {k}: {v}")


def _format_us(value) -> str:
    """Format a microsecond counter as `NNNus` or `N.NNms`, or `-` if None/0."""
    if value is None or value == 0:
        return "-"
    if value < 1000:
        return f"{value}us"
    return f"{value / 1000:.2f}ms"


async def periodic_metrics(portal, prefix: str, interval: float = 2.0):
    """Background task that logs the time-varying metrics (RTT, jitter, sync
    deltas, buffer fill) every `interval` seconds. Cancel with `task.cancel()`
    before disconnect."""
    import asyncio

    try:
        while True:
            await asyncio.sleep(interval)
            m = portal.metrics()
            rtt_last = _format_us(m.rtt.rtt_us_last)
            rtt_mean = _format_us(m.rtt.rtt_us_mean)
            rtt_p95 = _format_us(m.rtt.rtt_us_p95)
            sync_p50 = _format_us(m.sync.match_delta_us_p50)
            sync_p95 = _format_us(m.sync.match_delta_us_p95)
            frame_jitter = {k: _format_us(v) for k, v in m.transport.frame_jitter_us.items()}
            video_fill = dict(m.buffers.video_fill)
            print(
                f"{prefix} rtt={rtt_last}/{rtt_mean}/{rtt_p95} (last/mean/p95)"
                f" sync_delta={sync_p50}/{sync_p95} (p50/p95)"
                f" state_jitter={_format_us(m.transport.state_jitter_us)}"
                f" action_jitter={_format_us(m.transport.action_jitter_us)}"
                f" frame_jitter={frame_jitter}"
                f" video_fill={video_fill}"
                f" state_fill={m.buffers.state_fill}"
                f" dropped={m.sync.states_dropped}"
                f" obs={m.sync.observations_emitted}"
            )
    except asyncio.CancelledError:
        pass
