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

"""`livekit-portal convert`."""
from __future__ import annotations

import argparse
from pathlib import Path
from typing import List

from . import OUTPUTS
from ._archive import Archive, EpisodeRule, Selection, read_archive
from ._writer import DatasetSpec


def add_parser(subparsers) -> None:
    parser = subparsers.add_parser(
        "convert",
        help="convert .rrd recordings into a training dataset",
        description="Convert RrdSink .rrd recordings into one training dataset.",
        epilog="Episodes follow a keypoint convention: 'recording' (payload "
               "{\"task_description\": ...}) opens one, 'idle' closes it, 'discard' "
               "drops it. See docs/03-data-recording.md.",
    )
    parser.add_argument("paths", nargs="+", help=".rrd files, or directories holding them")
    parser.add_argument("--output", required=True, choices=sorted(OUTPUTS), help="dataset format")
    parser.add_argument("--fps", type=int, default=None,
                        help="dataset fps (default: the robot's state rate)")
    parser.add_argument("--episode-start", default="recording",
                        help="keypoint type that opens an episode (default: recording)")
    parser.add_argument("--episode-end", default="idle",
                        help="keypoint type that closes one (default: idle)")
    parser.add_argument("--episode-discard", default="discard",
                        help="keypoint type that closes one and drops it (default: discard)")
    parser.add_argument("--task", default="",
                        help="task for episodes whose keypoint has no task_description")
    parser.add_argument("--sender", default=None,
                        help="take this sender's actions (default: whichever was active)")
    parser.add_argument("--max-stale-ms", type=int, default=200,
                        help="how old a state or frame may be to pair with an action (default: 200)")
    parser.add_argument("--dry-run", action="store_true",
                        help="report what each archive holds and write nothing")
    for output in OUTPUTS.values():
        output.add_arguments(parser.add_argument_group(f"--output {output.name}", output.help))
    parser.set_defaults(run=run)


def run(args: argparse.Namespace) -> int:
    paths = _archives(args.paths)
    if not paths:
        print("[convert] no .rrd archives found")
        return 1

    rule = EpisodeRule(start=args.episode_start, end=args.episode_end,
                       discard=args.episode_discard, default_task=args.task)
    selection = Selection(sender=args.sender, max_stale_us=args.max_stale_ms * 1000)
    archives = [read_archive(path) for path in paths]
    for archive in archives:
        episodes = archive.episodes(rule)
        print(f"[convert] {archive.path.name}: {len(episodes)} episodes, "
              f"cameras={archive.schema.cameras}, senders={archive.senders}")

    if conflicts := _conflicts(archives):
        print("[convert] these archives cannot share one dataset:")
        for line in conflicts:
            print(f"[convert]   {line}")
        return 1
    if args.dry_run:
        return 0

    spec = _spec(archives, args.fps)
    writer = OUTPUTS[args.output].from_args(args)
    writer.open(spec)
    episodes = rows = 0
    try:
        for archive in archives:
            for number, episode in enumerate(archive.episodes(rule), start=1):
                written = writer.write_episode(episode, archive.rows(episode, selection))
                print(f"[convert]   {archive.path.name} #{number}: {written} rows, task={episode.task!r}")
                episodes += bool(written)
                rows += written
    finally:
        writer.close()
    print(f"[convert] wrote {episodes} episodes, {rows} rows")
    return 0 if rows else 1


def _archives(paths: List[str]) -> List[Path]:
    """Every `.rrd` named, or inside a directory named. Sorted, because
    `RrdSink` names them by start time."""
    found: List[Path] = []
    for raw in paths:
        path = Path(raw).expanduser()
        if path.is_dir():
            found.extend(sorted(path.glob("*.rrd")))
        elif path.suffix == ".rrd":
            found.append(path)
        else:
            print(f"[convert] skipping {path}: not a .rrd or a directory")
    return found


def _conflicts(archives: List[Archive]) -> List[str]:
    """How each archive's schema differs from the first. One dataset has one
    feature set, and a mismatch would otherwise convert with columns dropped
    or misnamed."""
    first = archives[0].schema
    conflicts = []
    for archive in archives[1:]:
        for label in ("state", "action", "cameras"):
            mine, theirs = getattr(first, label), getattr(archive.schema, label)
            if mine != theirs:
                conflicts.append(f"{archive.path.name}: {label} {theirs}, "
                                 f"but {archives[0].path.name} has {mine}")
    return conflicts


def _spec(archives: List[Archive], fps: int | None) -> DatasetSpec:
    schema = archives[0].schema
    shapes = {}
    for camera in schema.cameras:
        shape = next((s for a in archives if (s := a.frame_shape(camera)) is not None), None)
        if shape is None:
            raise ValueError(f"camera {camera!r} has no frames in any archive")
        shapes[camera] = shape
    if fps is None:
        period = next((p for a in archives if (p := a.state_period_us())), None)
        if not period:
            raise ValueError("cannot infer fps from the state stream; pass --fps")
        fps = round(1_000_000 / period)
        print(f"[convert] fps={fps}, from the robot's state rate")
    return DatasetSpec(schema=schema, fps=fps, frame_shapes=shapes)
