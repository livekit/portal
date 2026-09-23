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

"""Recording plumbing behind `Observer.record_to`.

Kept free of imports from the package root so `livekit.portal` can import it
without a cycle; conversions from FFI records are passed in by the caller.
"""
from __future__ import annotations

import collections
import logging
import threading
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Callable, Deque, Dict, List, Optional, Protocol, Tuple

if TYPE_CHECKING:
    from . import Action, FieldSpec, Keypoint, PortalMetrics, State, TimeSyncSource, VideoFrameData
    from . import VideoTrackSpec

_log = logging.getLogger("livekit.portal.recording")

DEFAULT_MAX_QUEUED_FRAMES = 64
DEFAULT_METRICS_INTERVAL_S = 1.0


@dataclass(frozen=True)
class SessionInfo:
    """What a sink learns when a recording opens."""

    session: str
    """The session label, normally the room name."""
    local_identity: Optional[str]
    """The observer's identity, or `None` if it records before connecting."""
    state_schema: List["FieldSpec"]
    action_schema: List["FieldSpec"]
    video_tracks: List["VideoTrackSpec"]
    time_sync_source: "TimeSyncSource"
    """Which clock the timestamps come from, so an archive can say so."""
    started_at_us: int
    """`now_us()` when the recording opened, on the robot's clock."""
    now_us: Callable[[], int]
    """The observer's `now_us()`, for sinks that stamp rows of their own
    (metrics, for example)."""


class Sink(Protocol):
    """Where an observer writes what it hears.

    Portal calls a sink from its own writer thread, behind a bounded queue,
    so a slow disk never stalls the receive path. Calls arrive in the order
    the observer heard them. `open` comes first and `close` last.
    """

    def open(self, session: SessionInfo) -> None: ...
    def write_state(self, state: "State") -> None: ...
    def write_frame(self, track: str, frame: "VideoFrameData") -> None: ...
    def write_action(self, action: "Action") -> None: ...
    def write_keypoint(self, keypoint: "Keypoint") -> None: ...
    def write_metrics(self, metrics: "PortalMetrics") -> None: ...
    def close(self) -> None: ...


@dataclass
class ObserverMetrics:
    """Recording counters, surfaced as `Observer.metrics().observer`."""

    recording: bool = False
    frames_written: int = 0
    frames_dropped: int = 0
    """Frames dropped because the sink fell behind and the queue was full."""
    queued: int = 0


@dataclass
class _Converters:
    """Turn the FFI records the dispatcher sees into the public types."""

    state: Callable[[Any], "State"]
    action: Callable[[Any], "Action"]
    keypoint: Callable[[Any], "Keypoint"]
    metrics: Callable[[], "PortalMetrics"]


_Item = Tuple[str, Any]


@dataclass
class _Counters:
    frames_written: int = 0
    frames_dropped: int = 0
    queued_frames: int = 0
    failed_writes: Dict[str, int] = field(default_factory=dict)


class Recorder:
    """Feeds one sink from the receive path.

    `enqueue` runs on the thread that delivered the packet and never blocks.
    Only video frames are dropped when the queue is full: state, actions and
    keypoints are tiny, and keypoints often mark episode boundaries, so
    losing one would corrupt the recording rather than thin it.
    """

    def __init__(
        self,
        sink: Sink,
        convert: _Converters,
        *,
        max_queued_frames: int = DEFAULT_MAX_QUEUED_FRAMES,
        metrics_interval_s: Optional[float] = DEFAULT_METRICS_INTERVAL_S,
    ) -> None:
        if max_queued_frames < 1:
            raise ValueError("max_queued_frames must be at least 1")
        self._sink = sink
        self._convert = convert
        self._max_queued_frames = max_queued_frames
        self._metrics_interval_s = metrics_interval_s
        self._queue: Deque[_Item] = collections.deque()
        self._cond = threading.Condition()
        self._counters = _Counters()
        self._stopping = False
        self._thread: Optional[threading.Thread] = None

    def start(self, session: SessionInfo) -> None:
        """Open the sink and start writing. Raises whatever `open` raises."""
        self._sink.open(session)
        self._thread = threading.Thread(target=self._run, name="portal-recorder", daemon=True)
        self._thread.start()

    def enqueue(self, kind: str, item: Any) -> None:
        with self._cond:
            if self._stopping:
                return
            if kind == "frame":
                if self._counters.queued_frames >= self._max_queued_frames:
                    self._counters.frames_dropped += 1
                    return
                self._counters.queued_frames += 1
            self._queue.append((kind, item))
            self._cond.notify()

    def stop(self) -> None:
        """Write out everything already queued, then close the sink."""
        with self._cond:
            self._stopping = True
            self._cond.notify()
        if self._thread is not None:
            self._thread.join()

    def metrics(self) -> ObserverMetrics:
        with self._cond:
            return ObserverMetrics(
                recording=not self._stopping,
                frames_written=self._counters.frames_written,
                frames_dropped=self._counters.frames_dropped,
                queued=len(self._queue),
            )

    def _run(self) -> None:
        interval = self._metrics_interval_s
        next_metrics = time.monotonic() + interval if interval is not None else None
        while True:
            with self._cond:
                if not self._queue and not self._stopping:
                    timeout = None if next_metrics is None else max(0.0, next_metrics - time.monotonic())
                    self._cond.wait(timeout=timeout)
                batch = list(self._queue)
                self._queue.clear()
                self._counters.queued_frames = 0
                stopping = self._stopping
            for kind, item in batch:
                self._write(kind, item)
            if next_metrics is not None and (stopping or time.monotonic() >= next_metrics):
                self._write("metrics", None)
                next_metrics = time.monotonic() + interval
            if stopping:
                break
        try:
            self._sink.close()
        except Exception:  # noqa: BLE001
            _log.exception("recording sink raised in close()")

    def _write(self, kind: str, item: Any) -> None:
        try:
            if kind == "state":
                self._sink.write_state(self._convert.state(item))
            elif kind == "frame":
                track, frame = item
                self._sink.write_frame(track, frame)
                self._counters.frames_written += 1
            elif kind == "action":
                self._sink.write_action(self._convert.action(item))
            elif kind == "keypoint":
                self._sink.write_keypoint(self._convert.keypoint(item))
            elif kind == "metrics":
                self._sink.write_metrics(self._convert.metrics())
        except Exception:  # noqa: BLE001
            # A bad write loses one record, not the recording. Log the first
            # failure per kind in full, then only count.
            failures = self._counters.failed_writes.get(kind, 0) + 1
            self._counters.failed_writes[kind] = failures
            if failures == 1:
                _log.exception("recording sink raised in write_%s(); recording continues", kind)
