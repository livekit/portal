"""The benchmark teleoperator: replies to every observation at once, and
measures how long each stream took to arrive.

  video   capture to decoded frame
  state   capture to state
  obs     capture to fused observation
"""
import asyncio

from livekit.portal import Observation, Operator, OperatorConfig, State, VideoFrameData
from _common import JOINTS, Settings, Stats, fmt_us


async def main() -> None:
    settings = Settings.from_env()
    config = OperatorConfig(settings.room)
    config.add_video("cam1", codec=settings.codec)
    config.add_state_typed(JOINTS)
    config.add_action_typed(JOINTS)
    config.set_fps(settings.fps)
    op = Operator(config)

    stats = Stats("teleoperator", "obs", "video", "state")

    # After the clock syncs, now_us() is on the robot's clock, so
    # `now - timestamp_us` is a one-way latency even across machines.
    def on_video_frame(_track: str, frame: VideoFrameData) -> None:
        now = op.now_us()
        stats["video"].record(now - frame.timestamp_us, now)

    def on_state(state: State) -> None:
        now = op.now_us()
        stats["state"].record(now - state.timestamp_us, now)

    def on_observation(obs: Observation) -> None:
        now = op.now_us()
        stats["obs"].record(now - obs.timestamp_us, now)
        # A zero-latency policy: answer straight away.
        op.send_action(obs.state, in_reply_to_ts_us=obs.timestamp_us)

    op.on_video_frame("cam1", on_video_frame)
    op.on_state(on_state)
    op.on_observation(on_observation)

    synced = asyncio.Event()
    op.on_time_synced(synced.set)
    await op.connect(settings.url, settings.token("teleoperator"))
    await synced.wait()
    await op.set_active_operator(op.local_identity())

    await asyncio.sleep(settings.warmup)
    stats.reset()

    # Measure until the robot finishes and leaves.
    m = op.metrics()
    while op.robot_identity() is not None:
        m = op.metrics()
        stats.print_live()
        await asyncio.sleep(1)

    stats.print_report()
    print(
        f"  portal   rtt_p95={fmt_us(m.rtt.rtt_us_p95)}"
        f" clock_uncertainty={fmt_us(m.time_sync.uncertainty_us)}"
        f" sync_delta_p95={fmt_us(m.sync.match_delta_us_p95)}"
        f" states_dropped={m.sync.states_dropped}"
    )
    await op.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
