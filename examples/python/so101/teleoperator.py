"""Run on the operator's side: a local SO-101 leader drives the remote
follower, shown in a Rerun viewer.

    cp .env.example .env  # fill in API_KEY / API_SECRET / serial port
    uv run teleoperator.py
"""
from lerobot.teleoperators.so_leader import SO101Leader, SO101LeaderConfig
from lerobot_robot_livekit import LiveKitRobot, LiveKitRobotConfig

from _common import env_int, env_str, load_env, log_metrics, log_step, mint_token, pace, required_env, start_viewer

IDENTITY = "so101-operator"


def main() -> None:
    load_env()
    url = required_env("LIVEKIT_URL")
    room = required_env("LIVEKIT_ROOM")
    fps = env_int("PORTAL_FPS", 30)
    camera_name = required_env("SO101_CAMERA_NAME")

    leader = SO101Leader(SO101LeaderConfig(
        id=env_str("SO101_LEADER_ID", "so101_leader"),
        port=required_env("SO101_LEADER_PORT"),
    ))
    # The remote follower, as a local lerobot Robot: send_action() goes over the
    # wire, get_observation() returns its synced joints and camera frames.
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
    start_viewer(room, camera_name)
    print(f"[operator] '{IDENTITY}' in '{room}' @ {fps} fps; ctrl-c to stop")

    try:
        for i in pace(fps):
            if action := leader.get_action():
                robot.send_action(action)
            observation = robot.get_observation()

            log_step(robot, observation, action)
            if i % fps == 0:
                print(f"[operator] {log_metrics(robot.metrics())}")
    except KeyboardInterrupt:
        print("\n[operator] stopping ...")
    finally:
        robot.disconnect()
        leader.disconnect()


if __name__ == "__main__":
    main()
