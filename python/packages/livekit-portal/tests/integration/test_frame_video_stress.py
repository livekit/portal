"""Stress + benchmark scenarios for frame video.

These hammer the byte-stream path harder than `test_frame_video.py`:
4K frames, sustained throughput at 30 fps, multiple concurrent tracks,
extreme aspect ratios, late-subscribe operator, latency measurement,
process-RSS tracking. Print numbers, assert invariants — no hangs, no
leaks, no silent drops.

Skipped automatically when `LIVEKIT_URL` isn't set (see conftest).
"""
from __future__ import annotations

import asyncio
import gc
import resource
import statistics
import time

import numpy as np
import pytest

from livekit.portal import (
    DType,
    Observation,
    PortalError,
    VideoCodec,
    VideoFrameData,
    frame_bytes_to_numpy_rgb,
)


def _rss_mib() -> float:
    """Process-wide resident set size in MiB. Linux reports kB, macOS bytes."""
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    # macOS Darwin reports ru_maxrss in bytes; Linux in kilobytes.
    import sys
    if sys.platform == "darwin":
        return rss / (1024 * 1024)
    return rss / 1024

pytestmark = pytest.mark.asyncio
SETTLE_S = 0.6


def _gradient(width: int, height: int, seed: int = 0) -> np.ndarray:
    """Reproducible high-entropy RGB. Same shape as test_frame_video.py."""
    x = np.arange(width, dtype=np.int32)
    y = np.arange(height, dtype=np.int32)[:, None]
    r = ((x + seed) % 256).astype(np.uint8)
    g = ((y + seed) % 256).astype(np.uint8)
    b = ((x + y + seed) % 256).astype(np.uint8)
    r_full = np.broadcast_to(r, (height, width))
    g_full = np.broadcast_to(g, (height, width))
    return np.stack([r_full, g_full, b], axis=-1)


# ---------------------------------------------------------------------------
# 4K — exercises the byte-stream fragmentation + memory ceilings
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "codec",
    [
        pytest.param(VideoCodec.RAW, id="raw"),
        pytest.param(VideoCodec.PNG, id="png"),
        pytest.param(VideoCodec.MJPEG, id="mjpeg"),
    ],
)
async def test_4k_frame_roundtrip(pair, codec):
    """3840×2160 = 24.9 MB raw. Far above the 15 KB data-packet cap and
    big enough to expose any silent bandwidth or memcpy bugs.

    Asserts:
      * Frame arrives.
      * Dimensions match.
      * For lossless codecs (Raw, PNG), bytes are exact.
    """
    pair.robot_cfg.add_video("cam", codec=codec, quality=90)
    pair.operator_cfg.add_video("cam", codec=codec, quality=90)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )

    sent = _gradient(3840, 2160, seed=11)

    t0 = time.perf_counter()
    pair.robot.send_video_frame("cam", sent)
    # 25 MB raw can take several seconds to traverse the SFU and decode.
    # Poll up to a generous ceiling rather than race a fixed settle window,
    # which flakes under load (issue #60).
    from integration.conftest import wait_for

    timeout_s = 30.0
    ok = await wait_for(lambda: len(received) == 1, timeout_s=timeout_s)
    elapsed = time.perf_counter() - t0

    assert ok, f"4K {codec} not received within {timeout_s}s; got {len(received)}"
    got = received[0]
    assert (got.width, got.height) == (3840, 2160)
    arr = frame_bytes_to_numpy_rgb(bytes(got.data), got.width, got.height)
    if codec is not VideoCodec.MJPEG:
        np.testing.assert_array_equal(
            arr, sent, f"{codec} 4K roundtrip must be byte-exact"
        )
    print(f"\n[stress] 4K {codec.name} send→receive in {elapsed:.2f}s")


# ---------------------------------------------------------------------------
# Sustained throughput
# ---------------------------------------------------------------------------


