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
    ChunkSpec,
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
