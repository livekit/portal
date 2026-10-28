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

"""Reading an `RrdSink` archive back: its schema, keypoints, episodes and rows.

Nothing here knows about an output format. A writer gets `Row`s: one per
action, with the state and frames it answered.
"""
from __future__ import annotations

import bisect
import io
import json
import logging
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional, Tuple

import numpy as np

logger = logging.getLogger(__name__)

_RGB = 2  # rerun.datatypes.ColorModel.RGB


@dataclass(frozen=True)
class Schema:
    """What the archive's `portal/schema` says it holds, in field order."""

    session: str
    state: List[str]
    action: List[str]
    cameras: List[str]


@dataclass(frozen=True)
class Keypoint:
    type: str
    payload: Dict[str, Any]
    timestamp_us: int
    sender: str


@dataclass(frozen=True)
class Episode:
    """A span of `robot_time`, inclusive at both ends."""

    task: str
    start_us: int
    end_us: int


@dataclass(frozen=True)
class EpisodeRule:
    """How keypoints cut a session into episodes.

    `start` opens an episode and `end` closes it; a second `start` closes the
    open one first. `discard` closes it and throws it away, for a take that
    went wrong. An episode still open when the archive ends is dropped, as an
    unfinished take. An archive with no `start` keypoint at all is one
    episode, so a recording made without an orchestrator still converts.
    """

    start: str = "recording"
    end: str = "idle"
    discard: str = "discard"
    default_task: str = ""


@dataclass(frozen=True)
class Selection:
    """Which actions become rows, and how stale an observation may be.

    With no `sender`, the actions that drove the robot (`active`); with one,
    that sender's actions whether or not it drove, e.g. a shadow policy.
    """

    sender: Optional[str] = None
    max_stale_us: int = 200_000


@dataclass(frozen=True)
class Row:
    timestamp_us: int
    """The `robot_time` the action answered: its state and frames are as of here."""
    action_timestamp_us: int
    sender: str
    active: bool
    state: Dict[str, float]
    action: Dict[str, float]
    frames: Dict[str, np.ndarray]
    """`(H, W, 3)` uint8 RGB per camera."""


@dataclass
class _Action:
    robot_us: int
    active: bool = False
    values: Dict[str, float] = field(default_factory=dict)


class _Frames:
    """One camera's frames, kept as the arrow columns they arrived in and
    decoded one at a time. Copying every blob into Python up front would hold
    the archive's video several times over."""

    def __init__(self) -> None:
        self._index: List[Tuple[int, int, int]] = []  # (ts_us, batch, row)
        self._batches: List[Any] = []

    def add(self, times: List[int], batch) -> None:
        number = len(self._batches)
        self._batches.append(batch)
        self._index.extend((ts, number, row) for row, ts in enumerate(times) if ts is not None)

    def finish(self) -> None:
        self._index.sort()

    def __len__(self) -> int:
        return len(self._index)

    def at(self, ts_us: int, max_stale_us: int) -> Optional[np.ndarray]:
        """The newest frame at or before `ts_us`, unless it is too old."""
        i = bisect.bisect_right(self._index, (ts_us, float("inf"), 0)) - 1
        if i < 0 or ts_us - self._index[i][0] > max_stale_us:
            return None
        _, number, row = self._index[i]
        return _decode(self._batches[number], row)

    def first(self) -> Optional[np.ndarray]:
        if not self._index:
            return None
        _, number, row = self._index[0]
        return _decode(self._batches[number], row)