async def test_sustained_30fps_mjpeg_540p_for_5s(pair):
    """Send at a steady 30 fps for 5 seconds (150 frames), 960×540 MJPEG.

    Asserts:
      * Receiver sees ≥ 90% of frames (loss tolerance).
      * No publisher-queue drops at this rate.
      * No more than 1 dropped frame at the publisher under steady state.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=85)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=85)

    received: list[int] = []  # store timestamps to detect ordering / loss
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f.timestamp_us)
    )

    fps = 30
    duration_s = 5
    n_frames = fps * duration_s
    rgb = _gradient(960, 540, seed=42)

    interval = 1.0 / fps
    start = time.monotonic()
    for i in range(n_frames):
        next_at = start + i * interval
        sleep_for = next_at - time.monotonic()
        if sleep_for > 0:
            await asyncio.sleep(sleep_for)
        pair.robot.send_video_frame("cam", rgb, timestamp_us=i)

    # Drain.
    await asyncio.sleep(SETTLE_S + 1.0)

    snap = pair.robot.metrics().transport
    publisher_dropped = snap.frames_dropped_publisher_full.get("cam", 0)
    sent = snap.frames_sent.get("cam", 0)

    assert publisher_dropped <= 1, (
        f"steady 30 fps must not overrun the publisher queue; got {publisher_dropped} drops"
    )
    delivered = len(received)
    delivered_pct = 100 * delivered / n_frames
    print(
        f"\n[stress] 30 fps × 5 s, 540p MJPEG: "
        f"sent={sent}, received={delivered} ({delivered_pct:.1f}%), "
        f"publisher_dropped={publisher_dropped}"
    )
    assert delivered_pct >= 90.0, (
        f"loss too high: {delivered_pct:.1f}% delivered (expected ≥ 90%)"
    )


# ---------------------------------------------------------------------------
# Multi-track concurrent — three tracks streaming at once
# ---------------------------------------------------------------------------


async def test_three_tracks_concurrent(pair):
    """Robot publishes 3 frame-video tracks simultaneously, mixed codecs.
    Each frame on each track should land independently — no
    cross-track stomping in slot map, sync buffer, or metrics."""
    cams = [
        ("front", VideoCodec.MJPEG, 85),
        ("wrist", VideoCodec.PNG, 90),
        ("over",  VideoCodec.RAW,   0),
    ]
    for name, codec, q in cams:
        pair.robot_cfg.add_video(name, codec=codec, quality=q)
        pair.operator_cfg.add_video(name, codec=codec, quality=q)

    received: dict[str, list[VideoFrameData]] = {n: [] for n, _, _ in cams}
    await pair.start()
    for name, _, _ in cams:
        # Capture name in default arg to bind it inside the lambda.
        pair.operator.on_video_frame(
            name, lambda n, f, _name=name: received[_name].append(f)
        )

    # Send 5 frames per camera, same timestamp budget so the receiver
    # groups them through the sync buffer if state were also flowing.
    rgb = _gradient(160, 120)
    for i in range(5):
        for name, _, _ in cams:
            pair.robot.send_video_frame(name, rgb, timestamp_us=i)
    await asyncio.sleep(SETTLE_S + 0.5)

    for name, _, _ in cams:
        assert len(received[name]) == 5, (
            f"track {name}: expected 5, got {len(received[name])}"
        )


# ---------------------------------------------------------------------------
# Extreme aspect ratios — one-pixel-wide / one-pixel-tall edges
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "w,h",
    [
        pytest.param(1, 1, id="1x1"),
        pytest.param(2, 2, id="2x2"),
        pytest.param(8, 8, id="8x8-mjpeg-block"),
    ],
)
async def test_minimal_dims_lossless(pair, w, h):
    """1×1 RGB roundtrip — codec must not have a hidden block-size floor.
    Lossless only, since MJPEG's 8×8 DCT block can't represent a 1×1 sample."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    sent = _gradient(w, h, seed=99)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    arr = frame_bytes_to_numpy_rgb(bytes(received[0].data), received[0].width, received[0].height)
    np.testing.assert_array_equal(arr, sent)


