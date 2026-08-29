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

"""Tests for YAML config-file loading on PortalConfig / RobotConfig /
OperatorConfig.

The Rust core has its own deeper unit tests against the loader. These
tests exercise the FFI surface end-to-end: parse + validate happen in
Rust, and the Python wrapper mirrors the Rust-built schemas back into
its own state via the FFI accessors.
"""
import os
import tempfile
import textwrap

import pytest

from livekit.portal import (
    ConfigFileError,
    DType,
    FieldSpec,
    OperatorConfig,
    Portal,
    PortalConfig,
    RobotConfig,
    Role,
    VideoCodec,
)


YAML_FULL = textwrap.dedent(
    """
    version: 1
    fps: 60
    slack: 8
    tolerance: 1.0
    state_reliable: false
    action_reliable: false
    reuse_stale_frames: true
    ping_ms: 500
    action_subscription: true
    videos:
      - { name: front, codec: h264 }
      - { name: wrist, codec: mjpeg, quality: 80 }
      - { name: depth, codec: png }
    state:
      - { name: joint_pos, dtype: f32 }
      - { name: gripper, dtype: bool }
    action:
      - { name: joint_pos, dtype: f32 }
    action_chunks:
      - name: vla
        horizon: 16
        fields:
          - { name: joint_pos, dtype: f32 }
    """
)


def test_from_yaml_str_mirrors_full_schema():
    cfg = PortalConfig.from_yaml_str(YAML_FULL, "demo", Role.ROBOT)
    assert cfg.session == "demo"
    assert cfg.role == Role.ROBOT

    # H264 videos go on the WebRTC list, frame-video codecs on their own list.
    assert cfg.video_tracks == ["front"]
    assert [t.name for t in cfg.frame_video_tracks] == ["wrist", "depth"]
    assert cfg.frame_video_tracks[0].codec == VideoCodec.MJPEG
    assert cfg.frame_video_tracks[0].quality == 80
    assert cfg.frame_video_tracks[1].codec == VideoCodec.PNG

    assert cfg.state_schema == [
        FieldSpec(name="joint_pos", dtype=DType.F32),
        FieldSpec(name="gripper", dtype=DType.BOOL),
    ]
    assert cfg.action_schema == [FieldSpec(name="joint_pos", dtype=DType.F32)]

    assert len(cfg.action_chunks) == 1
    assert cfg.action_chunks[0].name == "vla"
    assert cfg.action_chunks[0].horizon == 16


def test_from_yaml_str_works_with_minimal_doc():
    cfg = PortalConfig.from_yaml_str("version: 1\n", "demo", Role.OPERATOR)
    assert cfg.session == "demo"
    assert cfg.role == Role.OPERATOR
    assert cfg.video_tracks == []
    assert cfg.frame_video_tracks == []
    assert cfg.state_schema == []
    assert cfg.action_schema == []
    assert cfg.action_chunks == []


def test_from_yaml_str_role_is_supplied_at_load_time():
    # Same YAML, two roles. The wire contract is identical; only role differs.
    robot = PortalConfig.from_yaml_str(YAML_FULL, "demo", Role.ROBOT)
    operator = PortalConfig.from_yaml_str(YAML_FULL, "demo", Role.OPERATOR)
    assert robot.state_schema == operator.state_schema
    assert robot.action_schema == operator.action_schema
    assert robot.role == Role.ROBOT
    assert operator.role == Role.OPERATOR


def test_from_yaml_str_unknown_version_rejected():
    with pytest.raises(ConfigFileError):
        PortalConfig.from_yaml_str("version: 99\n", "demo", Role.ROBOT)


def test_from_yaml_str_invalid_dtype_rejected():
    with pytest.raises(ConfigFileError):
        PortalConfig.from_yaml_str(
            "version: 1\nstate:\n  - { name: x, dtype: float64 }\n",
            "demo",
            Role.ROBOT,
        )


