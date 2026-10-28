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

"""`livekit-portal convert`: an `RrdSink` archive back into aligned rows, and
into a LeRobot v3 dataset.

Each tick stamps its index into the state, the frame's pixels and the action,
so a row pairing an action with an observation it did not answer shows up as
mismatched numbers.
"""
from __future__ import annotations

import numpy as np
import pytest

pytest.importorskip("rerun.chunk", reason="needs rerun-sdk>=0.38")

from livekit.portal import (  # noqa: E402
    Action,
    DType,
    FieldSpec,
    FrameSource,
    Keypoint,
    State,
    TimeSyncSource,
    VideoCodec,
    VideoFrameData,
    VideoTrackSpec,
)
from livekit.portal._cli import main  # noqa: E402
from livekit.portal._recording import SessionInfo  # noqa: E402
from livekit.portal.convert import EpisodeRule, Selection, read_archive  # noqa: E402
from livekit.portal.recording import RrdSink  # noqa: E402

US = 1_000_000
TICK_US = US // 30
BASE_US = 1_790_000_000 * US
# Not alphabetical, so the archive's own order is distinguishable from sorted().
FIELDS = ["shoulder", "elbow", "gripper"]
W, H = 64, 48


def ts(tick: int) -> int:
    return BASE_US + tick * TICK_US


def _session(fields=FIELDS, cameras=("wrist",)) -> SessionInfo:
    return SessionInfo(
        session="lab",
        local_identity="recorder",
        state_schema=[FieldSpec(name=n, dtype=DType.F32) for n in fields],
        action_schema=[FieldSpec(name=n, dtype=DType.F32) for n in fields],
        video_tracks=[
            VideoTrackSpec(name=c, codec=VideoCodec.H264, quality=90, max_bitrate_kbps=None,
                           simulcast=False, screencast=False)
            for c in cameras
        ],
        time_sync_source=TimeSyncSource.PORTAL,
        started_at_us=BASE_US,
        now_us=lambda: BASE_US,
    )


def _values(tick: int, fields=FIELDS) -> dict:
    return {name: float(tick * 10 + i) for i, name in enumerate(fields)}


def _pixel(tick: int) -> int:
    return (tick * 4) % 256


def _record(directory, *, ticks=40, frame_every=2, keypoints=(), jpeg_quality=95,
            fields=FIELDS, cameras=("wrist",), shadow=False) -> str:
    """Video at half the state rate, two actions per frame: a reader that
    keyed actions by the frame they answered would lose half of them."""
    sink = RrdSink(directory, jpeg_quality=jpeg_quality)
    sink.open(_session(fields, cameras))
    for tick in range(ticks):
        sink.write_state(State(values={}, raw_values=_values(tick, fields), timestamp_us=ts(tick)))
        anchor = tick - tick % frame_every
        if tick == anchor:
            pixels = np.full((H, W, 3), _pixel(tick), dtype=np.uint8)
            for camera in cameras:
                sink.write_frame(camera, VideoFrameData(
                    width=W, height=H, data=pixels.tobytes(), timestamp_us=ts(tick),
                    source=FrameSource.LIVE))
        sink.write_action(Action(
            values={}, raw_values=_values(anchor, fields), timestamp_us=ts(tick) + 5_000,
            sender="human", active=True, in_reply_to_ts_us=ts(anchor)))
        if shadow:
            sink.write_action(Action(
                values={}, raw_values=_values(-1, fields), timestamp_us=ts(tick) + 6_000,
                sender="policy", active=False, in_reply_to_ts_us=ts(anchor)))
    for tick, type_, payload in keypoints:
        sink.write_keypoint(Keypoint(type=type_, payload=payload, timestamp_us=ts(tick), sender="orchestrator"))
    sink.close()
    return sink.path


TAKES = [
    (4, "recording", {"task_description": "pick"}),
    (13, "idle", {}),
    (20, "recording", {"task_description": "place"}),
    (31, "idle", {}),
    (32, "recording", {"task_description": "dropped"}),
    (34, "discard", {}),
    (35, "recording", {"task_description": "never closed"}),
]


def test_schema_and_field_order(tmp_path):
    archive = read_archive(_record(tmp_path))
    assert archive.schema.state == FIELDS
    assert archive.schema.action == FIELDS
    assert archive.schema.cameras == ["wrist"]


