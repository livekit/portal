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

"""Reading a `LanceSink` recording back into an `Archive`."""
from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

import numpy as np

try:
    import av
    import lance
except ImportError as e:  # pragma: no cover - exercised without the extra
    raise ImportError(
        "reading LanceSink recordings needs Lance and PyAV: install `livekit-portal[lance]`"
    ) from e

from .._lance import SCHEMA_KEY
from ._archive import Archive, Keypoint, _Action, _json, schema_from

FrameKey = Tuple[str, int, int]  # (track, segment, index)


def read_lance(path: Path) -> Archive:
    def table(name: str):
        return lance.dataset(str(path / f"{name}.lance"))

    if not (path / "states.lance").is_dir():
        raise ValueError(f"{path.name} is not a LanceSink recording: it has no states.lance")
    states = table("states")
    doc = (states.schema.metadata or {}).get(SCHEMA_KEY)
    if doc is None:
        raise ValueError(f"{path.name} has no portal schema: not written by LanceSink")
    schema = schema_from(json.loads(doc))

    archive = Archive(path, schema, _Video(table("videos")).frame)
    archive._states = sorted(
        (
            (row["timestamp_us"], dict(zip(schema.state, row["values"])))
            for row in states.to_table(columns=["timestamp_us", "values"]).to_pylist()
            if len(row["values"]) == len(schema.state)
        ),
        key=lambda pair: pair[0],
    )
    for row in table("actions").to_table().to_pylist():
        anchor = row["in_reply_to_ts_us"]
        archive._actions[(row["sender"], row["timestamp_us"])] = _Action(
            robot_us=row["timestamp_us"] if anchor is None else anchor,
            active=row["active"],
            values=dict(zip(schema.action, row["values"])),
        )
    archive.keypoints = [
        Keypoint(row["type"], _json(row["payload"]), row["timestamp_us"], row["sender"] or "")
        for row in table("keypoints").to_table().to_pylist()
    ]
    for row in table("frames").to_table(columns=["track", "timestamp_us", "segment", "index"]).to_pylist():
        frames = archive._frames.get(row["track"])
        if frames is not None:
            frames.add(row["timestamp_us"], (row["track"], row["segment"], row["index"]))
    for frames in archive._frames.values():
        frames.finish()
    return archive


class _Video:
    """Decodes frames out of the MP4 segments in `videos`.

    Rows ask for frames in time order, so one decoder per track, moving
    forward through its segment, serves them without holding decoded video
    in memory. A step backwards reopens the segment.
    """

    def __init__(self, videos) -> None:
        self._videos = videos
        ids = videos.to_table(columns=["track", "segment"], with_row_id=True)
        self._ids: Dict[Tuple[str, int], int] = {
            (track, segment): row_id
            for track, segment, row_id in zip(
                ids["track"].to_pylist(), ids["segment"].to_pylist(), ids["_rowid"].to_pylist()
            )
        }
        self._cursors: Dict[str, _Cursor] = {}

    def frame(self, key: FrameKey) -> np.ndarray:
        track, segment, index = key
        cursor = self._cursors.get(track)
        if cursor is None or cursor.segment != segment or cursor.index > index:
            if cursor is not None:
                cursor.close()
            (blob,) = self._videos.take_blobs("mp4", ids=[self._ids[(track, segment)]])
            cursor = self._cursors[track] = _Cursor(segment, blob.readall())
        return cursor.seek(index)


class _Cursor:
    def __init__(self, segment: int, mp4: bytes) -> None:
        self.segment = segment
        self.index = -1
        self._container = av.open(io.BytesIO(mp4))
        self._frames = self._container.decode(video=0)
        self._frame: Any = None
        self._pixels: Optional[np.ndarray] = None

    def seek(self, index: int) -> np.ndarray:
        while self.index < index:
            self._frame = next(self._frames, None)
            if self._frame is None:
                raise ValueError(f"segment {self.segment} ends before frame {index}")
            self.index += 1
            self._pixels = None
        if self._pixels is None:
            self._pixels = self._frame.to_ndarray(format="rgb24")
        return self._pixels

    def close(self) -> None:
        self._container.close()
