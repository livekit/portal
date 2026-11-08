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

"""`LanceSink`: one directory of Lance tables per recording."""
from __future__ import annotations

import datetime
import io
import json
import os
import time
from dataclasses import dataclass
from fractions import Fraction
from typing import TYPE_CHECKING, Any, Dict, List, Optional, Tuple

import numpy as np

try:
    import av
    import lance
    import pyarrow as pa
except ImportError as e:  # pragma: no cover - exercised without the extra
    raise ImportError(
        "LanceSink needs Lance and PyAV: install them with `pip install 'livekit-portal[lance]'`"
    ) from e

if TYPE_CHECKING:
    from . import Action, Keypoint, PortalMetrics, State, VideoFrameData
    from ._recording import SessionInfo

FLUSH_INTERVAL_S = 1.0
SCHEMA_KEY = b"portal"
_MICROS = Fraction(1, 1_000_000)


class LanceSink:
    """Writes a recording to `<directory>/<session>-<start time>/`, one Lance
    table per stream. Every row carries `session`, so recordings can be merged
    into one table later.

    | Table | Row |
    |---|---|
    | `states` | `timestamp_us`, `values` in schema order |
    | `actions` | `timestamp_us`, `in_reply_to_ts_us`, `sender`, `active`, `values` |
    | `keypoints` | `timestamp_us`, `type`, `sender`, `payload` (JSON) |
    | `videos` | one H.264 MP4 segment of a track: `track`, `segment`, `start_us`, `end_us`, `width`, `height`, `frames`, `mp4` (blob) |
    | `frames` | one per encoded frame: `track`, `timestamp_us`, `segment`, `index` within it |
    | `frames_dropped` | `track`, `timestamp_us` of a frame the recorder dropped |
    | `metrics` | observer's `now_us()`, then one column per metric |

    Every table's schema metadata holds the recording's field order and
    dtypes under `portal`. Rows are appended about once a second, and a
    track's frames land with its segment once the segment closes, so a crash
    loses the open segment and at most a second of everything else.

    Frames with an odd width or height lose their last column or row: H.264
    in 4:2:0 needs even dimensions.
    """

    def __init__(self, directory: "str | os.PathLike[str]", crf: int = 23, segment_s: float = 10.0) -> None:
        if not 0 <= crf <= 51:
            raise ValueError("crf must be between 0 and 51")
        if segment_s <= 0:
            raise ValueError("segment_s must be positive")
        self.directory = os.fspath(directory)
        self.crf = crf
        self.segment_s = segment_s
        self.path: Optional[str] = None
        self._session = ""
        self._state_fields: List[str] = []
        self._action_fields: List[str] = []
        self._tables: Dict[str, _Table] = {}
        self._segments: Dict[str, _Segment] = {}
        self._next_segment: Dict[str, int] = {}
        self._next_flush = 0.0
        self._now_us = None

    def open(self, session: "SessionInfo") -> None:
        self.path = os.path.join(self.directory, session.stem())
        os.makedirs(self.path)
        self._session = session.session
        self._state_fields = [f.name for f in session.state_schema]
        self._action_fields = [f.name for f in session.action_schema]
        self._now_us = session.now_us
        meta = {SCHEMA_KEY: json.dumps(session.describe()).encode()}
        tables = _schemas(len(session.state_schema), len(session.action_schema))
        self._tables = {
            name: _Table(os.path.join(self.path, f"{name}.lance"), pa.schema(fields, metadata=meta))
            for name, fields in tables.items()
        }
        self._next_flush = time.monotonic() + FLUSH_INTERVAL_S

    def write_state(self, state: "State") -> None:
        values = [state.raw_values[name] for name in self._state_fields]
        self._append("states", timestamp_us=state.timestamp_us, values=values)

    def write_action(self, action: "Action") -> None:
        self._append(
            "actions",
            timestamp_us=action.timestamp_us,
            in_reply_to_ts_us=action.in_reply_to_ts_us,
            sender=action.sender,
            active=action.active,
            values=[action.raw_values[name] for name in self._action_fields],
        )

    def write_keypoint(self, keypoint: "Keypoint") -> None:
        self._append(
            "keypoints",
            timestamp_us=keypoint.timestamp_us,
            type=keypoint.type,
            sender=keypoint.sender,
            payload=json.dumps(keypoint.payload),
        )

    def write_frame(self, track: str, frame: "VideoFrameData") -> None:
        pixels = np.frombuffer(frame.data, dtype=np.uint8).reshape(frame.height, frame.width, 3)
        pixels = pixels[: frame.height & ~1, : frame.width & ~1]
        segment = self._segments.get(track)
        if segment is not None and (
            segment.spec.shape != pixels.shape
            or frame.timestamp_us - segment.spec.start_us >= self.segment_s * 1_000_000
        ):
            self._finish(track)
            segment = None
        if segment is None:
            number = self._next_segment.get(track, 0)
            self._next_segment[track] = number + 1
            segment = self._segments[track] = _Segment(_SegmentSpec(number, pixels.shape, frame.timestamp_us, self.crf))
        segment.encode(pixels, frame.timestamp_us)
        self._maybe_flush()

    def write_frame_dropped(self, track: str, timestamp_us: int) -> None:
        self._append("frames_dropped", track=track, timestamp_us=timestamp_us)

    def write_metrics(self, metrics: "PortalMetrics") -> None:
        self._append(
            "metrics",
            timestamp_us=self._now_us(),
            rtt_us=metrics.rtt.rtt_us_last,
            time_sync_offset_us=metrics.time_sync.offset_us,
            time_sync_uncertainty_us=metrics.time_sync.uncertainty_us,
            states_received=metrics.transport.states_received,
            actions_received=metrics.transport.actions_received,
            actions_dropped_stale=metrics.transport.actions_dropped_stale,
            frames_received=sum(metrics.transport.frames_received.values()),
        )

    def close(self) -> None:
        if not self._tables:
            return
        for track in list(self._segments):
            self._finish(track)
        tables, self._tables = self._tables, {}
        for table in tables.values():
            table.flush()
            table.compact()

    def _append(self, table: str, **row: Any) -> None:
        if not self._tables:
            raise RuntimeError("LanceSink is not open")
        row["session"] = self._session
        self._tables[table].rows.append(row)
        self._maybe_flush()

    def _finish(self, track: str) -> None:
        """Close a track's open segment. Its video row goes in ahead of its
        frame rows, so a frame never names a segment that isn't there."""
        segment = self._segments.pop(track)
        mp4 = segment.finish()
        spec = segment.spec
        height, width, _ = spec.shape
        self._tables["videos"].rows.append({
            "session": self._session,
            "track": track,
            "segment": spec.number,
            "start_us": segment.timestamps[0],
            "end_us": segment.timestamps[-1],
            "width": width,
            "height": height,
            "frames": len(segment.timestamps),
            "mp4": {"data": mp4},
        })
        self._tables["frames"].rows.extend(
            {"session": self._session, "track": track, "timestamp_us": ts, "segment": spec.number, "index": i}
            for i, ts in enumerate(segment.timestamps)
        )

    def _maybe_flush(self) -> None:
        now = time.monotonic()
        if now < self._next_flush:
            return
        self._next_flush = now + FLUSH_INTERVAL_S
        for table in self._tables.values():
            table.flush()


