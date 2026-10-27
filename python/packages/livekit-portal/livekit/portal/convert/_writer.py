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

"""What an output format implements."""
from __future__ import annotations

import argparse
from dataclasses import dataclass
from typing import Dict, Iterable, Protocol, Tuple

from ._archive import Episode, Row, Schema


@dataclass(frozen=True)
class DatasetSpec:
    """What every archive going into one dataset shares."""

    schema: Schema
    fps: int
    frame_shapes: Dict[str, Tuple[int, int, int]]
    """`(H, W, 3)` per camera, from its first frame."""


class Writer(Protocol):
    """Writes episodes into one dataset. Like a recording `Sink`: `open`
    first, `close` exactly once, last."""

    def open(self, spec: DatasetSpec) -> None: ...

    def write_episode(self, episode: Episode, rows: Iterable[Row]) -> int:
        """Write one episode. Returns the rows written; 0 writes nothing."""
        ...

    def close(self) -> None: ...


class Output(Protocol):
    """An `--output` choice: its own CLI flags, and a writer built from them."""

    name: str
    help: str

    @staticmethod
    def add_arguments(group: argparse._ArgumentGroup) -> None: ...

    @staticmethod
    def from_args(args: argparse.Namespace) -> Writer: ...
