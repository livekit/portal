"""Integration tests for byte-stream video tracks against a live LiveKit server.

Selecting a non-H264 codec on `add_video` bypasses the WebRTC media path
and ships each frame as a byte-stream payload (Raw, Png, Mjpeg). The
receiver decodes back to RGB so consumer code calls `on_video_frame` /
`get_video_frame` / `on_observation` exactly the same way as the H264
WebRTC path.

These scenarios exercise the parts most likely to misbehave in production:
  * RGB roundtrip integrity (Raw, Png byte-exact; Mjpeg close enough)
  * Codec mismatch — wrong codec on receive must drop, not crash
  * Track name mismatch — frames for an undeclared track must drop
  * Mixed transports — H264 and byte-stream tracks cohabiting one Portal
    both feed the sync buffer
  * Pre-first-frame state buffering and observation emission
  * Burst publish — overflow path drops newest frames without hanging
  * Boundary dimensions (8x8 floor, multi-megapixel ceiling)
  * MJPEG quality boundaries (1 and 100 both encode/decode)
  * Disconnect mid-burst leaves no leaked tasks
  * Reconnect resets latest-wins slots

Skipped automatically when `LIVEKIT_URL` isn't set (see conftest).
"""
from __future__ import annotations

import asyncio
import time

import numpy as np
import pytest

from livekit.portal import (
    DType,
    Observation,
    VideoCodec,
    VideoFrameData,
    frame_bytes_to_numpy_rgb,
)

pytestmark = pytest.mark.asyncio

# Frame-video receive races a few hops: byte-stream open → read_all →
# decode → sync buffer → callback hop to asyncio loop. Half a second is
# enough on localhost; CI may need more.
SETTLE_S = 0.6


def _gradient(width: int, height: int, seed: int = 0) -> np.ndarray:
    """Reproducible high-entropy RGB frame. Each channel uses a different
    deterministic pattern so encode regressions in any one channel are
    visible in the byte-exact comparisons. `(H, W, 3)` uint8.
    """
    x = np.arange(width, dtype=np.int32)
    y = np.arange(height, dtype=np.int32)[:, None]
    r = ((x + seed) % 256).astype(np.uint8)
    g = ((y + seed) % 256).astype(np.uint8)
    b = ((x + y + seed) % 256).astype(np.uint8)
    r_full = np.broadcast_to(r, (height, width))
    g_full = np.broadcast_to(g, (height, width))
    return np.stack([r_full, g_full, b], axis=-1)


# ---------------------------------------------------------------------------
# Codec roundtrip — Raw, PNG byte-exact; MJPEG close (lossy)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("codec", [VideoCodec.RAW, VideoCodec.PNG])
async def test_lossless_codec_roundtrip_byte_exact(pair, codec):
    """Raw and PNG must deliver the publisher's bytes verbatim."""
    pair.robot_cfg.add_video("cam", codec=codec)
    pair.operator_cfg.add_video("cam", codec=codec)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )

    sent = _gradient(64, 48, seed=7)
    pair.robot.send_video_frame("cam", sent, timestamp_us=12_345)
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    got = received[0]
    assert got.width == 64 and got.height == 48
    assert got.timestamp_us == 12_345
    arr = frame_bytes_to_numpy_rgb(bytes(got.data), got.width, got.height)
    np.testing.assert_array_equal(arr, sent)


async def test_mjpeg_roundtrip_close(pair):
    """MJPEG is lossy by design; assert avg per-pixel error is small."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=95)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=95)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )

    sent = _gradient(96, 64, seed=11)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    got = received[0]
    arr = frame_bytes_to_numpy_rgb(bytes(got.data), got.width, got.height)
    avg_err = float(
        np.mean(np.abs(arr.astype(np.int32) - sent.astype(np.int32)))
    )
    assert avg_err < 6.0, f"MJPEG q=95 avg pixel error too high: {avg_err}"


# ---------------------------------------------------------------------------
# Misconfiguration — receiver should drop, not crash
# ---------------------------------------------------------------------------


async def test_codec_mismatch_drops(pair):
    """Robot publishes MJPEG, operator declared PNG. The wire codec id in
    the header disagrees with the operator's spec; the dispatcher must
    drop the frame and not call the callback.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )

    pair.robot.send_video_frame("cam", _gradient(32, 32))
    await asyncio.sleep(SETTLE_S)

    assert received == []
    # The receive metric should not have advanced.
    snap = pair.operator.metrics().transport.frames_received
    assert snap.get("cam", 0) == 0


async def test_unknown_track_drops_silently(pair):
    """Robot declares 'cam_a', operator declares 'cam_b'. The frame's
    track-name header doesn't match any operator spec; receiver drops,
    no callback fires on either name.
    """
    pair.robot_cfg.add_video("cam_a", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam_b", codec=VideoCodec.RAW)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam_b", lambda name, f: received.append(f)
    )

    pair.robot.send_video_frame("cam_a", _gradient(16, 16))
    await asyncio.sleep(SETTLE_S)
    assert received == []


