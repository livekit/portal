"""Everything in the SO-101 example that is not Portal: env, tokens, pacing,
and the Rerun viewer on the operator side."""
from __future__ import annotations

import datetime
import os
import pathlib
import time
from typing import Iterator

import numpy as np
import rerun as rr
import rerun.blueprint as rrb
from dotenv import load_dotenv
from livekit import api
from lerobot.utils.visualization_utils import init_rerun
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


def start_viewer(room: str, camera_name: str) -> None:
    """Spawn a Rerun viewer: camera on the left; state, actions and metrics on the right."""
    init_rerun(session_name=f"so101-{room}")
    rr.send_blueprint(rrb.Blueprint(
        rrb.Horizontal(
            rrb.Spatial2DView(origin=f"/observation/{camera_name}", name="Camera"),
            rrb.Vertical(
                rrb.TimeSeriesView(origin="/observation", name="Observation (state)"),
                rrb.TimeSeriesView(origin="/action", name="Action (commanded)"),
                rrb.TimeSeriesView(origin="/metrics", name="Portal metrics"),
            ),
            column_shares=[2, 3],
        ),
        collapse_panels=True,
    ))


def log_step(robot, observation: dict | None, action: dict | None) -> None:
    """Log one tick on the robot's clock, so scrubbing shows what happened on the arm."""
    if ts_us := robot.last_observation_timestamp_us:
        rr.set_time("robot_time", timestamp=ts_us / 1e6)
        rr.log("metrics/obs_age_ms", rr.Scalars((robot.now_us() - ts_us) / 1e3))
    for namespace, data in (("observation", observation or {}), ("action", action or {})):
        for k, v in data.items():
            if isinstance(v, np.ndarray):
                rr.log(f"{namespace}/{k}", rr.Image(v).compress())
            else:
                rr.log(f"{namespace}/{k}", rr.Scalars(float(v)))


def log_metrics(m) -> str:
    """Plot Portal's headline metrics and return a one-line summary."""
    values_us = {
        "rtt_p95": m.rtt.rtt_us_p95,
        "sync_delta_p95": m.sync.match_delta_us_p95,
        "state_jitter": m.transport.state_jitter_us,
        "action_jitter": m.transport.action_jitter_us,
    }
    parts = []
    for name, us in values_us.items():
        if us is not None:
            rr.log(f"metrics/{name}_ms", rr.Scalars(us / 1e3))
            parts.append(f"{name}={us / 1e3:.1f}ms")
    rr.log("metrics/states_dropped", rr.Scalars(float(m.sync.states_dropped)))
    parts.append(f"states_dropped={m.sync.states_dropped}")
    return " ".join(parts)
