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

"""With observation sync off, raw streams still arrive and nothing is bundled.

Both video transports are covered, since each one used to hand frames to
the sync buffer on its way to the callbacks.
"""
from __future__ import annotations

import asyncio

import numpy as np
import pytest

from livekit.portal import DType, Operator, OperatorConfig, VideoCodec

from .conftest import URL, _make_token, wait_for

pytestmark = pytest.mark.asyncio


def _frame(seed: int) -> np.ndarray:
    return np.full((240, 320, 3), seed % 255, dtype=np.uint8)


def _declare(cfg) -> None:
    cfg.add_video("webrtc", codec=VideoCodec.VP8)
    cfg.add_video("bytes", codec=VideoCodec.RAW)


async def _stream(pair, n: int = 30) -> None:
    for i in range(n):
        pair.robot.send_video_frame("webrtc", _frame(i))
        pair.robot.send_video_frame("bytes", _frame(i))
        pair.robot.send_state({"j": float(i)})
        await asyncio.sleep(1 / 30)


async def test_raw_streams_arrive_without_bundling(pair):
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    pair.operator_cfg.set_observation_sync(False)
    frames: dict[str, int] = {"webrtc": 0, "bytes": 0}
    states = []
    drops = []
    await pair.start()
    for track in frames:
        pair.operator.on_video_frame(track, lambda name, f: frames.__setitem__(name, frames[name] + 1))
    pair.operator.on_state(lambda s: states.append(s))
    pair.operator.on_drop(lambda d: drops.append(d))

    await _stream(pair)

    assert await wait_for(lambda: all(n > 10 for n in frames.values()) and len(states) == 30)
    assert pair.operator.get_video_frame("webrtc") is not None
    assert pair.operator.get_video_frame("bytes") is not None
    sync = pair.operator.metrics().sync
    assert sync.observations_emitted == 0
    assert drops == []


async def test_drop_never_fires_when_a_camera_stalls(pair):
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        cfg.add_video("cam1", codec=VideoCodec.RAW)
        cfg.add_video("cam2", codec=VideoCodec.RAW)
    pair.operator_cfg.set_observation_sync(False)
    drops = []
    await pair.start()
    pair.operator.on_drop(lambda d: drops.append(d))

    # cam2 goes silent while cam1 and state keep going, which would drop
    # states on a peer that bundles.
    for i in range(20):
        pair.robot.send_video_frame("cam1", _frame(i), timestamp_us=1_000_000 + i * 33_000)
        pair.robot.send_state({"j": float(i)}, timestamp_us=1_000_000 + i * 33_000)
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.5)

    assert drops == []
    assert pair.operator.metrics().sync.states_dropped == 0


async def test_bundling_and_raw_peers_share_a_room(pair):
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    observations = []
    raw_frames = []
    await pair.start()
    pair.operator.on_observation(lambda o: observations.append(o))

    cfg = OperatorConfig(pair.room)
    cfg.add_state_typed([("j", DType.F32)])
    _declare(cfg)
    cfg.set_observation_sync(False)
    raw = Operator(cfg)
    try:
        await raw.connect(URL, _make_token("raw", pair.room))
        raw.on_video_frame("bytes", lambda name, f: raw_frames.append(f))
        assert await wait_for(lambda: "raw" in pair.robot.operators())

        await _stream(pair)

        assert await wait_for(lambda: observations and len(raw_frames) > 10)
        assert raw.metrics().sync.observations_emitted == 0
    finally:
        await raw.disconnect()
