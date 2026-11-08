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

"""LanceSink writes the documented tables; read back with Lance and PyAV."""
from __future__ import annotations

import io
import json
import re
import subprocess
import sys

import numpy as np
import pytest

lance = pytest.importorskip("lance")
av = pytest.importorskip("av")

from livekit.portal import (  # noqa: E402
    Action,
    DType,
    FieldSpec,
    FrameSource,
    Keypoint,
    Operator,
    OperatorConfig,
    State,
    TimeSyncSource,
    VideoCodec,
    VideoFrameData,
    VideoTrackSpec,
)
from livekit.portal._recording import SessionInfo  # noqa: E402
from livekit.portal.recording import LanceSink  # noqa: E402

US = 1_000_000
TABLES = {"states", "actions", "keypoints", "videos", "frames", "frames_dropped", "metrics"}


def _session(**overrides) -> SessionInfo:
    fields = dict(
        session="lab room/1",
        local_identity="recorder",
        state_schema=[FieldSpec(name="shoulder", dtype=DType.F32), FieldSpec(name="elbow", dtype=DType.F64)],
        action_schema=[FieldSpec(name="a", dtype=DType.F32)],
        video_tracks=[VideoTrackSpec(name="cam", codec=VideoCodec.H264, quality=90, max_bitrate_kbps=None, simulcast=False, screencast=False)],
        time_sync_source=TimeSyncSource.SYSTEM,
        started_at_us=1_790_000_000 * US,
        now_us=lambda: 9 * US,
    )
    fields.update(overrides)
    return SessionInfo(**fields)


def _frame(ts: int, value: int = 100, width: int = 32, height: int = 24) -> VideoFrameData:
    data = np.full((height, width, 3), value, dtype=np.uint8)
    return VideoFrameData(width=width, height=height, data=data.tobytes(), timestamp_us=ts, source=FrameSource.LIVE)


def _action(ts: int, reply, sender="teleop", active=True, a=1.0) -> Action:
    return Action(values={"a": a}, raw_values={"a": a}, timestamp_us=ts, sender=sender, active=active, in_reply_to_ts_us=reply)


def _rows(path, table: str, **kwargs) -> list[dict]:
    return lance.dataset(f"{path}/{table}.lance").to_table(**kwargs).to_pylist()


def _decode(path, track: str, segment: int) -> list[np.ndarray]:
    ds = lance.dataset(f"{path}/videos.lance")
    ids = ds.to_table(columns=["track", "segment"], with_row_id=True).to_pylist()
    (row_id,) = [r["_rowid"] for r in ids if (r["track"], r["segment"]) == (track, segment)]
    (blob,) = ds.take_blobs("mp4", ids=[row_id])
    with av.open(io.BytesIO(blob.readall())) as container:
        return [f.to_ndarray(format="rgb24") for f in container.decode(video=0)]


def _record(tmp_path, **sink_args) -> str:
    sink = LanceSink(tmp_path, **sink_args)
    sink.open(_session())
    # Out of schema order: the table must follow the schema, not the dict.
    sink.write_state(State(values={}, raw_values={"elbow": -1.0, "shoulder": 0.5}, timestamp_us=1 * US))
    sink.write_frame("cam", _frame(1 * US, value=40))
    sink.write_frame("cam", _frame(1 * US + 33_000, value=200))
    sink.write_frame_dropped("cam", 1 * US + 66_000)
    # Two actions answering the same frame, one shadow action, one unsolicited.
    sink.write_action(_action(1 * US + 10, reply=1 * US, a=1.0))
    sink.write_action(_action(1 * US + 20, reply=1 * US, a=2.0))
    sink.write_action(_action(1 * US + 30, reply=1 * US, sender="policy", active=False, a=3.0))
    sink.write_action(_action(2 * US, reply=None, a=4.0))
    sink.write_keypoint(Keypoint(type="recording", payload={"task_description": "stack"}, timestamp_us=1 * US, sender="teleop"))
    sink.write_metrics(Operator(OperatorConfig("x")).metrics())
    sink.close()
    return sink.path


def test_directory_is_named_after_session_and_start(tmp_path):
    path = _record(tmp_path)
    assert re.fullmatch(r"lab_room_1-\d{8}T\d{6}", path.split("/")[-1])
    assert {p.name.removesuffix(".lance") for p in (tmp_path / path.split("/")[-1]).iterdir()} == TABLES


def test_every_table_carries_schema_and_session(tmp_path):
    path = _record(tmp_path)
    for table in TABLES:
        ds = lance.dataset(f"{path}/{table}.lance")
        schema = json.loads(ds.schema.metadata[b"portal"])
        assert [f["name"] for f in schema["state"]] == ["shoulder", "elbow"]
        assert [f["dtype"] for f in schema["state"]] == ["f32", "f64"]
        assert schema["time_sync_source"] == "system"
        assert {r["session"] for r in _rows(path, table, columns=["session"])} <= {"lab room/1"}


def test_state_values_follow_schema_order(tmp_path):
    (row,) = _rows(_record(tmp_path), "states")
    assert (row["timestamp_us"], row["values"]) == (1 * US, [0.5, -1.0])