class Archive:
    """One `.rrd` written by `RrdSink`, read back."""

    def __init__(self, path: Path, schema: Schema) -> None:
        self.path = path
        self.schema = schema
        self.keypoints: List[Keypoint] = []
        self._states: List[Tuple[int, Dict[str, float]]] = []
        self._actions: Dict[Tuple[str, int], _Action] = {}
        self._frames: Dict[str, _Frames] = {camera: _Frames() for camera in schema.cameras}

    @property
    def senders(self) -> List[str]:
        return sorted({sender for sender, _ in self._actions})

    def frame_shape(self, camera: str) -> Optional[Tuple[int, int, int]]:
        frame = self._frames[camera].first()
        return None if frame is None else frame.shape

    def state_period_us(self) -> Optional[int]:
        """Median spacing of the robot's state samples."""
        times = [ts for ts, _ in self._states]
        if len(times) < 2:
            return None
        return int(np.median(np.diff(times)))

    def episodes(self, rule: EpisodeRule) -> List[Episode]:
        marks = sorted(self.keypoints, key=lambda kp: kp.timestamp_us)
        if not any(kp.type == rule.start for kp in marks):
            span = self._span()
            return [] if span is None else [Episode(rule.default_task, *span)]

        episodes: List[Episode] = []
        open_at: Optional[Keypoint] = None
        for kp in marks:
            if kp.type not in (rule.start, rule.end, rule.discard):
                continue
            if open_at is not None and kp.type != rule.discard:
                episodes.append(Episode(_task(open_at, rule), open_at.timestamp_us, kp.timestamp_us))
            open_at = kp if kp.type == rule.start else None
        if open_at is not None:
            logger.warning("%s: an episode opened at %d never closed; dropped", self.path.name, open_at.timestamp_us)
        return episodes

    def rows(self, episode: Episode, selection: Selection) -> Iterator[Row]:
        """One row per selected action inside `episode`, in action order.

        Observations are taken as of the action's anchor: the newest state and
        frame at or before it, no older than `max_stale_us`. A row with any of
        them missing is skipped.
        """
        stale = selection.max_stale_us
        for (sender, action_us), action in sorted(self._actions.items(), key=lambda item: item[0][1]):
            if selection.sender is None and not action.active:
                continue
            if selection.sender is not None and sender != selection.sender:
                continue
            if not episode.start_us <= action.robot_us <= episode.end_us:
                continue
            if len(action.values) != len(self.schema.action):
                continue
            state = _as_of(self._states, action.robot_us, stale)
            if state is None:
                continue
            frames = {}
            for camera, column in self._frames.items():
                frame = column.at(action.robot_us, stale)
                if frame is None:
                    break
                frames[camera] = frame
            else:
                yield Row(
                    timestamp_us=action.robot_us,
                    action_timestamp_us=action_us,
                    sender=sender,
                    active=action.active,
                    state=state,
                    action={name: action.values[name] for name in self.schema.action},
                    frames=frames,
                )

    def _span(self) -> Optional[Tuple[int, int]]:
        times = [ts for ts, _ in self._states] + [a.robot_us for a in self._actions.values()]
        return (min(times), max(times)) if times else None


def read_archive(path: "str | os.PathLike[str]") -> Archive:
    """Parse one `.rrd`. Field order comes from its `portal/schema`, which
    `RrdSink` always writes: Rerun entities are independent, and guessing the
    order would build a dataset with its joints silently permuted."""
    try:
        from rerun.chunk import RrdReader
    except ImportError as e:  # pragma: no cover - exercised without the extra
        raise ImportError(
            "converting needs Rerun 0.38 or newer: install `livekit-portal[lerobot]`"
        ) from e

    path = Path(path)
    chunks = RrdReader(path).stream().to_chunks()
    schema = next((_schema(c) for c in chunks if c.entity_path == "/portal/schema"), None)
    if schema is None:
        raise ValueError(f"{path.name} has no portal/schema: not written by RrdSink")

    archive = Archive(path, schema)
    states: Dict[int, Dict[str, float]] = {}
    state_fields = set(schema.state)
    action_fields = set(schema.action)
    for chunk in chunks:
        parts = _parts(chunk.entity_path)
        if chunk.is_static or len(parts) < 2:
            continue
        batch = chunk.to_record_batch()
        root = parts[0]
        if root == "observation" and len(parts) == 2:
            name = parts[1]
            if name in archive._frames:
                archive._frames[name].add(_times(batch, "robot_time"), batch)
            elif name in state_fields:
                for ts, value in zip(_times(batch, "robot_time"), _scalars(batch)):
                    if ts is not None and value is not None:
                        states.setdefault(ts, {})[name] = value
        elif root == "action" and len(parts) == 3:
            sender, name = parts[1], parts[2]
            if name != "active" and name not in action_fields:
                continue
            for robot_us, action_us, value in zip(
                _times(batch, "robot_time"), _times(batch, "action_time"), _scalars(batch)
            ):
                if robot_us is None or action_us is None or value is None:
                    continue
                action = archive._actions.setdefault((sender, action_us), _Action(robot_us))
                if name == "active":
                    action.active = value != 0.0
                else:
                    action.values[name] = value
        elif root == "keypoints" and len(parts) == 2:
            payloads = _texts(batch, "payload")
            senders = _texts(batch, "sender")
            for ts, payload, sender in zip(_times(batch, "robot_time"), payloads, senders):
                if ts is None:
                    continue
                archive.keypoints.append(Keypoint(parts[1], _json(payload), ts, sender or ""))

    archive._states = sorted(
        (ts, {name: values[name] for name in schema.state})
        for ts, values in states.items()
        if len(values) == len(schema.state)
    )
    for frames in archive._frames.values():
        frames.finish()
    return archive


