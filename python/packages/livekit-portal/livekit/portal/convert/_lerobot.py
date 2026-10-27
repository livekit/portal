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

"""`--output lerobotv3`: a LeRobot v3 dataset, written by the installed lerobot."""
from __future__ import annotations

import argparse
import shutil
from pathlib import Path
from typing import Iterable, Optional

from ._archive import Episode, Row
from ._writer import DatasetSpec


class LeRobotV3:
    name = "lerobotv3"
    help = "LeRobot v3 dataset, local or pushed to the Hugging Face Hub"

    @staticmethod
    def add_arguments(group: argparse._ArgumentGroup) -> None:
        group.add_argument("--repo-id", default="local/portal",
                           help="dataset id, <user>/<name> (default: local/portal)")
        group.add_argument("--root", default=None,
                           help="where to write it (default: data/<repo-id>)")
        group.add_argument("--robot-type", default="livekit_portal",
                           help="robot_type in the dataset's metadata")
        group.add_argument("--push", action="store_true",
                           help="push to the Hugging Face Hub as <repo-id> when done")
        group.add_argument("--private", action="store_true", help="with --push, a private repo")
        group.add_argument("--overwrite", action="store_true",
                           help="replace whatever is already at --root")
        # Encoder threads have aborted mid-episode on macOS with `BUG IN CLIENT
        # OF LIBDISPATCH: Semaphore object deallocated while in use`; 1 avoids it.
        group.add_argument("--image-writer-threads", type=int, default=4,
                           help="encoder threads (default 4; use 1 after a libdispatch abort)")

    @staticmethod
    def from_args(args: argparse.Namespace) -> "LeRobotV3":
        return LeRobotV3(args)

    def __init__(self, args: argparse.Namespace) -> None:
        if args.push and args.repo_id.startswith("local/"):
            raise ValueError("--push needs a Hub --repo-id, <user>/<name>")
        self.repo_id = args.repo_id
        self.root = Path(args.root if args.root else Path("data") / args.repo_id).expanduser()
        self.robot_type = args.robot_type
        self.push = args.push
        self.private = args.private
        self.overwrite = args.overwrite
        self.image_writer_threads = args.image_writer_threads
        self._dataset = None
        self._state: list = []
        self._action: list = []

    def open(self, spec: DatasetSpec) -> None:
        try:
            from lerobot.datasets.lerobot_dataset import LeRobotDataset
        except ImportError as e:
            raise ImportError(
                "--output lerobotv3 needs lerobot: install `livekit-portal[lerobot]` (Python 3.12+)"
            ) from e

        if self.root.exists() and any(self.root.iterdir()):
            if not self.overwrite:
                raise FileExistsError(f"{self.root} is not empty; pass --overwrite to replace it")
            shutil.rmtree(self.root)
        self._state = spec.schema.state
        self._action = spec.schema.action
        self._dataset = LeRobotDataset.create(
            repo_id=self.repo_id,
            fps=spec.fps,
            features=_features(spec),
            root=self.root,
            robot_type=self.robot_type,
            use_videos=True,
            image_writer_threads=self.image_writer_threads,
            streaming_encoding=True,
        )

    def write_episode(self, episode: Episode, rows: Iterable[Row]) -> int:
        import numpy as np

        dataset = self._dataset
        written = 0
        for row in rows:
            frame = {
                "observation.state": np.array([row.state[n] for n in self._state], dtype=np.float32),
                "action": np.array([row.action[n] for n in self._action], dtype=np.float32),
                "task": episode.task,
            }
            for camera, pixels in row.frames.items():
                frame[f"observation.images.{camera}"] = pixels
            dataset.add_frame(frame)
            written += 1
        if written:
            dataset.save_episode()
        return written

    def close(self) -> None:
        dataset: Optional[object] = self._dataset
        if dataset is None:
            return
        self._dataset = None
        dataset.finalize()
        if self.push:
            dataset.push_to_hub(private=self.private)


def _features(spec: DatasetSpec) -> dict:
    features = {
        "observation.state": {"dtype": "float32", "shape": (len(spec.schema.state),), "names": spec.schema.state},
        "action": {"dtype": "float32", "shape": (len(spec.schema.action),), "names": spec.schema.action},
    }
    for camera, shape in spec.frame_shapes.items():
        features[f"observation.images.{camera}"] = {
            "dtype": "video",
            "shape": shape,
            "names": ["height", "width", "channels"],
        }
    return features