def test_keypoints_cut_episodes(tmp_path):
    archive = read_archive(_record(tmp_path, keypoints=TAKES))
    episodes = archive.episodes(EpisodeRule())
    assert [(e.task, e.start_us, e.end_us) for e in episodes] == [
        ("pick", ts(4), ts(13)),
        ("place", ts(20), ts(31)),
    ]


def test_no_keypoints_is_one_episode(tmp_path):
    archive = read_archive(_record(tmp_path, ticks=10))
    (episode,) = archive.episodes(EpisodeRule(default_task="demo"))
    assert (episode.task, episode.start_us, episode.end_us) == ("demo", ts(0), ts(9))


@pytest.mark.parametrize("jpeg_quality", [95, 0])
def test_every_action_is_a_row_with_the_observation_it_answered(tmp_path, jpeg_quality):
    archive = read_archive(_record(tmp_path, keypoints=TAKES, jpeg_quality=jpeg_quality))
    episode = archive.episodes(EpisodeRule())[0]
    rows = list(archive.rows(episode, Selection()))
    # Ticks 4..13, one action each, though only every other tick has a frame.
    assert len(rows) == 10
    for row in rows:
        anchor = (row.timestamp_us - BASE_US) // TICK_US
        assert row.action == _values(anchor)
        assert row.state == _values(anchor)
        assert row.frames["wrist"].shape == (H, W, 3)
        assert abs(int(row.frames["wrist"].mean()) - _pixel(anchor)) <= 2


def test_inactive_actions_only_by_sender(tmp_path):
    archive = read_archive(_record(tmp_path, ticks=10, shadow=True))
    (episode,) = archive.episodes(EpisodeRule())
    assert archive.senders == ["human", "policy"]
    assert {r.sender for r in archive.rows(episode, Selection())} == {"human"}
    shadow = list(archive.rows(episode, Selection(sender="policy")))
    assert len(shadow) == 10
    assert all(not r.active for r in shadow)


def test_stale_observations_are_not_paired(tmp_path):
    archive = read_archive(_record(tmp_path, ticks=10, frame_every=6))
    (episode,) = archive.episodes(EpisodeRule())
    # Frames at ticks 0 and 6; anchors are those ticks, so the frame is never
    # stale. A tight bound still drops nothing, a negative one drops all.
    assert len(list(archive.rows(episode, Selection(max_stale_us=0)))) == 10
    assert list(archive.rows(episode, Selection(max_stale_us=-1))) == []


def test_mismatched_archives_are_refused(tmp_path, capsys):
    _record(tmp_path / "a", ticks=4)
    _record(tmp_path / "b", ticks=4, fields=["shoulder", "elbow"])
    with pytest.raises(SystemExit) as exit_:
        main(["convert", str(tmp_path / "a"), str(tmp_path / "b"),
              "--output", "lerobotv3", "--root", str(tmp_path / "out")])
    assert exit_.value.code == 1
    assert "cannot share one dataset" in capsys.readouterr().out
    assert not (tmp_path / "out").exists()


def test_lerobotv3_round_trip(tmp_path):
    pytest.importorskip("lerobot.datasets", reason="needs livekit-portal[lerobot]")
    from lerobot.datasets.lerobot_dataset import LeRobotDataset

    _record(tmp_path / "sessions", keypoints=TAKES, shadow=True)
    root = tmp_path / "dataset"
    with pytest.raises(SystemExit) as exit_:
        main(["convert", str(tmp_path / "sessions"), "--output", "lerobotv3",
              "--repo-id", "local/test", "--root", str(root), "--image-writer-threads", "1"])
    assert exit_.value.code == 0

    dataset = LeRobotDataset("local/test", root=root, video_backend="pyav")
    assert dataset.meta.fps == 30
    assert dataset.meta.total_episodes == 2
    assert dataset.num_frames == 10 + 12
    assert dataset.meta.features["observation.state"]["names"] == FIELDS
    assert [dataset.meta.episodes[i]["tasks"] for i in range(2)] == [["pick"], ["place"]]
    for i in range(dataset.num_frames):
        item = dataset[i]
        state = item["observation.state"].numpy()
        action = item["action"].numpy()
        np.testing.assert_array_equal(state, action)
        anchor = int(state[0]) // 10
        np.testing.assert_array_equal(state, list(_values(anchor).values()))
        # Video is lossy; the frame only has to be the right tick's.
        pixel = float(item["observation.images.wrist"].mean()) * 255
        assert abs(pixel - _pixel(anchor)) <= 4
