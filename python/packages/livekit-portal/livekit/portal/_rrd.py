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

"""`RrdSink`: one Rerun `.rrd` archive per recording."""
from __future__ import annotations

import datetime
import io
import json
import os
import re
from typing import TYPE_CHECKING, Optional

import numpy as np

try:
    import rerun as rr
    from PIL import Image
except ImportError as e:  # pragma: no cover - exercised without the extra
    raise ImportError(
        "RrdSink needs Rerun: install it with `pip install 'livekit-portal[rerun]'`"
    ) from e

if TYPE_CHECKING:
    from . import Action, Keypoint, PortalMetrics, State, VideoFrameData
    from ._recording import SessionInfo

SCHEMA_VERSION = 1


def _ts(us: int) -> np.datetime64:
    return np.datetime64(int(us), "us")


def _part(name: str) -> str:
    return rr.escape_entity_path_part(name)


class RrdSink:
    """Writes a recording to `<directory>/<session>-<start time>.rrd`.

    Two timelines, both on the robot's clock:

    - `robot_time` lines up an action with the frame it answered, so the
      frame, the state and the response share a row.
    - `action_time` keeps every action. Rerun keeps one value per entity per
      timestamp, so two actions answering the same frame would overwrite
      each other on `robot_time` alone.

    | Entity | `robot_time` | `action_time` |
    |---|---|---|
    | `observation/<field>` | state timestamp | |
    | `observation/<track>` | frame timestamp (JPEG, or raw) | |
    | `action/<sender>/<field>`, `action/<sender>/active` | `in_reply_to_ts_us`, else the action's own timestamp | action timestamp |
    | `keypoints/<type>` | keypoint timestamp (`payload` as JSON, `sender`) | |
    | `metrics/...` | observer's `now_us()` | |
    | `portal/schema` | static: field order and dtypes, tracks, clock source | |

    `jpeg_quality=0` stores frames uncompressed.
    """

    def __init__(self, directory: "str | os.PathLike[str]", jpeg_quality: int = 95) -> None:
        if not 0 <= jpeg_quality <= 100:
            raise ValueError("jpeg_quality must be between 0 and 100")
        self.directory = os.fspath(directory)
        self.jpeg_quality = jpeg_quality
        self.path: Optional[str] = None
        self._rec: Optional[rr.RecordingStream] = None
        self._now_us = None

    def open(self, session: "SessionInfo") -> None:
        os.makedirs(self.directory, exist_ok=True)
        started = datetime.datetime.fromtimestamp(
            session.started_at_us / 1_000_000, tz=datetime.timezone.utc
        )
        stem = f"{re.sub(r'[^A-Za-z0-9._-]+', '_', session.session)}-{started:%Y%m%dT%H%M%S}"
        self.path = os.path.join(self.directory, f"{stem}.rrd")
        self._rec = rr.RecordingStream("livekit-portal", recording_id=stem)
        self._rec.save(self.path)
        self._now_us = session.now_us
        self._rec.log(
            "portal/schema",
            rr.TextDocument(json.dumps(_schema(session), indent=2), media_type="application/json"),
            static=True,
        )

    def write_state(self, state: "State") -> None:
        rec = self._stream()
        rec.reset_time()
        rec.set_time("robot_time", timestamp=_ts(state.timestamp_us))
        for name, value in state.raw_values.items():
            rec.log(f"observation/{_part(name)}", rr.Scalars(value))

    def write_frame(self, track: str, frame: "VideoFrameData") -> None:
        rec = self._stream()
        rec.reset_time()
        rec.set_time("robot_time", timestamp=_ts(frame.timestamp_us))
        pixels = np.frombuffer(frame.data, dtype=np.uint8).reshape(frame.height, frame.width, 3)
        if self.jpeg_quality == 0:
            image = rr.Image(pixels)
        else:
            buf = io.BytesIO()
            Image.fromarray(pixels).save(buf, format="JPEG", quality=self.jpeg_quality)
            image = rr.EncodedImage(contents=buf.getvalue(), media_type="image/jpeg")
        rec.log(f"observation/{_part(track)}", image)

    def write_action(self, action: "Action") -> None:
        rec = self._stream()
        rec.reset_time()
        anchor = action.in_reply_to_ts_us
        rec.set_time("robot_time", timestamp=_ts(action.timestamp_us if anchor is None else anchor))
        rec.set_time("action_time", timestamp=_ts(action.timestamp_us))
        prefix = f"action/{_part(action.sender)}"
        for name, value in action.raw_values.items():
            rec.log(f"{prefix}/{_part(name)}", rr.Scalars(value))
        rec.log(f"{prefix}/active", rr.Scalars(1.0 if action.active else 0.0))

    def write_keypoint(self, keypoint: "Keypoint") -> None:
        rec = self._stream()
        rec.reset_time()
        rec.set_time("robot_time", timestamp=_ts(keypoint.timestamp_us))
        rec.log(
            f"keypoints/{_part(keypoint.type)}",
            rr.AnyValues(payload=json.dumps(keypoint.payload), sender=keypoint.sender),
        )

    def write_metrics(self, metrics: "PortalMetrics") -> None:
        rec = self._stream()
        rec.reset_time()
        rec.set_time("robot_time", timestamp=_ts(self._now_us()))
        rows = {
            "rtt_us": metrics.rtt.rtt_us_last,
            "time_sync/offset_us": metrics.time_sync.offset_us,
            "time_sync/uncertainty_us": metrics.time_sync.uncertainty_us,
            "transport/states_received": metrics.transport.states_received,
            "transport/actions_received": metrics.transport.actions_received,
            "transport/frames_received": sum(metrics.transport.frames_received.values()),
        }
        for name, value in rows.items():
            if value is not None:
                rec.log(f"metrics/{name}", rr.Scalars(float(value)))

    def close(self) -> None:
        rec = self._rec
        if rec is None:
            return
        self._rec = None
        rec.flush()
        rec.disconnect()

    def _stream(self) -> "rr.RecordingStream":
        if self._rec is None:
            raise RuntimeError("RrdSink is not open")
        return self._rec


def _schema(session: "SessionInfo") -> dict:
    return {
        "version": SCHEMA_VERSION,
        "session": session.session,
        "recorder": session.local_identity,
        "started_at_us": session.started_at_us,
        "time_sync_source": session.time_sync_source.name.lower(),
        "state": [{"name": f.name, "dtype": f.dtype.name.lower()} for f in session.state_schema],
        "action": [{"name": f.name, "dtype": f.dtype.name.lower()} for f in session.action_schema],
        "video": [{"name": t.name, "codec": t.codec.name.lower()} for t in session.video_tracks],
    }
