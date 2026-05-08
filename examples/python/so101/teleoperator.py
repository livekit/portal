"""Run on the **operator** side.

Drives a local SO-101 leader arm and presents the remote SO-101 follower
as a local lerobot ``Robot`` over a LiveKit Portal `Operator` session.
Each tick: read leader pose, push as action; pull
synced observation back (joint positions + camera frame) and stream it
to a rerun viewer along with Portal transport metrics.

Usage:
    cp .env.example .env  # fill in API_KEY / API_SECRET / serial port
    uv run teleoperator.py
"""
from __future__ import annotations

import time

import numpy as np
import rerun as rr
import rerun.blueprint as rrb
from lerobot.teleoperators.so_leader import SO101Leader, SO101LeaderConfig
from lerobot.utils.visualization_utils import init_rerun
from lerobot_robot_livekit import LiveKitRobot, LiveKitRobotConfig

from _common import env_int, env_str, load_env, mint_token, pace, required_env

IDENTITY = "so101-operator"


def log_rerun(namespace: str, data: dict) -> None:
    """Log a lerobot-shaped dict (motor floats + camera ndarrays) under `namespace/`.

    Uses `/` as the rerun path separator so entities nest under the namespace
    (e.g. `observation/shoulder_pan.pos`, `observation/front`) — lets the
    blueprint target whole groups via `origin=...`.

    Images are JPEG-compressed so memory stays bounded even with scrubbable
    history retained; scalars are logged as rerun scalar series.
    """
    for k, v in data.items():
        entity = f"{namespace}/{k}"
        if isinstance(v, np.ndarray):
            rr.log(entity, rr.Image(v).compress())
        else:
            rr.log(entity, rr.Scalars(float(v)))


def _us_to_ms(us: int | None) -> float | None:
    return us / 1e3 if us else None


def log_metrics(m) -> str:
    """Log Portal metrics under `metrics/` and return a one-line summary.

    Only logs fields that are actually present — Portal marks warm-up values
    (RTT / sync delta p50/p95) as `Optional[int]` in the UniFFI dataclass,
    so `None` means "no sample yet" and we skip those plots until they land.
    """
    parts: list[str] = []

    rtt_last = _us_to_ms(m.rtt.rtt_us_last)
    rtt_mean = _us_to_ms(m.rtt.rtt_us_mean)
    rtt_p95 = _us_to_ms(m.rtt.rtt_us_p95)
    if rtt_last is not None:
        rr.log("metrics/rtt_last_ms", rr.Scalars(rtt_last))
    if rtt_mean is not None:
        rr.log("metrics/rtt_mean_ms", rr.Scalars(rtt_mean))
    parts.append(
        f"rtt={rtt_last:.1f}/{rtt_mean:.1f}/{rtt_p95:.1f}ms"
        if rtt_last is not None and rtt_mean is not None and rtt_p95 is not None
        else "rtt=-"
    )

    sync_p50 = _us_to_ms(m.sync.match_delta_us_p50)
    sync_p95 = _us_to_ms(m.sync.match_delta_us_p95)
    if sync_p50 is not None:
        rr.log("metrics/sync_delta_p50_ms", rr.Scalars(sync_p50))
    if sync_p95 is not None:
        rr.log("metrics/sync_delta_p95_ms", rr.Scalars(sync_p95))
    parts.append(
        f"sync={sync_p50:.1f}/{sync_p95:.1f}ms"
        if sync_p50 is not None and sync_p95 is not None
        else "sync=-"
    )

    state_jitter = _us_to_ms(m.transport.state_jitter_us) or 0.0
    action_jitter = _us_to_ms(m.transport.action_jitter_us) or 0.0
    rr.log("metrics/state_jitter_ms", rr.Scalars(state_jitter))
    rr.log("metrics/action_jitter_ms", rr.Scalars(action_jitter))
    parts.append(f"jitter={state_jitter:.1f}/{action_jitter:.1f}ms")

    drops = m.sync.states_dropped
    rr.log("metrics/states_dropped", rr.Scalars(float(drops)))
    parts.append(f"drops={drops}")

    return " ".join(parts)


def build_blueprint(camera_name: str) -> rrb.Blueprint:
    """Four-panel layout: camera image, observation scalars, action scalars, metrics.

    Each panel is scoped by `origin=...` so rerun auto-discovers new entities
    within the group (per-motor scalars appear on the right panel without
    needing to enumerate them here).
    """
    return rrb.Blueprint(
        rrb.Horizontal(
            rrb.Spatial2DView(
                origin=f"/observation/{camera_name}",
                name="Camera",
            ),
            rrb.Vertical(
                rrb.TimeSeriesView(origin="/observation", name="Observation (state)"),
                rrb.TimeSeriesView(origin="/action", name="Action (commanded)"),
                rrb.TimeSeriesView(origin="/metrics", name="Portal metrics"),
            ),
            column_shares=[2, 3],
        ),
        collapse_panels=True,
    )


def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    fps = env_int("PORTAL_FPS", 30)
    camera_name = required_env("SO101_CAMERA_NAME")

    # Leader = local physical arm producing actions.
    # LiveKitRobot = remote follower dressed up as a local lerobot Robot, so
    # send_action() goes over the wire and get_observation() returns the
    # remote's synced joint state + camera frames.
    leader = SO101Leader(SO101LeaderConfig(
        id=env_str("SO101_LEADER_ID", "so101_leader"),
        port=required_env("SO101_LEADER_PORT"),
    ))
    robot = LiveKitRobot(LiveKitRobotConfig(
        url=url,
        token=mint_token(IDENTITY, room),
        session=room,
        fps=fps,
        camera_names=(camera_name,),
        camera_width=env_int("SO101_CAMERA_WIDTH", 640),
        camera_height=env_int("SO101_CAMERA_HEIGHT", 480),
    ), teleop=leader)

    leader.connect()
    robot.connect()
    init_rerun(session_name=f"so101-{room}")  # spawns the rerun viewer
    rr.send_blueprint(build_blueprint(camera_name))
    print(f"[operator] '{IDENTITY}' in '{room}' @ {fps} fps; ctrl-c to stop")

    try:
        for i in pace(fps):
            # Send action first so control latency never waits on rerun logging.
            if action := leader.get_action():
                robot.send_action(action)

            obs = robot.get_observation()

            # Anchor rerun's timeline to the sender's wall clock so scrubbing
            # reflects what happened on the physical robot, not receive time.
            if ts_us := robot.last_observation_timestamp_us:
                rr.set_time("robot_time", timestamp=ts_us / 1e6)
                # End-to-end staleness: how old is the obs we're acting on?
                # Includes sender→receiver transport + Portal sync buffering.
                age_ms = (time.time() * 1e6 - ts_us) / 1e3
                rr.log("metrics/obs_age_ms", rr.Scalars(age_ms))

            log_rerun("observation", obs or {})
            log_rerun("action", action or {})

            # Snapshot transport metrics once per second — cheap, but no need
            # to poll the FFI every tick.
            if i % fps == 0 and (m := robot.metrics()) is not None:
                summary = log_metrics(m)
                print(f"[operator] {summary}")
    except KeyboardInterrupt:
        print("\n[operator] stopping ...")
    finally:
        robot.disconnect()
        leader.disconnect()


if __name__ == "__main__":
    main()
