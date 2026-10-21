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

"""RrdSink writes the documented layout; read back with Rerun's RrdReader."""
from __future__ import annotations

import json
import re
import subprocess
import sys

import numpy as np
import pytest

rr = pytest.importorskip("rerun")
pa = pytest.importorskip("pyarrow")
from rerun.chunk import RrdReader  # noqa: E402

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
from livekit.portal.recording import RrdSink  # noqa: E402

US = 1_000_000


def _session(**overrides) -> SessionInfo:
    fields = dict(
        session="lab room/1",
        local_identity="recorder",
        state_schema=[FieldSpec(name="shoulder", dtype=DType.F32), FieldSpec(name="elbow", dtype=DType.F64)],
        action_schema=[FieldSpec(name="a", dtype=DType.F32)],
        video_tracks=[VideoTrackSpec(name="cam", codec=VideoCodec.RAW, quality=90, max_bitrate_kbps=None, simulcast=False, screencast=False)],
        time_sync_source=TimeSyncSource.SYSTEM,
        started_at_us=1_790_000_000 * US,
        now_us=lambda: 9 * US,
    )
    fields.update(overrides)
    return SessionInfo(**fields)


def _frame(ts: int, value: int = 7) -> VideoFrameData:
    data = np.full((4, 6, 3), value, dtype=np.uint8)
    return VideoFrameData(width=6, height=4, data=data.tobytes(), timestamp_us=ts, source=FrameSource.LIVE)


def _action(ts: int, reply, sender="teleop", active=True, a=1.0) -> Action:
    return Action(values={"a": a}, raw_values={"a": a}, timestamp_us=ts, sender=sender, active=active, in_reply_to_ts_us=reply)


def _read(path) -> dict[str, list[dict]]:
    rows: dict[str, list[dict]] = {}
    for chunk in RrdReader(path).store().stream().to_chunks():
        batch = chunk.to_record_batch()
        # Timestamps come back as integer nanoseconds: Arrow can't turn a
        # nanosecond timestamp into a datetime without pandas.
        cols = {
            name: (col.cast(pa.int64()) if pa.types.is_timestamp(col.type) else col).to_pylist()
            for name, col in zip(batch.schema.names, batch.columns)
        }
        for i in range(batch.num_rows):
            rows.setdefault(str(chunk.entity_path), []).append({k: v[i] for k, v in cols.items()})
    return rows


def _us(row, timeline):
    return row[timeline] // 1_000


def _record(tmp_path, jpeg_quality=95):
    sink = RrdSink(tmp_path, jpeg_quality=jpeg_quality)
    sink.open(_session())
    sink.write_state(State(values={}, raw_values={"shoulder": 0.5, "elbow": -1.0}, timestamp_us=1 * US))
    sink.write_frame("cam", _frame(1 * US))
    # Two actions answering the same frame, one shadow action, one unsolicited.
    sink.write_action(_action(1 * US + 10, reply=1 * US, a=1.0))
    sink.write_action(_action(1 * US + 20, reply=1 * US, a=2.0))
    sink.write_action(_action(1 * US + 30, reply=1 * US, sender="policy", active=False, a=3.0))
    sink.write_action(_action(2 * US, reply=None, a=4.0))
    sink.write_keypoint(Keypoint(type="recording", payload={"task_description": "stack"}, timestamp_us=1 * US, sender="teleop"))
    sink.write_metrics(Operator(OperatorConfig("x")).metrics())
    sink.close()
    return sink.path, _read(sink.path)


def test_file_is_named_after_session_and_start(tmp_path):
    path, _ = _record(tmp_path)
    assert re.fullmatch(r"lab_room_1-\d{8}T\d{6}\.rrd", path.split("/")[-1])


def test_schema_records_field_order_and_clock(tmp_path):
    _, rows = _record(tmp_path)
    (row,) = rows["/portal/schema"]
    schema = json.loads(row["TextDocument:text"][0])
    assert [f["name"] for f in schema["state"]] == ["shoulder", "elbow"]
    assert [f["dtype"] for f in schema["state"]] == ["f32", "f64"]
    assert schema["video"] == [{"name": "cam", "codec": "raw"}]
    assert schema["time_sync_source"] == "system"
    assert schema["recorder"] == "recorder"


def test_state_and_frames_are_on_robot_time(tmp_path):
    _, rows = _record(tmp_path)
    for entity, value in (("/observation/shoulder", 0.5), ("/observation/elbow", -1.0)):
        (row,) = rows[entity]
        assert (_us(row, "robot_time"), row["Scalars:scalars"]) == (1 * US, [value])
    (frame,) = rows["/observation/cam"]
    assert _us(frame, "robot_time") == 1 * US
    assert frame["EncodedImage:media_type"] == ["image/jpeg"]


def test_every_action_survives_on_action_time(tmp_path):
    _, rows = _record(tmp_path)
    teleop = rows["/action/teleop/a"]
    assert [_us(r, "action_time") for r in teleop] == [1 * US + 10, 1 * US + 20, 2 * US]
    # The two answers to the frame at 1 s share that robot_time; the
    # unsolicited action falls back to its own timestamp.
    assert [_us(r, "robot_time") for r in teleop] == [1 * US, 1 * US, 2 * US]
    assert [r["Scalars:scalars"] for r in teleop] == [[1.0], [2.0], [4.0]]
    (shadow,) = rows["/action/policy/active"]
    assert shadow["Scalars:scalars"] == [0.0]
    assert [r["Scalars:scalars"] for r in rows["/action/teleop/active"]] == [[1.0]] * 3


def test_keypoints_keep_payload_and_sender(tmp_path):
    _, rows = _record(tmp_path)
    (kp,) = rows["/keypoints/recording"]
    assert _us(kp, "robot_time") == 1 * US
    assert json.loads(kp["payload"][0]) == {"task_description": "stack"}
    assert kp["sender"] == ["teleop"]


def test_metrics_use_the_observer_clock(tmp_path):
    _, rows = _record(tmp_path)
    offsets = rows["/metrics/time_sync/offset_us"]
    assert [_us(r, "robot_time") for r in offsets] == [9 * US]


def test_quality_zero_stores_raw_pixels(tmp_path):
    _, rows = _record(tmp_path, jpeg_quality=0)
    (frame,) = rows["/observation/cam"]
    assert "Image:buffer" in frame
    assert bytes(frame["Image:buffer"][0]) == bytes([7]) * (4 * 6 * 3)


def test_invalid_quality_is_rejected(tmp_path):
    with pytest.raises(ValueError):
        RrdSink(tmp_path, jpeg_quality=101)


def test_recording_module_imports_without_rerun():
    code = (
        "import sys; sys.modules['rerun'] = None\n"
        "import livekit.portal.recording as r\n"
        "try:\n"
        "    r.RrdSink\n"
        "except ImportError as e:\n"
        "    print('ImportError:', e)\n"
    )
    out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, check=True).stdout
    assert "livekit-portal[rerun]" in out
