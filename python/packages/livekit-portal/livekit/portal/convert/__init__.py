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

"""Turn `RrdSink` archives into training datasets.

`read_archive` parses an archive into episodes and aligned rows; an output
format is a `Writer` over those rows. `livekit-portal convert` wires the two
together, picking the writer from `OUTPUTS`. Adding a format is a module with
an `Output` and one entry in `OUTPUTS`.
"""
from typing import Dict

from ._archive import (
    Archive,
    Episode,
    EpisodeRule,
    Keypoint,
    Row,
    Schema,
    Selection,
    read_archive,
)
from ._lerobot import LeRobotV3
from ._writer import DatasetSpec, Output, Writer

OUTPUTS: Dict[str, Output] = {output.name: output for output in (LeRobotV3,)}

__all__ = [
    "Archive",
    "DatasetSpec",
    "Episode",
    "EpisodeRule",
    "Keypoint",
    "LeRobotV3",
    "OUTPUTS",
    "Output",
    "Row",
    "Schema",
    "Selection",
    "Writer",
    "read_archive",
]