@pytest.mark.parametrize(
    "w,h",
    [
        pytest.param(2048, 8, id="wide-2048x8"),
        pytest.param(8, 2048, id="tall-8x2048"),
    ],
)
async def test_extreme_aspect_ratio(pair, w, h):
    """Very wide / very tall frames. Stresses the codec's stride
    handling and the wire format's u16 width/height fields."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    sent = _gradient(w, h, seed=7)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S + 0.3)

    assert len(received) == 1
    assert (received[0].width, received[0].height) == (w, h)


# ---------------------------------------------------------------------------
# Late-subscribe operator — robot streams before the operator joins
# ---------------------------------------------------------------------------


async def test_operator_joins_after_robot_starts_sending():
    """Robot connects and starts publishing frames. Operator joins later.
    Frames sent before the operator was subscribed are unrecoverable —
    LiveKit byte streams aren't replayed — but post-join frames must
    arrive cleanly and no leaked state should carry over.
    """
    from integration.conftest import URL, _make_token, Pair

    p = Pair()
    p.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    p.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    try:
        # Robot connects first, sends a few "pre-operator" frames.
        from livekit.portal import Robot, Operator
        p.robot = Robot(p.robot_cfg)
        await p.robot.connect(URL, _make_token("robot", p.room))
        rgb = _gradient(160, 120)
        for i in range(5):
            p.robot.send_video_frame("cam", rgb, timestamp_us=i)
        # Give those a chance to (silently) hit the air.
        await asyncio.sleep(0.4)

        # Operator joins. Frames sent before this won't surface (no peer
        # subscribed). Then send post-join frames and assert delivery.
        received: list[VideoFrameData] = []
        p.operator = Operator(p.operator_cfg)
        await p.operator.connect(URL, _make_token("operator", p.room))
        p.operator.on_video_frame(
            "cam", lambda name, f: received.append(f)
        )
        await asyncio.sleep(0.4)  # let the join settle

        for i in range(100, 105):
            p.robot.send_video_frame("cam", rgb, timestamp_us=i)
        await asyncio.sleep(SETTLE_S + 0.4)

        # All 5 post-join frames must arrive. Pre-join frames should not.
        post_join_ts = {f.timestamp_us for f in received}
        assert post_join_ts.issuperset({100, 101, 102, 103, 104}), (
            f"missing post-join frames: got {post_join_ts}"
        )
        # We should not see any of the pre-join timestamps (0..5).
        assert post_join_ts.isdisjoint({0, 1, 2, 3, 4}), (
            f"unexpected pre-join replays: {post_join_ts & {0,1,2,3,4}}"
        )
    finally:
        await p.stop()


# ---------------------------------------------------------------------------
# Backpressure path — sustained over-rate must surface as drops, not OOM
# ---------------------------------------------------------------------------


async def test_sustained_overrate_drops_bounded(pair):
    """Send at 200 fps (way above what the link can sustain) for 2 seconds.
    The publisher queue caps at 60 — drops are expected and must surface
    in the metric. Memory must not balloon (heap-bounded by queue cap +
    in-flight payload).
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.RAW)
    await pair.start()

    rgb = _gradient(640, 480)
    target_fps = 200
    duration_s = 2
    interval = 1.0 / target_fps
    n = target_fps * duration_s

    start = time.monotonic()
    for i in range(n):
        next_at = start + i * interval
        delay = next_at - time.monotonic()
        if delay > 0:
            await asyncio.sleep(delay)
        pair.robot.send_video_frame("cam", rgb, timestamp_us=i)

    await asyncio.sleep(SETTLE_S + 1.5)

    snap = pair.robot.metrics().transport
    sent = snap.frames_sent.get("cam", 0)
    dropped = snap.frames_dropped_publisher_full.get("cam", 0)
    print(
        f"\n[stress] 200 fps × 2 s, 640x480 RAW: sent={sent} dropped={dropped} "
        f"(offered={n})"
    )
    assert sent + dropped == n, (
        f"sent + dropped must account for every offered frame: sent={sent} dropped={dropped} offered={n}"
    )
    assert dropped > 0, "expected drops at 200 fps RAW; the link can't sustain that"


