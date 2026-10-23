"""The robot: streams a camera and its state, and executes actions from the
operator in control."""
import asyncio

from livekit.portal import Robot, RobotConfig, RpcInvocationData
from _common import CONFIG, Settings, arm_state, camera_frame, print_metrics, ticks


def say(data: RpcInvocationData) -> str:
    print(f"[robot] {data.caller_identity} says: {data.payload}")
    return "ok"


async def main() -> None:
    settings = Settings.from_env()
    config = RobotConfig.from_yaml_file(CONFIG, settings.room)
    robot = Robot(config)

    robot.on_active_operator_changed(lambda who: print(f"[robot] in control: {who}"))
    robot.register_rpc_method("say", say)

    await robot.connect(settings.url, settings.token("robot"))

    fps = config.fps
    async for i in ticks(fps, settings.duration):
        # One timestamp for the frame and the state, so they fuse into one observation.
        ts = robot.now_us()
        robot.send_video_frame("cam1", camera_frame(i / fps), timestamp_us=ts)
        robot.send_state(arm_state(i / fps), timestamp_us=ts)

        # Latest action from the operator in control. Hand it to your motors.
        action = robot.get_action()
        if action and i % fps == 0:
            print(f"[robot] action from {action.sender}: {action.values}")
            print_metrics("robot", robot.metrics())

    await robot.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
