"""Everything in the benchmark that is not Portal: settings, tokens, synthetic
frames, pacing, and latency statistics."""
from __future__ import annotations

import asyncio
import datetime
import os
import pathlib
import time
from dataclasses import dataclass

import numpy as np
from dotenv import load_dotenv
from livekit import api
from livekit.portal import DType, VideoCodec

HERE = pathlib.Path(__file__).parent
JOINTS = [(f"j{i}", DType.F32) for i in range(6)]


@dataclass(frozen=True)
class Settings:
    url: str
    room: str
    fps: int
    codec: VideoCodec
    width: int
    height: int
    warmup: float
    duration: float

    @classmethod
    def from_env(cls) -> Settings:
        load_dotenv(HERE / ".env")
        env = os.environ.get
        return cls(
            url=os.environ["LIVEKIT_URL"],
            room=os.environ["LIVEKIT_ROOM"],
            fps=int(env("PORTAL_FPS", 30)),
            codec=getattr(VideoCodec, env("PORTAL_CODEC", "h264").upper()),
            width=int(env("PORTAL_FRAME_WIDTH", 640)),
            height=int(env("PORTAL_FRAME_HEIGHT", 480)),
            warmup=float(env("PORTAL_WARMUP_SECONDS", 3)),
            duration=float(env("PORTAL_DURATION_SECONDS", 30)),
        )

    def token(self, identity: str) -> str:
        grants = api.VideoGrants(
            room_join=True,
            room=self.room,
            can_publish=True,
            can_subscribe=True,
            can_update_own_metadata=True,
        )
        # Zero playout delay: buffering video for smoothness would read as latency.
        room_config = api.RoomConfiguration(name=self.room, min_playout_delay=0, max_playout_delay=1)
        return (
            api.AccessToken(os.environ["LIVEKIT_API_KEY"], os.environ["LIVEKIT_API_SECRET"])
            .with_identity(identity)
            .with_grants(grants)
            .with_room_config(room_config)
            .with_ttl(datetime.timedelta(hours=1))
            .to_jwt()
        )


async def ticks(fps: int, duration: float):
    """Yield the tick number at `fps` for `duration` seconds."""
    next_tick = time.monotonic()
    for i in range(int(duration * fps)):
        yield i
        next_tick += 1 / fps
        await asyncio.sleep(max(0.0, next_tick - time.monotonic()))


def test_frames(width: int, height: int, count: int) -> list[np.ndarray]:
    """Moving gradients plus noise, so the encoder works on every frame.
    Made up front so generating them stays out of the measurement."""
    rng = np.random.default_rng(0)
    x = np.arange(width, dtype=np.int32)
    y = np.arange(height, dtype=np.int32)[:, None]
    frames = []
    for i in range(count):
        r = np.broadcast_to((x + i * 8) % 256, (height, width))
        g = np.broadcast_to((y + i * 8) % 256, (height, width))
        b = (x + y + i * 8) % 256
        frame = np.stack([r, g, b], axis=-1).astype(np.uint8)
        frame ^= rng.integers(0, 8, size=frame.shape, dtype=np.uint8)
        frames.append(frame)
    return frames


def fmt_us(value: float | None) -> str:
    if value is None:
        return "-"
    return f"{value:.0f}us" if abs(value) < 1000 else f"{value / 1000:.2f}ms"


class Stream:
    """Latency samples for one stream, in microseconds."""

    def __init__(self) -> None:
        self.latency_us: list[int] = []
        self.arrived_us: list[int] = []

    def record(self, latency_us: int, arrived_us: int) -> None:
        self.latency_us.append(latency_us)
        self.arrived_us.append(arrived_us)

    def pct(self, p: float) -> float | None:
        return float(np.percentile(self.latency_us, p)) if self.latency_us else None

    def jitter(self) -> float | None:
        """Mean change in latency between consecutive samples (RFC 3550, unsmoothed)."""
        return float(np.mean(np.abs(np.diff(self.latency_us)))) if len(self.latency_us) > 1 else None

    def max_gap(self) -> float | None:
        """Longest silence between arrivals: where a stall shows up."""
        return float(np.max(np.diff(self.arrived_us))) if len(self.arrived_us) > 1 else None


class Stats:
    """A named set of streams, with live and final printing."""

    def __init__(self, role: str, *names: str) -> None:
        self.role = role
        self.streams = {name: Stream() for name in names}

    def __getitem__(self, name: str) -> Stream:
        return self.streams[name]

    def reset(self) -> None:
        self.streams = {name: Stream() for name in self.streams}

    def print_live(self) -> None:
        live = " ".join(f"{n}={fmt_us(s.pct(50))}/{fmt_us(s.pct(95))}" for n, s in self.streams.items())
        print(f"[{self.role}] {live} (p50/p95)")

    def print_report(self) -> None:
        print(f"\n[{self.role}] results:")
        for name, s in self.streams.items():
            print(
                f"  {name:<8} n={len(s.latency_us):<6}"
                f" p50={fmt_us(s.pct(50)):>9} p95={fmt_us(s.pct(95)):>9}"
                f" p99={fmt_us(s.pct(99)):>9} max={fmt_us(s.pct(100)):>9}"
                f" jitter={fmt_us(s.jitter()):>9} max_gap={fmt_us(s.max_gap()):>9}"
            )