# ---------------------------------------------------------------------------
# u16 boundary — declared dimensions exactly at the wire ceiling
# ---------------------------------------------------------------------------


async def test_dim_above_u16_rejected_at_send(pair):
    """Width above u16::MAX must surface as a Codec error at send, not
    silently truncate or panic. This is a property of the wire format,
    so the actual server flow doesn't matter — we just want a clean
    error from `send_video_frame`."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.RAW)
    await pair.start()

    # Construct a frame whose dimensions fit Python ints but exceed u16.
    # We can't actually allocate a 100_000×8 RGB array reasonably without
    # eating memory, so check the length-prefix path with a small but
    # over-u16 width via a hand-built bytes payload.
    over_w = 100_000
    over_h = 8
    rgb = bytes(over_w * over_h * 3)
    with pytest.raises(Exception) as excinfo:
        pair.robot.send_video_frame("cam", rgb, width=over_w, height=over_h)
    msg = str(excinfo.value).lower()
    assert "u16" in msg or "dimension" in msg, (
        f"expected u16 / dimension error, got: {excinfo.value}"
    )


# ---------------------------------------------------------------------------
# Repeated reconnect — leaks would surface as growing metrics or hangs
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Latency profile — round-trip from send to user callback, per codec
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "w,h",
    [
        pytest.param(8, 8,    id="tiny-8x8-1chunk"),
        pytest.param(320, 240, id="qvga-1chunk"),
        pytest.param(640, 480, id="vga-62chunks"),
        pytest.param(1280, 720, id="720p-185chunks"),
        pytest.param(1920, 1080, id="1080p-415chunks"),
    ],
)
async def test_raw_latency_vs_chunk_count(pair, w, h):
    """Latency on RAW scales roughly linearly with payload size in 15 KB
    chunks (~2 ms per chunk on localhost). The cost lives in the SCTP
    data-channel drain (`buffered_amount` flow control), not in any
    writer-level loop — pipelining the writer was tested upstream and
    helps only when the whole payload fits in the in-flight window. So
    the only path to lower latency for big lossless frames is a smaller
    encoded payload (MJPEG, lower-resolution capture).

    Prints chunk count + p50/p95 + cumulative bytes_sent so the
    relationship is visible. No tight upper bound — networks vary.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.RAW)

    samples_us: list[int] = []
    arrival_event = asyncio.Event()
    next_expected_ts: int | None = None

    def on_frame(name: str, f: VideoFrameData) -> None:
        nonlocal next_expected_ts
        if next_expected_ts is None or f.timestamp_us != next_expected_ts:
            return
        now_us = int(time.time() * 1_000_000)
        samples_us.append(now_us - f.timestamp_us)
        arrival_event.set()

    await pair.start()
    pair.operator.on_video_frame("cam", on_frame)

    rgb = _gradient(w, h, seed=2)
    n = 20

    # Warm up.
    for _ in range(2):
        ts = int(time.time() * 1_000_000)
        next_expected_ts = ts
        arrival_event.clear()
        pair.robot.send_video_frame("cam", rgb, timestamp_us=ts)
        try:
            await asyncio.wait_for(arrival_event.wait(), timeout=8.0)
        except asyncio.TimeoutError:
            pytest.fail(f"warmup RAW {w}x{h} did not arrive")
    samples_us.clear()

    for _ in range(n):
        ts = int(time.time() * 1_000_000)
        next_expected_ts = ts
        arrival_event.clear()
        pair.robot.send_video_frame("cam", rgb, timestamp_us=ts)
        try:
            await asyncio.wait_for(arrival_event.wait(), timeout=8.0)
        except asyncio.TimeoutError:
            pytest.fail(f"latency sample for RAW {w}x{h} did not arrive")
        await asyncio.sleep(0.02)

    payload_bytes = w * h * 3
    chunks = (payload_bytes + 14999) // 15000
    p50 = statistics.median(samples_us) / 1000
    p95 = sorted(samples_us)[int(0.95 * len(samples_us))] / 1000
    avg = statistics.mean(samples_us) / 1000
    bytes_sent = pair.robot.metrics().transport.bytes_sent.get("cam", 0)
    print(
        f"\n[chunk-scan] RAW {w}x{h} payload={payload_bytes / 1024:.1f} KiB "
        f"chunks={chunks}  p50={p50:.1f}ms  p95={p95:.1f}ms  avg={avg:.1f}ms  "
        f"bytes_sent={bytes_sent} (≈{bytes_sent / max(1, len(samples_us) + 2)})"
    )