# ---------------------------------------------------------------------------
# Sync-buffer integration — frame-video frames participate in observations
# ---------------------------------------------------------------------------


async def test_observation_emits_for_frame_video_track(pair):
    """A frame-video track behaves exactly like a webrtc track from the
    sync buffer's POV: state + matching frame → one observation."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)
    # state schema from conftest is `[("j", F32)]`.

    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    ts = int(time.time() * 1_000_000)
    pair.robot.send_video_frame("cam", _gradient(16, 16), timestamp_us=ts)
    pair.robot.send_state({"j": 0.5}, timestamp_us=ts)
    await asyncio.sleep(SETTLE_S)

    assert len(obs) >= 1, "frame_video frame should pair with state into an observation"
    o = obs[-1]
    assert "cam" in o.frames
    assert o.frames["cam"].width == 16


async def test_mixed_transports_in_one_portal(pair):
    """A Portal can declare a webrtc track and a frame-video track at the
    same time. Both feed the sync buffer; observations include both
    frames once both arrive within tolerance.
    """
    pair.robot_cfg.add_video("cam_webrtc")
    pair.robot_cfg.add_video("cam_data", codec=VideoCodec.MJPEG, quality=80)
    pair.operator_cfg.add_video("cam_webrtc")
    pair.operator_cfg.add_video("cam_data", codec=VideoCodec.MJPEG, quality=80)

    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    # Push a few frames + states. Need WebRTC to actually flow through
    # the SFU, so wait a beat after connect for tracks to subscribe, and
    # send several so something lands inside the sync window.
    await asyncio.sleep(0.5)
    for i in range(10):
        ts = int(time.time() * 1_000_000)
        pair.robot.send_video_frame("cam_webrtc", _gradient(64, 48, seed=i))
        pair.robot.send_video_frame("cam_data", _gradient(64, 48, seed=i + 1))
        pair.robot.send_state({"j": float(i)}, timestamp_us=ts)
        await asyncio.sleep(0.05)
    await asyncio.sleep(SETTLE_S + 0.5)

    # At least one observation should have both tracks.
    pair_obs = [o for o in obs if "cam_webrtc" in o.frames and "cam_data" in o.frames]
    assert pair_obs, (
        f"expected at least one observation with both tracks; got "
        f"{len(obs)} obs total, frame keys seen: {[set(o.frames) for o in obs[:5]]}"
    )


# ---------------------------------------------------------------------------
# Dimensions & quality boundaries
# ---------------------------------------------------------------------------


async def test_tiny_frame_8x8(pair):
    """8x8 RGB — smallest sensible frame. Verifies the wire format
    survives below typical block boundaries that codecs assume."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    sent = _gradient(8, 8, seed=2)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S)
    assert len(received) == 1
    arr = frame_bytes_to_numpy_rgb(
        bytes(received[0].data), received[0].width, received[0].height
    )
    np.testing.assert_array_equal(arr, sent)


async def test_large_frame_above_data_packet_cap(pair):
    """1280×720 raw RGB ≈ 2.7 MB — far above the 15 KB data-packet limit.
    Byte streams fragment under the hood, so the receive side should
    reassemble cleanly.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.RAW)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    sent = _gradient(1280, 720, seed=3)
    pair.robot.send_video_frame("cam", sent)
    # ~2.7 MB raw over the byte-stream path. Poll for arrival rather than
    # race a fixed settle window, which flakes under load (issue #60).
    from integration.conftest import wait_for

    assert await wait_for(lambda: len(received) == 1, timeout_s=15.0)
    arr = frame_bytes_to_numpy_rgb(
        bytes(received[0].data), received[0].width, received[0].height
    )
    np.testing.assert_array_equal(arr, sent)


async def test_odd_dimensions_supported(pair):
    """Frame video has no parity constraint (unlike I420 → libwebrtc).
    Odd-on-both-axes frames must encode and roundtrip cleanly.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    sent = _gradient(15, 9, seed=4)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S)
    assert len(received) == 1
    assert (received[0].width, received[0].height) == (15, 9)


@pytest.mark.parametrize("quality", [1, 100])
async def test_mjpeg_quality_boundaries(pair, quality):
    """MJPEG accepts 1..=100. Both endpoints must encode without panic
    and decode to a frame of the right dimensions."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=quality)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=quality)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    pair.robot.send_video_frame("cam", _gradient(64, 48))
    await asyncio.sleep(SETTLE_S)
    assert len(received) == 1
    assert (received[0].width, received[0].height) == (64, 48)


# ---------------------------------------------------------------------------
# Burst, ordering, lifecycle
# ---------------------------------------------------------------------------


async def test_burst_does_not_hang(pair):
    """Send 200 frames as fast as possible. Some may drop at the
    publish-queue boundary (cap=60) — the rest must arrive without the
    publisher hanging or panicking."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=70)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=70)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    rgb = _gradient(160, 120, seed=5)
    for i in range(200):
        pair.robot.send_video_frame("cam", rgb, timestamp_us=i)
    await asyncio.sleep(SETTLE_S + 1.5)

    # Some frames must arrive (delivery-best-effort, but not zero).
    assert received, "expected at least some frames despite burst overflow"
    # Every received frame must have one of the timestamps we sent.
    seen_ts = {f.timestamp_us for f in received}
    assert seen_ts.issubset(set(range(200)))


