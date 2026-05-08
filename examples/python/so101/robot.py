"""Run on the **physical SO-101** side.

Drives a local SO-101 follower with one OpenCV camera, and exposes it over
a LiveKit Portal `Robot` session. Pulls actions from the remote
operator each tick and forwards them to the arm; pushes the arm's
observation (joint positions + camera frame) back upstream.

Usage:
    cp .env.example .env  # fill in API_KEY / API_SECRET / serial port
    uv run robot.py
"""
from __future__ import annotations

from lerobot.cameras.opencv import OpenCVCameraConfig
from lerobot.robots.so_follower import SO101Follower, SO101FollowerConfig
from lerobot_teleoperator_livekit import (
    LiveKitTeleoperator,
    LiveKitTeleoperatorConfig,
)

from _common import env_int, env_str, load_env, mint_token, pace, required_env

IDENTITY = "so101-robot"


def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    fps = env_int("PORTAL_FPS", 30)
    camera_name = required_env("SO101_CAMERA_NAME")

    # Local physical follower + camera. LiveKitTeleoperator wraps it as a
    # remote-driven Teleoperator: get_action() pulls from the wire,
    # send_feedback() publishes the arm's obs (joints + camera) upstream.
    robot = SO101Follower(SO101FollowerConfig(
        id=env_str("SO101_FOLLOWER_ID", "so101_follower"),
        port=required_env("SO101_FOLLOWER_PORT"),
        cameras={camera_name: OpenCVCameraConfig(
            index_or_path=env_int("SO101_CAMERA_INDEX", 0),
            fps=fps,
            width=env_int("SO101_CAMERA_WIDTH", 640),
            height=env_int("SO101_CAMERA_HEIGHT", 480),
        )},
    ))
    teleop = LiveKitTeleoperator(LiveKitTeleoperatorConfig(
        url=url,
        token=mint_token(IDENTITY, room),
        session=room,
        fps=fps,
    ), robot=robot)

    robot.connect()
    teleop.connect()
    print(f"[robot] '{IDENTITY}' in '{room}' @ {fps} fps; ctrl-c to stop")

    try:
        for _ in pace(fps):
            # Publish obs first, then forward any action that just arrived.
            # Order keeps the local arm's feedback fresh without blocking on
            # a tick where no action is available yet.
            teleop.send_feedback(robot.get_observation())
            if action := teleop.get_action():
                robot.send_action(action)
    except KeyboardInterrupt:
        print("\n[robot] stopping ...")
    finally:
        teleop.disconnect()
        robot.disconnect()


if __name__ == "__main__":
    main()
