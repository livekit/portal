"""Robot side. Runs on your machine.

Each tick we build a frame, paint a (seq, capture_us) token into it as a QR
code, and send it over H264 with a matching state sample. The policy on Modal
reads the QR back and sends the token straight back as an action. We stamped
capture_us on our own clock, so `now - capture_us` is the round-trip
glass-to-glass latency.

    cp .env.example .env
    uv sync
    uv run robot.py                 # this file, here
    uv run modal run policy.py      # the policy, in another terminal
"""
from __future__ import annotations

import asyncio
import datetime
import os
import pathlib
import time
from typing import Optional

import numpy as np
from dotenv import load_dotenv
from livekit import api
from livekit.portal import Action, Robot, RobotConfig

TRACK = "cam1"
ROOM = "portal-modal-mock"
FPS = 15                    # keep in sync with fps in portal.yaml
WIDTH, HEIGHT = 640, 480
DURATION_S = 30

# The wire contract (video track, state and action schemas, fps) lives in
# portal.yaml, shared with policy.py.
CONFIG = pathlib.Path(__file__).parent / "portal.yaml"


def connect(identity: str):
    """Read creds from .env (or the Modal secret) and mint a room token."""
    load_dotenv(pathlib.Path(__file__).parent / ".env")   # no-op if the file is absent
    grants = api.VideoGrants(
        room_join=True, room=ROOM, can_publish=True,
        can_subscribe=True, can_update_own_metadata=True,
    )
    token = (
        api.AccessToken(os.environ["LIVEKIT_API_KEY"], os.environ["LIVEKIT_API_SECRET"])
        .with_identity(identity).with_grants(grants)
        .with_ttl(datetime.timedelta(hours=1)).to_jwt()
    )
    return os.environ["LIVEKIT_URL"], token


def make_frame(seq: int, capture_us: int) -> np.ndarray:
    """A grey frame with a QR code of `p1:seq,capture_us` in the middle. The
    grey drifts a little so the stream looks alive."""
    import qrcode
    from PIL import Image

    qr = qrcode.QRCode(error_correction=qrcode.constants.ERROR_CORRECT_Q, box_size=10, border=2)
    qr.add_data(f"p1:{seq},{capture_us}")
    qr.make(fit=True)
    img = qr.make_image(fill_color="black", back_color="white").convert("RGB")
    px = min(WIDTH, HEIGHT) // 2
    code = np.asarray(img.resize((px, px), Image.NEAREST), dtype=np.uint8)

    frame = np.full((HEIGHT, WIDTH, 3), 100 + seq % 40, dtype=np.uint8)
    top, left = (HEIGHT - px) // 2, (WIDTH - px) // 2
    frame[top:top + px, left:left + px] = code
    return frame


def fmt_us(value) -> str:
    if not value:
        return "-"
    return f"{value}us" if value < 1000 else f"{value / 1000:.1f}ms"


class Stats:
    """Latency and loss, all measured on the robot's own clock."""

    def __init__(self) -> None:
        self.replies = 0
        self.highest_seq = -1
        self.glass_us: Optional[int] = None   # full video round trip
        self.codec_us: Optional[int] = None   # extra time the video path took

    def record(self, seq: int, glass_us: int, codec_us: int) -> None:
        self.replies += 1
        self.highest_seq = max(self.highest_seq, seq)
        self.glass_us = glass_us
        self.codec_us = codec_us

    @property
    def dropped(self) -> int:
        """Frames we sent, up to the newest reply, that never came back."""
        return max(0, self.highest_seq + 1 - self.replies)


async def main() -> None:
    url, token = connect("robot")

    cfg = RobotConfig.from_yaml_file(CONFIG, ROOM)
    robot = Robot(cfg)
    stats = Stats()

    def on_action(action: Action) -> None:
        v = action.values
        glass_us = int(time.time() * 1_000_000) - int(v["t_capture_us"])
        stats.record(int(v["seq"]), glass_us, int(v["codec_lag_us"]))

    robot.on_action(on_action)

    print(f"[robot] connecting to {url} in room '{ROOM}' ...")
    await robot.connect(url, token)

    # Wait for a policy to take control before streaming, so we do not stream
    # into an empty room and count those early frames as dropped.
    print("[robot] connected, waiting for a policy to take control ...")
    while robot.active_operator() is None:
        await asyncio.sleep(0.2)
    print(f"[robot] '{robot.active_operator()}' in control, streaming "
          f"{WIDTH}x{HEIGHT} H264 at {FPS} fps for {DURATION_S}s")

    interval = 1.0 / FPS
    start = time.monotonic()
    next_tick = start
    last_log = start

    try:
        for seq in range(DURATION_S * FPS):
            capture_us = int(time.time() * 1_000_000)
            frame = make_frame(seq, capture_us)
            # Same timestamp on the frame and the state so they fuse into one
            # observation on the policy side.
            robot.send_video_frame(TRACK, frame, timestamp_us=capture_us)
            robot.send_state({"seq": float(seq)}, timestamp_us=capture_us)

            now = time.monotonic()
            if now - last_log >= 1.0:
                m = robot.metrics()
                print(
                    f"[robot] t={int(now - start):>2}s sent={seq + 1} "
                    f"replies={stats.replies} dropped={stats.dropped} "
                    f"glass2glass={fmt_us(stats.glass_us)} codec_lag={fmt_us(stats.codec_us)} "
                    f"e2e={fmt_us(m.policy.e2e_us_p50)}/{fmt_us(m.policy.e2e_us_p95)} "
                    f"rtt={fmt_us(m.rtt.rtt_us_last)} active={robot.active_operator()}"
                )
                last_log = now

            next_tick += interval
            sleep_for = next_tick - time.monotonic()
            if sleep_for > 0:
                await asyncio.sleep(sleep_for)

        # Let the last in-flight replies land before the final tally, or they
        # would look like dropped frames.
        await asyncio.sleep(1.0)

        m = robot.metrics()
        print()
        print("[robot] final:")
        print(f"  glass-to-glass p50/p95: {fmt_us(m.policy.e2e_us_p50)} / {fmt_us(m.policy.e2e_us_p95)}")
        print(f"  network rtt (last):     {fmt_us(m.rtt.rtt_us_last)}")
        print(f"  codec lag (last):       {fmt_us(stats.codec_us)}")
        print(f"  frames sent:            {DURATION_S * FPS}")
        print(f"  replies / dropped:      {stats.replies} / {stats.dropped}")
    finally:
        print("[robot] disconnecting...")
        await robot.disconnect()
        robot.close()


if __name__ == "__main__":
    asyncio.run(main())
