"""The observer: records the session to a Rerun archive and marks each take
with keypoints. It sees everything and never drives."""
import asyncio

from livekit.portal import Observer, ObserverConfig
from livekit.portal.recording import RrdSink
from _common import CONFIG, Settings


async def main() -> None:
    settings = Settings.from_env()
    observer = Observer(ObserverConfig.from_yaml_file(CONFIG, settings.room))

    observer.on_keypoint(lambda kp: print(f"[observer] {kp.type} {kp.payload}"))
    synced = asyncio.Event()
    observer.on_time_synced(synced.set)

    await observer.connect(settings.url, settings.token("observer"))
    # Start on the robot's clock, so the whole archive is too.
    await synced.wait()

    sink = RrdSink(settings.recording_dir)
    observer.record_to(sink)

    for take in range(int(settings.duration // settings.take_seconds)):
        await observer.send_keypoint("recording", {"task_description": f"take {take}"})
        await asyncio.sleep(settings.take_seconds)
        await observer.send_keypoint("idle")

    # Flushes and closes the archive.
    await observer.disconnect()
    print(f"[observer] open it with: rerun {sink.path}")


if __name__ == "__main__":
    asyncio.run(main())
