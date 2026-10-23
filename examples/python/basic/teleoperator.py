"""The teleoperator: takes control of the robot, watches its camera and state,
and sends actions back."""
import asyncio

from livekit.portal import Operator, OperatorConfig
from _common import CONFIG, Settings, print_metrics, ticks


async def main() -> None:
    settings = Settings.from_env()
    config = OperatorConfig.from_yaml_file(CONFIG, settings.room)
    op = Operator(config)

    await op.connect(settings.url, settings.token("teleoperator"))
    while op.robot_identity() is None:
        await asyncio.sleep(0.1)

    # The robot ignores everyone until someone takes control.
    await op.set_active_operator(op.local_identity())
    await op.perform_rpc("say", payload="hello")

    fps = config.fps
    async for i in ticks(fps, settings.duration):
        # Latest observation: a camera frame and the state captured with it.
        obs = op.get_observation()
        if obs is None:
            continue

        # Your teleop device or policy goes here. This one mirrors the state back.
        op.send_action(obs.state, in_reply_to_ts_us=obs.timestamp_us)

        if i % fps == 0:
            frame = obs.frames["cam1"]
            print(f"[teleoperator] {frame.width}x{frame.height} frame, state {obs.state}")
            print_metrics("teleoperator", op.metrics())

    await op.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
