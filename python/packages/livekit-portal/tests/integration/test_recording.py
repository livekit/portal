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

"""An observer recording a live session to an in-memory sink."""
from __future__ import annotations

import asyncio

import numpy as np
import pytest

from livekit.portal import DType, Observer, ObserverConfig, TimeSyncSource, VideoCodec

from test_recorder import MemorySink
from .conftest import URL, _make_token, wait_for

pytestmark = pytest.mark.asyncio


def _declare(cfg) -> None:
    cfg.add_action_typed([("a", DType.F32)])
    cfg.add_video("cam", codec=VideoCodec.RAW)


async def test_observer_records_a_session(pair):
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    await pair.start()
    cfg = ObserverConfig(pair.room)
    cfg.add_state_typed([("j", DType.F32)])
    _declare(cfg)
    obs = Observer(cfg)
    sink = MemorySink()
    await obs.connect(URL, _make_token("recorder", pair.room))
    assert await wait_for(lambda: obs.active_operator() == "operator")
    obs.record_to(sink, metrics_interval_s=0.2)

    frame = np.zeros((24, 32, 3), dtype=np.uint8)
    await pair.operator.send_keypoint("recording", {"task_description": "stack"})
    for i in range(60):
        pair.robot.send_state({"j": float(i)})
        pair.robot.send_video_frame("cam", frame)
        pair.operator.send_action({"a": float(i)})
        await asyncio.sleep(1 / 60)
    await pair.operator.send_keypoint("idle")
    assert await wait_for(lambda: sink.names().count("keypoint") == 2 and sink.names().count("state") == 60)
    await asyncio.sleep(0.3)
    await obs.disconnect()

    names = sink.names()
    assert names[0] == "open" and names[-1] == "close"
    info = sink.calls[0][1]
    assert info.session == pair.room
    assert info.local_identity == "recorder"
    assert [f.name for f in info.state_schema] == ["j"]
    assert [t.name for t in info.video_tracks] == ["cam"]
    assert info.time_sync_source == TimeSyncSource.PORTAL

    by_kind = lambda kind: [c for c in sink.calls if c[0] == kind]
    assert len(by_kind("state")) == 60
    assert len(by_kind("frame")) >= 50
    assert len(by_kind("action")) >= 55
    assert {c[1].sender for c in by_kind("action")} == {"operator"}
    assert all(c[1].active for c in by_kind("action"))
    assert [c[1].type for c in by_kind("keypoint")] == ["recording", "idle"]
    assert len(by_kind("metrics")) >= 2
    m = obs.metrics().observer
    assert m.recording is False and m.frames_dropped == 0


async def test_observer_records_a_session_to_rrd(pair, tmp_path):
    pytest.importorskip("rerun")
    from livekit.portal.recording import RrdSink
    from test_rrd_sink import _read

    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    await pair.start()
    cfg = ObserverConfig(pair.room)
    cfg.add_state_typed([("j", DType.F32)])
    _declare(cfg)
    obs = Observer(cfg)
    sink = RrdSink(tmp_path)
    await obs.connect(URL, _make_token("recorder", pair.room))
    assert await wait_for(lambda: obs.active_operator() == "operator")
    obs.record_to(sink, metrics_interval_s=0.2)

    frame = np.zeros((24, 32, 3), dtype=np.uint8)
    await pair.operator.send_keypoint("recording", {"task_description": "stack"})
    for i in range(30):
        pair.robot.send_state({"j": float(i)})
        pair.robot.send_video_frame("cam", frame)
        pair.operator.send_action({"a": float(i)})
        await asyncio.sleep(1 / 60)
    await pair.operator.send_keypoint("idle")
    await asyncio.sleep(0.5)
    await obs.disconnect()

    rows = _read(sink.path)
    assert len(rows["/observation/j"]) == 30
    assert len(rows["/observation/cam"]) >= 25
    assert len(rows["/action/operator/a"]) >= 28
    assert set(rows) >= {"/keypoints/recording", "/keypoints/idle", "/portal/schema"}
    assert any(e.startswith("/metrics/") for e in rows)
