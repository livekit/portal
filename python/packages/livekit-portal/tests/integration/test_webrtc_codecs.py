# Copyright 2026 LiveKit, Inc.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Integration tests for the WebRTC video codecs against a live LiveKit server.

`add_video` with a WebRTC codec (`H264`, `VP8`, `VP9`, `AV1`, `H265`) rides
the WebRTC media path: libwebrtc encodes, the SFU forwards, libwebrtc decodes
back to RGB on the receiver. `max_bitrate_kbps` caps the encoder's peak rate.

These scenarios confirm the codec actually negotiates end-to-end and frames
flow — the publish path maps the Portal codec to the libwebrtc codec, so a
mapping regression would show up as zero received frames.

Skipped automatically when `LIVEKIT_URL` isn't set (see conftest).
"""
from __future__ import annotations

import asyncio
import time

import numpy as np
import pytest

from livekit.portal import VideoCodec, VideoFrameData

pytestmark = pytest.mark.asyncio

# WebRTC receive races connect → subscribe → SFU forward → decode → callback.
# Give the track time to subscribe, then send a burst so something lands.
SUBSCRIBE_SETTLE_S = 0.5
DRAIN_S = 1.0


def _gradient(width: int, height: int, seed: int = 0) -> np.ndarray:
    x = np.arange(width, dtype=np.int32)
    y = np.arange(height, dtype=np.int32)[:, None]
    r = ((x + seed) % 256).astype(np.uint8)
    g = ((y + seed) % 256).astype(np.uint8)
    b = ((x + y + seed) % 256).astype(np.uint8)
    return np.stack(
        [np.broadcast_to(r, (height, width)), np.broadcast_to(g, (height, width)), b],
        axis=-1,
    )


# VP8/VP9/H264 are broadly supported by libwebrtc builds. AV1/H265 negotiation
# is platform- and build-dependent, so they're excluded from the must-pass
# matrix to keep the test deterministic.
@pytest.mark.parametrize("codec", [VideoCodec.H264, VideoCodec.VP8, VideoCodec.VP9])
async def test_webrtc_codec_frames_flow(pair, codec):
    """Each WebRTC codec must publish, negotiate, and deliver frames."""
    pair.robot_cfg.add_video("cam", codec=codec)
    pair.operator_cfg.add_video("cam", codec=codec)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame("cam", lambda name, f: received.append(f))

    # 320x240: above libwebrtc's minimum encode resolution. (VP8 in
    # particular rescales sub-QVGA frames, so a 64x48 source would come
    # back resized — irrelevant to whether the codec negotiates.)
    await asyncio.sleep(SUBSCRIBE_SETTLE_S)
    for i in range(15):
        pair.robot.send_video_frame("cam", _gradient(320, 240, seed=i))
        await asyncio.sleep(0.05)
    await asyncio.sleep(DRAIN_S)

    assert received, f"no frames received for codec {codec!r}"
    assert received[0].width > 0 and received[0].height > 0


async def test_h264_with_max_bitrate_cap_flows(pair):
    """An H264 track with an explicit bitrate ceiling still publishes and
    delivers frames — the cap is a ceiling, not a hard target."""
    pair.robot_cfg.add_video("cam", codec=VideoCodec.H264, max_bitrate_kbps=2000)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.H264)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame("cam", lambda name, f: received.append(f))

    await asyncio.sleep(SUBSCRIBE_SETTLE_S)
    for i in range(15):
        pair.robot.send_video_frame("cam", _gradient(320, 240, seed=i))
        await asyncio.sleep(0.05)
    await asyncio.sleep(DRAIN_S)

    assert received, "no frames received for bitrate-capped H264 track"


@pytest.mark.parametrize(
    "flags",
    [
        pytest.param({"screencast": True}, id="screencast"),
        pytest.param({"simulcast": True}, id="simulcast"),
        pytest.param({"simulcast": True, "screencast": True}, id="both"),
    ],
)
async def test_encoder_toggles_publish_and_deliver(pair, flags):
    """`simulcast` and `screencast` must not break publish or negotiation.

    `screencast` flips the libwebrtc content-type hint, which decides the
    degradation preference. `simulcast` changes the number of published
    encodings. Both alter `TrackPublishOptions` and the track source, so a
    regression in either shows up here as zero received frames.

    Only the publisher sets the flags. They are encoder-side, so the
    subscriber declares a plain track and must still decode normally.
    """
    pair.robot_cfg.add_video("cam", codec=VideoCodec.H264, **flags)
    pair.operator_cfg.add_video("cam", codec=VideoCodec.H264)

    received: list[VideoFrameData] = []
    await pair.start()
    pair.operator.on_video_frame("cam", lambda name, f: received.append(f))

    await asyncio.sleep(SUBSCRIBE_SETTLE_S)
    for i in range(15):
        pair.robot.send_video_frame("cam", _gradient(320, 240, seed=i))
        await asyncio.sleep(0.05)
    await asyncio.sleep(DRAIN_S)

    assert received, f"no frames received with {flags}"
    assert received[0].width > 0 and received[0].height > 0