class _Table:
    """Rows buffered in memory and appended to one Lance table on flush.
    Each append is a version of its own, so `compact` folds them on close."""

    def __init__(self, uri: str, schema: "pa.Schema") -> None:
        self.uri = uri
        self.schema = schema
        self.rows: List[Dict[str, Any]] = []
        lance.write_dataset(schema.empty_table(), uri)

    def flush(self) -> None:
        if not self.rows:
            return
        rows, self.rows = self.rows, []
        lance.write_dataset(pa.Table.from_pylist(rows, schema=self.schema), self.uri, mode="append")

    def compact(self) -> None:
        lance.dataset(self.uri).optimize.compact_files()
        lance.dataset(self.uri).cleanup_old_versions(older_than=datetime.timedelta(0))


@dataclass(frozen=True)
class _SegmentSpec:
    number: int
    shape: Tuple[int, int, int]
    start_us: int
    crf: int


class _Segment:
    """One track's frames encoded into an in-memory MP4. Timestamps are kept
    to the microsecond, so the video plays back at the rate it was recorded."""

    def __init__(self, spec: _SegmentSpec) -> None:
        self.spec = spec
        self.timestamps: List[int] = []
        self._buffer = io.BytesIO()
        self._container = av.open(self._buffer, "w", format="mp4")
        self._stream = self._container.add_stream(
            "libx264", options={"crf": str(spec.crf), "preset": "veryfast"}
        )
        self._stream.height, self._stream.width, _ = spec.shape
        self._stream.pix_fmt = "yuv420p"
        self._stream.codec_context.time_base = _MICROS
        self._last_pts = -1

    def encode(self, pixels: np.ndarray, timestamp_us: int) -> None:
        frame = av.VideoFrame.from_ndarray(np.ascontiguousarray(pixels), format="rgb24")
        # The muxer refuses timestamps that go backwards. One that does is
        # moved past the last; `frames` keeps the true one.
        self._last_pts = frame.pts = max(timestamp_us - self.spec.start_us, self._last_pts + 1)
        frame.time_base = _MICROS
        self._mux(frame)
        self.timestamps.append(timestamp_us)

    def finish(self) -> bytes:
        self._mux(None)
        self._container.close()
        return self._buffer.getvalue()

    def _mux(self, frame: "Optional[av.VideoFrame]") -> None:
        for packet in self._stream.encode(frame):
            self._container.mux(packet)


def _values(n: int) -> "pa.DataType":
    # Lance refuses a fixed-size list of length zero.
    return pa.list_(pa.float64(), n) if n else pa.list_(pa.float64())


def _schemas(state: int, action: int) -> Dict[str, List[Any]]:
    session = pa.field("session", pa.string())
    ts = pa.field("timestamp_us", pa.int64())
    track = pa.field("track", pa.string())
    return {
        "states": [session, ts, pa.field("values", _values(state))],
        "actions": [
            session, ts,
            pa.field("in_reply_to_ts_us", pa.int64()),
            pa.field("sender", pa.string()),
            pa.field("active", pa.bool_()),
            pa.field("values", _values(action)),
        ],
        "keypoints": [
            session, ts,
            pa.field("type", pa.string()),
            pa.field("sender", pa.string()),
            pa.field("payload", pa.string()),
        ],
        "videos": [
            session, track,
            pa.field("segment", pa.int32()),
            pa.field("start_us", pa.int64()),
            pa.field("end_us", pa.int64()),
            pa.field("width", pa.int32()),
            pa.field("height", pa.int32()),
            pa.field("frames", pa.int32()),
            lance.blob_field("mp4"),
        ],
        "frames": [
            session, track, ts,
            pa.field("segment", pa.int32()),
            pa.field("index", pa.int32()),
        ],
        "frames_dropped": [session, track, ts],
        "metrics": [session, ts] + [
            pa.field(name, pa.int64())
            for name in (
                "rtt_us", "time_sync_offset_us", "time_sync_uncertainty_us", "states_received",
                "actions_received", "actions_dropped_stale", "frames_received",
            )
        ],
    }