def _schema(chunk) -> Schema:
    batch = chunk.to_record_batch()
    (text,) = _texts(batch, "TextDocument:text")
    doc = json.loads(text)
    return Schema(
        session=doc.get("session", ""),
        state=[f["name"] for f in doc["state"]],
        action=[f["name"] for f in doc["action"]],
        cameras=[t["name"] for t in doc["video"]],
    )


def _task(kp: Keypoint, rule: EpisodeRule) -> str:
    task = kp.payload.get("task_description", kp.payload.get("task"))
    return rule.default_task if task is None else str(task)


def _as_of(series: List[Tuple[int, Any]], ts_us: int, max_stale_us: int) -> Optional[Any]:
    i = bisect.bisect_right(series, ts_us, key=lambda pair: pair[0]) - 1
    if i < 0 or ts_us - series[i][0] > max_stale_us:
        return None
    return series[i][1]


def _parts(entity_path: str) -> List[str]:
    """`/action/lab\\ room/a` -> `["action", "lab room", "a"]`."""
    parts, current, escaped = [], [], False
    for ch in entity_path.lstrip("/"):
        if escaped:
            current.append(ch)
            escaped = False
        elif ch == "\\":
            escaped = True
        elif ch == "/":
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
    parts.append("".join(current))
    return parts


def _times(batch, timeline: str) -> List[Optional[int]]:
    """A timeline as integer microseconds. Rerun stores nanoseconds."""
    import pyarrow as pa

    if timeline not in batch.schema.names:
        return [None] * batch.num_rows
    values = batch.column(timeline).cast(pa.int64()).to_pylist()
    return [None if v is None else v // 1000 for v in values]


def _first(values: List[Any]) -> List[Any]:
    return [v[0] if isinstance(v, list) and v else None for v in values]


def _scalars(batch) -> List[Optional[float]]:
    return _first(batch.column("Scalars:scalars").to_pylist())


def _texts(batch, column: str) -> List[Optional[str]]:
    return _first(batch.column(column).to_pylist())


def _json(text: Optional[str]) -> Dict[str, Any]:
    try:
        value = json.loads(text or "{}")
    except ValueError:
        return {}
    return value if isinstance(value, dict) else {}


def _blob(batch, column: str, row: int) -> bytes:
    return batch.column(column)[row].values[0].values.to_numpy(zero_copy_only=False).tobytes()


def _decode(batch, row: int) -> np.ndarray:
    names = batch.schema.names
    if "EncodedImage:blob" in names:
        from PIL import Image

        with Image.open(io.BytesIO(_blob(batch, "EncodedImage:blob", row))) as image:
            return np.asarray(image.convert("RGB"))
    (fmt,) = batch.column("Image:format")[row].as_py()
    if fmt.get("color_model") != _RGB:
        raise ValueError(f"unsupported raw image format {fmt}")
    pixels = np.frombuffer(_blob(batch, "Image:buffer", row), dtype=np.uint8)
    return pixels.reshape(fmt["height"], fmt["width"], 3)
