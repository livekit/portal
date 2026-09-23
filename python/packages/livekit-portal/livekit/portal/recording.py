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

"""Recording what an observer hears.

    from livekit.portal.recording import Sink, SessionInfo

    class MySink:
        def open(self, session: SessionInfo) -> None: ...
        def write_state(self, state) -> None: ...
        def write_frame(self, track, frame) -> None: ...
        def write_action(self, action) -> None: ...
        def write_keypoint(self, keypoint) -> None: ...
        def write_metrics(self, metrics) -> None: ...
        def close(self) -> None: ...

    observer.record_to(MySink())

`RrdSink` writes Rerun archives; it needs `pip install 'livekit-portal[rerun]'`.
"""
from ._recording import ObserverMetrics, SessionInfo, Sink

__all__ = ["Sink", "SessionInfo", "ObserverMetrics", "RrdSink"]


def __getattr__(name: str):
    # `RrdSink` pulls in Rerun, which ships as the `[rerun]` extra; importing
    # this module must keep working without it.
    if name == "RrdSink":
        from ._rrd import RrdSink

        return RrdSink
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