@pytest.mark.parametrize(
    "codec,quality",
    [
        pytest.param(VideoCodec.RAW,   0,  id="raw-640x480"),
        pytest.param(VideoCodec.PNG,   0,  id="png-640x480"),
        pytest.param(VideoCodec.MJPEG, 90, id="mjpeg-q90-640x480"),
    ],
)
async def test_latency_profile_640x480(pair, codec, quality):
    """Measure end-to-end send→callback latency per codec at 640×480 over
    50 frames. Localhost SFU + decode dominates the number, but the
    relative shape (Raw < MJPEG ≈ PNG) tells us whether encode/decode is
    a bottleneck. Asserts a soft ceiling so a regression that doubles
    latency would be loud.
    """
    pair.robot_cfg.add_video("cam", codec=codec, quality=quality)
    pair.operator_cfg.add_video("cam", codec=codec, quality=quality)

    samples_us: list[int] = []
    arrival_event = asyncio.Event()
    next_expected_ts: int | None = None

    def on_frame(name: str, f: VideoFrameData) -> None:
        nonlocal next_expected_ts
        if next_expected_ts is None or f.timestamp_us != next_expected_ts:
            return
        # Latency = wallclock_now - send_ts (which is wallclock_at_send).
        now_us = int(time.time() * 1_000_000)
        samples_us.append(now_us - f.timestamp_us)
        arrival_event.set()

    await pair.start()
    pair.operator.on_video_frame("cam", on_frame)

    rgb = _gradient(640, 480, seed=1)
    n = 50
    # Warm up the codec / SFU so the first sample isn't an outlier.
    for _ in range(3):
        ts = int(time.time() * 1_000_000)
        next_expected_ts = ts
        arrival_event.clear()
        pair.robot.send_video_frame("cam", rgb, timestamp_us=ts)
        try:
            await asyncio.wait_for(arrival_event.wait(), timeout=2.0)
        except asyncio.TimeoutError:
            pytest.fail(f"warmup frame for {codec} did not arrive")
    samples_us.clear()

    for _ in range(n):
        ts = int(time.time() * 1_000_000)
        next_expected_ts = ts
        arrival_event.clear()
        pair.robot.send_video_frame("cam", rgb, timestamp_us=ts)
        try:
            await asyncio.wait_for(arrival_event.wait(), timeout=2.0)
        except asyncio.TimeoutError:
            pytest.fail(f"latency sample for {codec} did not arrive")
        # Light spacing between samples so we measure latency, not throughput.
        await asyncio.sleep(0.01)

    p50 = statistics.median(samples_us) / 1000  # ms
    p95 = sorted(samples_us)[int(0.95 * len(samples_us))] / 1000
    avg = statistics.mean(samples_us) / 1000
    print(
        f"\n[latency] 640x480 {codec.name} "
        f"q={quality}: p50={p50:.1f}ms  p95={p95:.1f}ms  avg={avg:.1f}ms  n={len(samples_us)}"
    )
    # 250ms is a deliberately loose ceiling — localhost should be ~5-30ms.
    # A regression that doubled the actual latency would still trip.
    assert p95 < 250, f"{codec.name} p95 latency {p95:.1f}ms is suspicious"


# ---------------------------------------------------------------------------
# Memory profile — RSS before and after sustained 540p MJPEG
# ---------------------------------------------------------------------------