def test_from_yaml_str_duplicate_video_rejected():
    yaml = textwrap.dedent(
        """
        version: 1
        videos:
          - { name: cam, codec: h264 }
          - { name: cam, codec: mjpeg, quality: 80 }
        """
    )
    with pytest.raises(ConfigFileError):
        PortalConfig.from_yaml_str(yaml, "demo", Role.ROBOT)


def test_from_yaml_file_round_trip():
    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".yaml", delete=False
    ) as f:
        f.write(YAML_FULL)
        path = f.name
    try:
        cfg = PortalConfig.from_yaml_file(path, "demo", Role.ROBOT)
        assert cfg.video_tracks == ["front"]
        assert len(cfg.frame_video_tracks) == 2
        assert len(cfg.action_chunks) == 1
    finally:
        os.unlink(path)


def test_yaml_built_config_drives_portal():
    # The whole point: Portal construction works seamlessly with a
    # YAML-built PortalConfig. Verifies the Python-side mirror is
    # populated correctly (Portal reads chunk specs and field names
    # from the config).
    cfg = PortalConfig.from_yaml_str(YAML_FULL, "demo", Role.ROBOT)
    portal = Portal(cfg)
    assert portal._state_fields == ["joint_pos", "gripper"]
    assert portal._action_fields == ["joint_pos"]
    assert portal._video_tracks == ["front"]
    assert "vla" in portal._chunk_schemas


def test_robot_config_from_yaml_str():
    cfg = RobotConfig.from_yaml_str(YAML_FULL, "demo")
    assert cfg.role == Role.ROBOT
    assert [f.name for f in cfg.state_schema] == ["joint_pos", "gripper"]


def test_operator_config_from_yaml_str():
    cfg = OperatorConfig.from_yaml_str(YAML_FULL, "demo")
    assert cfg.role == Role.OPERATOR
    assert len(cfg.action_chunks) == 1


@pytest.mark.parametrize(
    "cfg",
    [
        PortalConfig.from_yaml_str(YAML_FULL, "demo", Role.ROBOT),
        RobotConfig.from_yaml_str(YAML_FULL, "demo"),
        OperatorConfig.from_yaml_str(YAML_FULL, "demo"),
    ],
    ids=["portal", "robot", "operator"],
)
def test_yaml_sync_knobs_are_readable(cfg):
    # A YAML-built config is the main case where the caller doesn't already
    # know these values — reading them back is the only way to find out.
    assert cfg.fps == 60
    assert cfg.slack == 8
    assert cfg.tolerance == pytest.approx(1.0)
    assert cfg.state_reliable is False
    assert cfg.action_reliable is False
    assert cfg.reuse_stale_frames is True
    assert cfg.ping_ms == 500
    assert cfg.action_subscription is True
    assert cfg.has_e2ee_key is False


def test_robot_config_from_yaml_file():
    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".yaml", delete=False
    ) as f:
        f.write(YAML_FULL)
        path = f.name
    try:
        cfg = RobotConfig.from_yaml_file(path, "demo")
        assert cfg.role == Role.ROBOT
    finally:
        os.unlink(path)


def test_stall_policy_parses_per_track():
    """Both knobs parse at either level, and a per-track value overrides the
    default without disturbing tracks that did not set one."""
    cfg = PortalConfig.from_yaml_str(
        textwrap.dedent(
            """
            version: 1
            on_stall: freeze
            max_lag_ms: 120
            videos:
              - { name: front, codec: h264 }
              - { name: wrist, codec: h264, on_stall: drop, max_lag_ms: 20 }
            """
        ),
        "demo",
        Role.ROBOT,
    )
    assert list(cfg.video_tracks) == ["front", "wrist"]


def test_unknown_stall_policy_is_rejected():
    """A typo must fail loudly rather than silently falling back to the
    default — a stall policy that quietly reverts is one you cannot trust in
    an incident."""
    with pytest.raises(ConfigFileError) as e:
        PortalConfig.from_yaml_str(
            textwrap.dedent(
                """
                version: 1
                videos:
                  - { name: front, codec: h264, on_stall: nope }
                """
            ),
            "demo",
            Role.ROBOT,
        )
    assert "nope" in str(e.value)
