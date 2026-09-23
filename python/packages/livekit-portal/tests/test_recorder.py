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

"""The recorder's queue, ordering and failure handling, driven directly."""
from __future__ import annotations

import threading
import time

import pytest

from livekit.portal import Observer, ObserverConfig, TimeSyncSource
from livekit.portal._recording import Recorder, SessionInfo, _Converters


class MemorySink:
    def __init__(self, *, frame_delay_s: float = 0.0, fail_on: str = "") -> None:
        self.calls: list[tuple] = []
        self.frame_delay_s = frame_delay_s
        self.fail_on = fail_on
        self.lock = threading.Lock()

    def _record(self, name, *args):
        if name == self.fail_on:
            raise RuntimeError(f"boom in {name}")
        with self.lock:
            self.calls.append((name, *args))

    def open(self, session):
        self._record("open", session)

    def write_state(self, state):
        self._record("state", state)

    def write_frame(self, track, frame):
        time.sleep(self.frame_delay_s)
        self._record("frame", track, frame)

    def write_action(self, action):
        self._record("action", action)

    def write_keypoint(self, keypoint):
        self._record("keypoint", keypoint)

    def write_metrics(self, metrics):
        self._record("metrics", metrics)

    def close(self):
        self._record("close")

    def names(self, *, without_metrics: bool = True) -> list[str]:
        with self.lock:
            return [c[0] for c in self.calls if not (without_metrics and c[0] == "metrics")]


IDENTITY = _Converters(state=lambda x: x, action=lambda x: x, keypoint=lambda x: x, metrics=lambda: "m")
SESSION = SessionInfo(
    session="room",
    local_identity="observer",
    state_schema=[],
    action_schema=[],
    video_tracks=[],
    time_sync_source=TimeSyncSource.PORTAL,
    started_at_us=1,
    now_us=lambda: 1,
)


def _recorder(sink, **kwargs) -> Recorder:
    kwargs.setdefault("metrics_interval_s", None)
    rec = Recorder(sink, IDENTITY, **kwargs)
    rec.start(SESSION)
    return rec


def test_open_first_close_last_and_order_is_kept():
    sink = MemorySink()
    rec = _recorder(sink)
    rec.enqueue("state", "s1")
    rec.enqueue("frame", ("cam", "f1"))
    rec.enqueue("keypoint", "k1")
    rec.enqueue("action", "a1")
    rec.stop()
    assert sink.names() == ["open", "state", "frame", "keypoint", "action", "close"]
    assert sink.calls[0][1] is SESSION


def test_stop_writes_everything_already_queued():
    sink = MemorySink(frame_delay_s=0.01)
    rec = _recorder(sink, max_queued_frames=1000)
    for i in range(50):
        rec.enqueue("frame", ("cam", i))
    rec.stop()
    assert sink.names().count("frame") == 50
    assert sink.names()[-1] == "close"


def test_slow_sink_drops_only_frames_and_never_blocks_the_receive_path():
    sink = MemorySink(frame_delay_s=0.02)
    rec = _recorder(sink, max_queued_frames=4)
    worst = 0.0
    for i in range(200):
        start = time.perf_counter()
        rec.enqueue("frame", ("cam", i))
        rec.enqueue("state", i)
        if i % 20 == 0:
            rec.enqueue("keypoint", i)
        worst = max(worst, time.perf_counter() - start)
    dropped = rec.metrics().frames_dropped
    rec.stop()

    assert worst < 0.001, f"enqueue took {worst * 1000:.2f} ms"
    assert dropped > 0
    names = sink.names()
    assert names.count("state") == 200
    assert names.count("keypoint") == 10
    assert names.count("frame") == 200 - dropped


def test_a_failing_write_is_skipped_and_recording_continues(caplog):
    sink = MemorySink(fail_on="state")
    rec = _recorder(sink)
    rec.enqueue("state", 1)
    rec.enqueue("state", 2)
    rec.enqueue("keypoint", "k")
    rec.stop()
    assert sink.names() == ["open", "keypoint", "close"]
    failures = [r for r in caplog.records if "write_state" in r.getMessage()]
    assert len(failures) == 1, "logged once, not per record"


def test_a_failing_open_raises_and_starts_nothing():
    rec = Recorder(MemorySink(fail_on="open"), IDENTITY)
    with pytest.raises(RuntimeError, match="boom in open"):
        rec.start(SESSION)


def test_metrics_are_written_on_the_interval():
    sink = MemorySink()
    rec = _recorder(sink, metrics_interval_s=0.05)
    time.sleep(0.3)
    rec.stop()
    n = sink.names(without_metrics=False).count("metrics")
    assert 3 <= n <= 8, n


def test_nothing_is_accepted_after_stop():
    sink = MemorySink()
    rec = _recorder(sink)
    rec.stop()
    rec.enqueue("state", 1)
    assert sink.names() == ["open", "close"]


def test_observer_records_one_sink_at_a_time():
    obs = Observer(ObserverConfig("demo"))
    try:
        assert obs.metrics().observer.recording is False
        obs.stop_recording()  # no-op
        sink = MemorySink()
        obs.record_to(sink, metrics_interval_s=None)
        assert obs.metrics().observer.recording is True
        with pytest.raises(RuntimeError, match="already recording"):
            obs.record_to(MemorySink())
        obs.stop_recording()
        assert sink.names() == ["open", "close"]
        info = sink.calls[0][1]
        assert (info.session, info.local_identity) == ("demo", None)
    finally:
        obs.close()