def test_every_action_is_kept_with_its_anchor(tmp_path):
    rows = _rows(_record(tmp_path), "actions")
    assert [(r["timestamp_us"], r["in_reply_to_ts_us"], r["sender"], r["active"], r["values"]) for r in rows] == [
        (1 * US + 10, 1 * US, "teleop", True, [1.0]),
        (1 * US + 20, 1 * US, "teleop", True, [2.0]),
        (1 * US + 30, 1 * US, "policy", False, [3.0]),
        (2 * US, None, "teleop", True, [4.0]),
    ]


def test_keypoints_keep_payload_and_sender(tmp_path):
    (kp,) = _rows(_record(tmp_path), "keypoints")
    assert (kp["timestamp_us"], kp["type"], kp["sender"]) == (1 * US, "recording", "teleop")
    assert json.loads(kp["payload"]) == {"task_description": "stack"}


def test_frames_point_into_an_mp4_segment(tmp_path):
    path = _record(tmp_path)
    frames = _rows(path, "frames")
    assert [(f["track"], f["timestamp_us"], f["segment"], f["index"]) for f in frames] == [
        ("cam", 1 * US, 0, 0),
        ("cam", 1 * US + 33_000, 0, 1),
    ]
    (video,) = _rows(path, "videos", columns=["track", "segment", "start_us", "end_us", "width", "height", "frames"])
    assert video == {"track": "cam", "segment": 0, "start_us": 1 * US, "end_us": 1 * US + 33_000,
                     "width": 32, "height": 24, "frames": 2}
    decoded = _decode(path, "cam", 0)
    assert [f.shape for f in decoded] == [(24, 32, 3)] * 2
    assert [round(float(f.mean())) for f in decoded] == pytest.approx([40, 200], abs=3)


def test_segments_roll_over_on_time_and_shape(tmp_path):
    sink = LanceSink(tmp_path, segment_s=1.0)
    sink.open(_session())
    for i in range(4):   # 0.0 s .. 1.5 s: a second segment at 1.0 s
        sink.write_frame("cam", _frame(i * US // 2))
    sink.write_frame("cam", _frame(5 * US // 4))   # backwards in time: still kept
    sink.write_frame("cam", _frame(2 * US, width=16, height=12))   # new shape, new segment
    sink.write_frame("cam", _frame(2 * US))
    sink.close()
    videos = _rows(sink.path, "videos", columns=["segment", "frames", "width"])
    assert [(v["segment"], v["frames"], v["width"]) for v in videos] == [(0, 2, 32), (1, 3, 32), (2, 1, 16), (3, 1, 32)]
    frames = _rows(sink.path, "frames")
    assert [f["timestamp_us"] for f in frames if f["segment"] == 1] == [US, 3 * US // 2, 5 * US // 4]
    assert [len(_decode(sink.path, "cam", s)) for s in range(4)] == [2, 3, 1, 1]


def test_odd_frames_are_cropped_to_even(tmp_path):
    sink = LanceSink(tmp_path)
    sink.open(_session())
    sink.write_frame("cam", _frame(1 * US, width=33, height=25))
    sink.close()
    (frame,) = _decode(sink.path, "cam", 0)
    assert frame.shape == (24, 32, 3)


def test_dropped_frames_are_marked(tmp_path):
    (row,) = _rows(_record(tmp_path), "frames_dropped")
    assert (row["track"], row["timestamp_us"]) == ("cam", 1 * US + 66_000)


def test_metrics_use_the_observer_clock(tmp_path):
    (row,) = _rows(_record(tmp_path), "metrics")
    assert row["timestamp_us"] == 9 * US
    assert row["time_sync_offset_us"] == 0


def test_close_leaves_one_version_per_table(tmp_path, monkeypatch):
    monkeypatch.setattr("livekit.portal._lance.FLUSH_INTERVAL_S", 0.0)   # every write appends
    path = _record(tmp_path)
    for table in TABLES:
        ds = lance.dataset(f"{path}/{table}.lance")
        assert len(ds.versions()) == 1
        assert len(ds.get_fragments()) <= 1


def test_rows_are_readable_before_close(tmp_path, monkeypatch):
    monkeypatch.setattr("livekit.portal._lance.FLUSH_INTERVAL_S", 0.0)
    sink = LanceSink(tmp_path)
    sink.open(_session())
    sink.write_state(State(values={}, raw_values={"shoulder": 0.5, "elbow": -1.0}, timestamp_us=1 * US))
    assert len(_rows(sink.path, "states")) == 1
    sink.close()


def test_existing_recording_is_not_overwritten(tmp_path):
    _record(tmp_path)
    with pytest.raises(FileExistsError):
        LanceSink(tmp_path).open(_session())


@pytest.mark.parametrize("args", [{"crf": 52}, {"crf": -1}, {"segment_s": 0}])
def test_invalid_arguments_are_rejected(tmp_path, args):
    with pytest.raises(ValueError):
        LanceSink(tmp_path, **args)


def test_recording_module_imports_without_lance():
    code = (
        "import sys; sys.modules['lance'] = None\n"
        "import livekit.portal.recording as r\n"
        "try:\n"
        "    r.LanceSink\n"
        "except ImportError as e:\n"
        "    print('ImportError:', e)\n"
    )
    out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, check=True).stdout
    assert "livekit-portal[lance]" in out