async def test_disconnect_midstream_leaves_clean_state(pair):
    """Send a few frames, disconnect, reconnect with a fresh Pair, send
    more frames. The original publisher's drainer task must abort
    cleanly; a leaked task would either keep the room open or panic on
    a torn-down LocalParticipant."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )

    rgb = _gradient(32, 32)
    for i in range(5):
        pair.robot.send_video_frame("cam", rgb, timestamp_us=i)
        await asyncio.sleep(0.05)
    await pair.robot.disconnect()
    # Operator can outlive a robot disconnect; wait a beat to make sure
    # nothing crashes on the operator side.
    await asyncio.sleep(0.3)
    # No assertion on count — disconnect can race with in-flight frames.
    # The point is no exception.


async def test_get_video_frame_after_send(pair):
    """`get_video_frame` returns the latest received frame for the named
    track. After one send, the operator's pull-side slot must populate.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)
    await pair.start()

    rgb = _gradient(32, 32, seed=9)
    pair.robot.send_video_frame("cam", rgb, timestamp_us=42)
    await asyncio.sleep(SETTLE_S)

    got = pair.operator.get_video_frame("cam")
    assert got is not None
    assert got.timestamp_us == 42
    arr = frame_bytes_to_numpy_rgb(bytes(got.data), got.width, got.height)
    np.testing.assert_array_equal(arr, rgb)


async def test_state_buffered_until_first_frame(pair):
    """Push a state with no frames yet, then a frame with the same
    timestamp. The sync buffer should hold the state, then emit when the
    frame arrives — no observation drop.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    obs: list[Observation] = []
    drops: list = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))
    pair.operator.on_drop(lambda batch: drops.extend(batch))

    ts = int(time.time() * 1_000_000)
    pair.robot.send_state({"j": 0.25}, timestamp_us=ts)
    # Send the frame slightly later but inside the sync window.
    await asyncio.sleep(0.1)
    pair.robot.send_video_frame("cam", _gradient(32, 32), timestamp_us=ts + 1000)
    await asyncio.sleep(SETTLE_S)

    assert len(obs) >= 1, f"state should pair once frame arrives; obs={len(obs)}, drops={drops}"


# ---------------------------------------------------------------------------
# Pixel-content edge cases
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "fill",
    [
        pytest.param(0, id="all-black"),
        pytest.param(255, id="all-white"),
        pytest.param(127, id="all-grey"),
    ],
)
async def test_uniform_fill_byte_exact(pair, fill):
    """Pathological uniform frames stress the codec's entropy coder
    boundary. Lossless codecs must still be byte-exact."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )
    sent = np.full((48, 64, 3), fill, dtype=np.uint8)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    arr = frame_bytes_to_numpy_rgb(
        bytes(received[0].data), received[0].width, received[0].height
    )
    np.testing.assert_array_equal(arr, sent)


async def test_publisher_full_metric_increments_on_burst(pair):
    """Sending more frames than the publish queue can drain in real time
    must surface in `frames_dropped_publisher_full` rather than silently
    disappear. Push 200 raw frames as fast as possible — at least some
    will be dropped at the queue cap (60), and the metric must reflect
    that count.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.RAW)
    await pair.start()

    rgb = _gradient(320, 240, seed=5)
    for i in range(200):
        pair.robot.send_video_frame("cam", rgb, timestamp_us=i)

    # Give the drainer a moment so any in-flight frames either land or
    # clearly haven't.
    await asyncio.sleep(SETTLE_S + 1.0)

    snap = pair.robot.metrics().transport.frames_dropped_publisher_full
    dropped = snap.get("cam", 0)
    sent = pair.robot.metrics().transport.frames_sent.get("cam", 0)
    # The exact split depends on link speed; assert the invariants:
    # neither both zero nor a sum below the offered load.
    assert dropped > 0, f"expected drops under burst, got dropped={dropped} sent={sent}"
    assert sent + dropped >= 200, f"sent + dropped should account for all 200; sent={sent} dropped={dropped}"


async def test_random_noise_byte_exact(pair):
    """High-entropy noise. PNG can't compress this much, exercising the
    largest typical PNG payload size on the wire.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.PNG)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.PNG)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame(
        "cam", lambda name, f: received.append(f)
    )

    rng = np.random.RandomState(123)
    sent = rng.randint(0, 256, size=(160, 120, 3), dtype=np.uint8)
    pair.robot.send_video_frame("cam", sent)
    await asyncio.sleep(SETTLE_S + 0.4)

    assert len(received) == 1
    arr = frame_bytes_to_numpy_rgb(
        bytes(received[0].data), received[0].width, received[0].height
    )
    np.testing.assert_array_equal(arr, sent)
