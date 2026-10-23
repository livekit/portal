"""The benchmark robot: streams video and state, and measures what comes back.

  loop    capture to the action that answers it. This bounds your control loop.
  action  the operator's send_action to the robot's on_action
  send    how long send_video_frame blocks the robot
"""
import asyncio
import time

from livekit.portal import Action, Robot, RobotConfig
from _common import JOINTS, Settings, Stats, fmt_us, test_frames, ticks


async def main() -> None:
    settings = Settings.from_env()
    config = RobotConfig(settings.room)
    config.add_video("cam1", codec=settings.codec)
    config.add_state_typed(JOINTS)
    config.add_action_typed(JOINTS)
    config.set_fps(settings.fps)
    robot = Robot(config)

    stats = Stats("robot", "loop", "action", "send")

    def on_action(action: Action) -> None:
        # The teleoperator is synced to this clock, so these are true latencies.
        now = robot.now_us()
        stats["action"].record(now - action.timestamp_us, now)
        stats["loop"].record(now - action.in_reply_to_ts_us, now)

    robot.on_action(on_action)

    await robot.connect(settings.url, settings.token("robot"))
    while robot.active_operator() is None:
        await asyncio.sleep(0.1)

    fps = settings.fps
    frames = test_frames(settings.width, settings.height, fps)
    async for i in ticks(fps, settings.warmup + settings.duration):
        if i == int(settings.warmup * fps):
            stats.reset()

        ts = robot.now_us()
        started = time.perf_counter()
        robot.send_video_frame("cam1", frames[i % fps], timestamp_us=ts)
        stats["send"].record(int((time.perf_counter() - started) * 1e6), ts)
        robot.send_state({name: i / fps for name, _ in JOINTS}, timestamp_us=ts)

        if i % fps == 0:
            stats.print_live()

    # Let the last replies land.
    await asyncio.sleep(1)
    stats.print_report()
    m = robot.metrics()
    print(f"  portal   e2e_p50={fmt_us(m.policy.e2e_us_p50)} e2e_p95={fmt_us(m.policy.e2e_us_p95)}")
    await robot.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