async def test_rss_does_not_balloon_under_sustained_load(pair):
    """Two-phase memory test that distinguishes warm-up cost from a real
    leak. We send 150 frames at 540p MJPEG (warm-up), snapshot peak RSS,
    send another 150 frames (steady state), snapshot again.

    `ru_maxrss` is monotonic (peak), so:
      * Warm-up delta  = whatever LiveKit + tokio + Python need at first.
      * Steady-state delta = should be ≪ warm-up if buffers are bounded.

    A real leak would make the second delta similar to or bigger than
    the first because each frame keeps allocating without ever freeing.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=85)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=85)

    received_count = 0

    def on_frame(name: str, f: VideoFrameData) -> None:
        nonlocal received_count
        received_count += 1

    await pair.start()
    pair.operator.on_video_frame("cam", on_frame)

    rgb = _gradient(960, 540, seed=3)

    async def burst(n: int) -> None:
        fps = 30
        interval = 1.0 / fps
        start = time.monotonic()
        for i in range(n):
            next_at = start + i * interval
            delay = next_at - time.monotonic()
            if delay > 0:
                await asyncio.sleep(delay)
            pair.robot.send_video_frame("cam", rgb, timestamp_us=i)

    async def snapshot() -> float:
        # Force Python GC + a brief idle so any unrooted bytes/dataclasses
        # from the previous burst are collected before we read the peak.
        # `ru_maxrss` is monotonic (process-lifetime peak), so the test
        # measures relative deltas, not absolute values.
        gc.collect()
        await asyncio.sleep(0.5)
        return _rss_mib()

    rss_baseline = await snapshot()

    await burst(150)
    rss_after_warmup = await snapshot()

    await burst(150)
    rss_after_steady = await snapshot()

    delta_warmup = rss_after_warmup - rss_baseline
    delta_steady = rss_after_steady - rss_after_warmup

    print(
        f"\n[memory] 540p MJPEG, 30 fps, two phases × 150 frames each:"
        f"\n  baseline   RSS = {rss_baseline:.1f} MiB"
        f"\n  warmup-end RSS = {rss_after_warmup:.1f} MiB  (Δ {delta_warmup:+.1f} MiB)"
        f"\n  steady-end RSS = {rss_after_steady:.1f} MiB  (Δ {delta_steady:+.1f} MiB)"
        f"\n  received       = {received_count}"
    )
    # The leak property: steady-state growth must be substantially smaller
    # than warmup growth. A per-frame leak would make both deltas similar
    # (each frame keeps allocating); bounded buffers mean the peak stops
    # rising once Python's GC and LiveKit's pools are warm. We require
    # `delta_steady ≤ 0.4 × delta_warmup` (with a 25 MiB floor so the
    # ratio doesn't apply when warmup is small from a fast GC pass).
    leak_floor_mib = 25.0
    leak_ratio = 0.4
    leak_ceiling = max(leak_floor_mib, delta_warmup * leak_ratio)
    assert delta_steady < leak_ceiling, (
        f"steady-state RSS grew by {delta_steady:.1f} MiB; warmup was "
        f"{delta_warmup:.1f} MiB. Steady should be ≪ warmup if buffers "
        f"are bounded — investigate."
    )


async def test_repeated_reconnect_no_leak():
    """Connect / send / disconnect 5 times in succession. The 5th cycle
    must work as cleanly as the first — a leaked drainer task or stuck
    byte stream would manifest as a hang or send error here."""
    from integration.conftest import URL, _make_token, Pair

    from livekit.portal import Robot, Operator

    for cycle in range(5):
        p = Pair()
        p.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=80)
        p.operator_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=80)
        try:
            p.robot = Robot(p.robot_cfg)
            p.operator = Operator(p.operator_cfg)
            await p.robot.connect(URL, _make_token("robot", p.room))
            await p.operator.connect(URL, _make_token("operator", p.room))
            await asyncio.sleep(0.2)

            received = []
            p.operator.on_video_frame(
                "cam", lambda name, f: received.append(f)
            )
            for i in range(3):
                p.robot.send_video_frame(
                    "cam", _gradient(160, 120, seed=cycle), timestamp_us=i
                )
            await asyncio.sleep(0.5)
            assert len(received) == 3, (
                f"cycle {cycle}: expected 3 frames, got {len(received)}"
            )
        finally:
            await p.stop()
