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

"""Config builder smoke tests. No networking, no runtime."""
import pytest

from livekit.portal import (
    DEFAULT_MJPEG_QUALITY,
    DType,
    FieldSpec,
    OperatorConfig,
    Portal,
    PortalConfig,
    PortalError,
    RobotConfig,
    Role,
    VideoCodec,
)


def test_new_config_constructs():
    cfg = PortalConfig("demo", Role.OPERATOR)
    assert cfg.session == "demo"
    assert cfg.role == Role.OPERATOR


def test_config_adders_are_captured():
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_video("cam1")
    cfg.add_video("cam2")
    cfg.add_state_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.add_action_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    assert cfg.video_tracks == ["cam1", "cam2"]
    assert cfg.state_fields == ["j1", "j2", "j3"]
    assert cfg.action_fields == ["j1", "j2", "j3"]
    expected = [
        FieldSpec(name=n, dtype=DType.F32) for n in ("j1", "j2", "j3")
    ]
    assert cfg.state_schema == expected
    assert cfg.action_schema == expected


def test_simulcast_and_screencast_are_accepted_on_webrtc_codecs():
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_video("plain")
    cfg.add_video("pinned", screencast=True)
    cfg.add_video("layered", simulcast=True)
    cfg.add_video("both", simulcast=True, screencast=True)
    assert cfg.video_tracks == ["plain", "pinned", "layered", "both"]


def test_simulcast_and_screencast_are_keyword_only():
    cfg = PortalConfig("demo", Role.ROBOT)
    # Guards the signature: a fifth positional would silently land on a
    # future parameter if these ever stopped being keyword-only.
    with pytest.raises(TypeError):
        cfg.add_video("cam", VideoCodec.H264, DEFAULT_MJPEG_QUALITY, None, True)


def test_screencast_on_byte_stream_codec_is_ignored_not_rejected():
    # The programmatic API mirrors `max_bitrate_kbps`: silently ignored on
    # the byte-stream path. Only the YAML loader rejects it outright.
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_video("cam", codec=VideoCodec.MJPEG, quality=80, screencast=True)
    assert cfg.video_tracks == []
    assert [t.name for t in cfg.frame_video_tracks] == ["cam"]


def test_mixed_dtype_schema_is_accepted():
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_action_typed(
        [
            ("shoulder", DType.F32),
            ("gripper", DType.BOOL),
            ("mode", DType.I8),
            ("counter", DType.U16),
        ]
    )
    assert cfg.action_fields == ["shoulder", "gripper", "mode", "counter"]
    assert cfg.action_schema == [
        FieldSpec(name="shoulder", dtype=DType.F32),
        FieldSpec(name="gripper", dtype=DType.BOOL),
        FieldSpec(name="mode", dtype=DType.I8),
        FieldSpec(name="counter", dtype=DType.U16),
    ]


@pytest.mark.parametrize(
    "make_cfg",
    [
        lambda: PortalConfig("demo", Role.ROBOT),
        lambda: RobotConfig("demo"),
        lambda: OperatorConfig("demo"),
    ],
    ids=["portal", "robot", "operator"],
)
def test_config_knobs_default_and_round_trip(make_cfg):
    cfg = make_cfg()
    # Defaults, as documented in docs/03-portal-api.md.
    assert cfg.fps == 30
    assert cfg.slack == 5
    assert cfg.tolerance == pytest.approx(1.5)
    assert cfg.ping_ms == 1000
    assert cfg.state_reliable is True
    assert cfg.action_reliable is True
    assert cfg.reuse_stale_frames is False
    assert cfg.action_subscription is False
    assert cfg.has_e2ee_key is False

    cfg.set_fps(60)
    cfg.set_slack(8)
    cfg.set_tolerance(0.5)
    cfg.set_ping_ms(0)
    cfg.set_state_reliable(False)
    cfg.set_action_reliable(False)
    cfg.set_reuse_stale_frames(True)
    cfg.set_action_subscription(True)
    cfg.set_e2ee_key(b"0" * 32)

    assert cfg.fps == 60
    assert cfg.slack == 8
    assert cfg.tolerance == pytest.approx(0.5)
    assert cfg.ping_ms == 0
    assert cfg.state_reliable is False
    assert cfg.action_reliable is False
    assert cfg.reuse_stale_frames is True
    assert cfg.action_subscription is True
    # Presence only — the key bytes are deliberately not readable back.
    assert cfg.has_e2ee_key is True


def test_set_fps_zero_raises():
    cfg = PortalConfig("demo", Role.ROBOT)
    # The core `set_fps(0)` asserts; UniFFI surfaces the panic as an
    # `InternalError` from the generated module. We accept any Exception.
    with pytest.raises(Exception):
        cfg.set_fps(0)


def test_new_portal_echoes_declared_fields():
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_video("cam1")
    cfg.add_state_typed([("j1", DType.F64), ("j2", DType.F64)])
    cfg.add_action_typed([("j1", DType.F64), ("j2", DType.F64)])

    portal = Portal(cfg)
    # The Portal snapshots these from the core after construction.
    assert portal._state_fields == ["j1", "j2"]
    assert portal._action_fields == ["j1", "j2"]
    assert portal._video_tracks == ["cam1"]


def test_get_action_returns_none_when_empty():
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_action_typed([("j1", DType.F64)])
    portal = Portal(cfg)
    assert portal.get_action() is None
    assert portal.get_state() is None


def test_send_action_before_connect_is_wrong_role_error():
    # Robot role should be rejected from send_action (operator-only).
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_action_typed([("j1", DType.F64)])
    portal = Portal(cfg)
    with pytest.raises(PortalError.WrongRole):
        portal.send_action({"j1": 1.0})


def test_fieldspec_accepted_as_schema_entry():
    cfg = PortalConfig("demo", Role.ROBOT)
    cfg.add_action_typed(
        [FieldSpec(name="j1", dtype=DType.F32), ("j2", DType.F64)]
    )
    assert cfg.action_schema == [
        FieldSpec(name="j1", dtype=DType.F32),
        FieldSpec(name="j2", dtype=DType.F64),
    ]


# --- Typed delivery wrappers ------------------------------------------------
#
# These tests exercise the Python wrappers (`Action`, `State`, `Observation`)
# rather than the FFI records. We build an FFI record by hand and pass it
# through the same `_wrap_*` helpers the dispatcher uses on live deliveries.


def _mixed_schema_portal():
    cfg = PortalConfig("typed", Role.OPERATOR)
    cfg.add_action_typed(
        [
            ("shoulder", DType.F32),
            ("elbow", DType.F32),
            ("gripper", DType.BOOL),
            ("mode", DType.I8),
            ("counter", DType.U16),
        ]
    )
    cfg.add_state_typed(
        [
            ("j1", DType.F32),
            ("j2", DType.F32),
            ("estop", DType.BOOL),
        ]
    )
    return Portal(cfg)


def test_action_wrapper_values_are_typed_by_default():
    from livekit.portal import _wrap_action
    from livekit.portal import livekit_portal_ffi as _ffi

    portal = _mixed_schema_portal()
    ffi_action = _ffi.Action(
        values={
            "shoulder": 0.5,
            "elbow": -1.25,
            "gripper": 1.0,
            "mode": 3.0,
            "counter": 42.0,
        },
        timestamp_us=100,
        in_reply_to_ts_us=None,
        sender="",
    )
    action = _wrap_action(ffi_action, portal._action_schema)
    assert action.timestamp_us == 100
    # Typed by default.
    assert action.values == {
        "shoulder": 0.5,
        "elbow": -1.25,
        "gripper": True,
        "mode": 3,
        "counter": 42,
    }
    assert isinstance(action.values["gripper"], bool)
    assert isinstance(action.values["mode"], int)
    assert isinstance(action.values["shoulder"], float)
    # Raw escape hatch preserves the f64 dict.
    assert action.raw_values == {
        "shoulder": 0.5,
        "elbow": -1.25,
        "gripper": 1.0,
        "mode": 3.0,
        "counter": 42.0,
    }


def test_state_wrapper_values_are_typed_by_default():
    from livekit.portal import _wrap_state
    from livekit.portal import livekit_portal_ffi as _ffi

    portal = _mixed_schema_portal()
    ffi_state = _ffi.State(
        values={"j1": 0.1, "j2": -0.2, "estop": 1.0},
        timestamp_us=99,
    )
    state = _wrap_state(ffi_state, portal._state_schema)
    assert state.values == {"j1": 0.1, "j2": -0.2, "estop": True}
    assert isinstance(state.values["estop"], bool)
    assert state.raw_values["estop"] == 1.0


def test_observation_wrapper_exposes_typed_state():
    from livekit.portal import _wrap_observation
    from livekit.portal import livekit_portal_ffi as _ffi

    portal = _mixed_schema_portal()
    ffi_obs = _ffi.Observation(
        state={"j1": 0.1, "j2": 0.2, "estop": 0.0},
        frames={},
        timestamp_us=50,
    )
    obs = _wrap_observation(ffi_obs, portal._state_schema)
    assert obs.state == {"j1": 0.1, "j2": 0.2, "estop": False}
    assert obs.raw_state == {"j1": 0.1, "j2": 0.2, "estop": 0.0}
    assert obs.frames == {}
    assert obs.timestamp_us == 50


def test_wrapper_drops_fields_missing_from_payload():
    from livekit.portal import _wrap_action
    from livekit.portal import livekit_portal_ffi as _ffi

    portal = _mixed_schema_portal()
    ffi_action = _ffi.Action(
        values={"shoulder": 0.25},
        timestamp_us=0,
        in_reply_to_ts_us=None,
        sender="",
    )
    action = _wrap_action(ffi_action, portal._action_schema)
    # Partial payload → wrapper returns only the fields that were sent.
    assert action.values == {"shoulder": 0.25}
    assert action.raw_values == {"shoulder": 0.25}


# --- Send-side dtype enforcement --------------------------------------------
#
# The Python wrapper validates each outgoing value's Python type against
# the declared dtype before crossing the FFI boundary. A mismatch raises
# `PortalError.DtypeMismatch` at the earliest point so the caller sees
# the bug in their stack trace, not as a silent cast on the peer.


def test_send_rejects_float_for_bool_field():
    portal = _mixed_schema_portal()
    with pytest.raises(PortalError.DtypeMismatch) as info:
        portal.send_action({"gripper": 0.5})
    msg = str(info.value)
    assert "gripper" in msg
    assert "BOOL" in msg


def test_send_rejects_int_for_float_field_via_bool():
    # Python's `True` is also an int (1). A BOOL-looking value should
    # never slip into a float field.
    portal = _mixed_schema_portal()
    with pytest.raises(PortalError.DtypeMismatch):
        portal.send_action({"shoulder": True})


def test_send_rejects_bool_for_int_field():
    portal = _mixed_schema_portal()
    with pytest.raises(PortalError.DtypeMismatch):
        portal.send_action({"mode": True})


def test_send_rejects_float_for_int_field():
    portal = _mixed_schema_portal()
    with pytest.raises(PortalError.DtypeMismatch):
        portal.send_action({"mode": 3.5})


def test_send_accepts_int_for_float_field():
    # Numeric promotion: int is a valid float. Should pass validation
    # (and be rejected only by the publisher role check — our fixture is
    # OPERATOR so sending an action is the right role).
    portal = _mixed_schema_portal()
    # Does not raise at the validation step.
    try:
        portal.send_action({"shoulder": 1, "gripper": True, "mode": 3})
    except PortalError.DtypeMismatch:
        pytest.fail("int should coerce into a float-declared field")
    except PortalError:
        # May raise a different PortalError because there's no connected
        # peer / role — that's fine. We only care that validation passed.
        pass


def test_send_unknown_key_is_not_blocked_by_validator():
    # Unknown keys skip the dtype check — the core publisher warns about
    # them separately. Validator shouldn't raise for keys not in schema.
    portal = _mixed_schema_portal()
    try:
        portal.send_action({"gripper": True, "unknown_key": 1.0})
    except PortalError.DtypeMismatch:
        pytest.fail("unknown keys must not trigger dtype validation")
    except PortalError:
        pass


def test_send_accepts_numpy_scalars():
    # ML code routinely hands us `np.bool_`, `np.int32`, `np.float32` from
    # policy tensors. The validator has to accept them or the SDK is
    # unusable for that audience.
    np = pytest.importorskip("numpy")
    portal = _mixed_schema_portal()
    try:
        portal.send_action(
            {
                "shoulder": np.float32(0.5),
                "elbow": np.float64(-0.25),
                "gripper": np.bool_(True),
                "mode": np.int8(3),
                "counter": np.uint16(42),
            }
        )
    except PortalError.DtypeMismatch as e:
        pytest.fail(f"numpy scalars rejected: {e}")
    except PortalError:
        # WrongRole or similar — validation passed, that's what we test.
        pass


def test_send_rejects_numpy_bool_for_int_field():
    # Symmetry with Python bool: `np.bool_` must not satisfy an int
    # field, otherwise a bug where someone stashed a bool in a mode
    # field would slip through.
    np = pytest.importorskip("numpy")
    portal = _mixed_schema_portal()
    with pytest.raises(PortalError.DtypeMismatch):
        portal.send_action({"mode": np.bool_(True)})


# --- Action chunk schema + wrappers ----------------------------------------


def test_add_action_chunk_records_spec():
    cfg = PortalConfig("vla", Role.OPERATOR)
    cfg.add_action_chunk(
        "act",
        horizon=10,
        fields=[("j1", DType.F32), ("j2", DType.F32)],
    )
    chunks = cfg.action_chunks
    assert len(chunks) == 1
    assert chunks[0].name == "act"
    assert chunks[0].horizon == 10
    assert [f.name for f in chunks[0].fields] == ["j1", "j2"]


def test_action_chunk_wrapper_reconstructs_numpy_arrays():
    np = pytest.importorskip("numpy")
    from livekit.portal import _wrap_action_chunk
    from livekit.portal import livekit_portal_ffi as _ffi

    cfg = PortalConfig("vla", Role.OPERATOR)
    cfg.add_action_chunk(
        "act",
        horizon=4,
        fields=[("j1", DType.F32), ("gripper", DType.BOOL)],
    )
    portal = Portal(cfg)
    ffi_chunk = _ffi.ActionChunk(
        name="act",
        horizon=4,
        data={"j1": [0.0, 0.5, 1.0, 1.5], "gripper": [0.0, 1.0, 1.0, 0.0]},
        timestamp_us=42,
        in_reply_to_ts_us=10,
        sender="",
    )
    chunk = _wrap_action_chunk(ffi_chunk, portal._chunk_schemas)
    assert chunk.name == "act"
    assert chunk.horizon == 4
    assert chunk.timestamp_us == 42
    assert chunk.in_reply_to_ts_us == 10
    # Reconstructed columns are numpy arrays of declared dtype.
    assert chunk.data["j1"].dtype == np.float32
    assert chunk.data["gripper"].dtype == np.bool_
    # Raw view stays as f64 lists.
    assert chunk.raw_data["j1"] == [0.0, 0.5, 1.0, 1.5]


def test_send_action_chunk_accepts_dict_columns():
    np = pytest.importorskip("numpy")
    cfg = PortalConfig("vla", Role.OPERATOR)
    cfg.add_action_chunk(
        "act",
        horizon=3,
        fields=[("j1", DType.F32)],
    )
    portal = Portal(cfg)
    # Dict-of-arrays input — accepted, even though no peer is connected
    # (PortalError on send is fine; we only care that the validator path
    # didn't reject the shape).
    try:
        portal.send_action_chunk("act", {"j1": np.array([0.1, 0.2, 0.3])})
    except PortalError.UnknownChunk:
        pytest.fail("declared chunk name treated as unknown")
    except PortalError:
        # send may fail with a different PortalError because there's no
        # connected peer — that's the expected path.
        pass


def test_send_action_chunk_accepts_2d_ndarray():
    np = pytest.importorskip("numpy")
    cfg = PortalConfig("vla", Role.OPERATOR)
    cfg.add_action_chunk(
        "act",
        horizon=3,
        fields=[("j1", DType.F32), ("j2", DType.F32)],
    )
    portal = Portal(cfg)
    arr = np.array([[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]], dtype=np.float32)
    try:
        portal.send_action_chunk("act", arr)
    except PortalError.UnknownChunk:
        pytest.fail("declared chunk name treated as unknown")
    except PortalError:
        pass


# --- Chunk send-side dtype enforcement --------------------------------------
#
# Parity with the scalar checks above. The dict form is validated per column
# with the same category rules `_validate_send_values` applies to a scalar.
# The 2D ndarray form is deliberately exempt: one uniform policy tensor
# fanned across a mixed schema is the case that shape exists for.


def _chunk_portal():
    cfg = PortalConfig("vla", Role.OPERATOR)
    cfg.add_action_chunk(
        "act",
        horizon=2,
        fields=[("j1", DType.F32), ("gripper", DType.BOOL), ("mode", DType.I8)],
    )
    return Portal(cfg)


def test_send_chunk_rejects_float_column_for_bool_field():
    portal = _chunk_portal()
    with pytest.raises(PortalError.DtypeMismatch) as info:
        portal.send_action_chunk("act", {"gripper": [0.0, 1.0]})
    msg = str(info.value)
    assert "gripper" in msg
    assert "BOOL" in msg


def test_send_chunk_rejects_float_column_for_int_field():
    portal = _chunk_portal()
    with pytest.raises(PortalError.DtypeMismatch):
        portal.send_action_chunk("act", {"mode": [3.5, 1.0]})


def test_send_chunk_rejects_bool_column_for_int_field():
    portal = _chunk_portal()
    with pytest.raises(PortalError.DtypeMismatch):
        portal.send_action_chunk("act", {"mode": [True, False]})


def test_send_chunk_rejects_float_ndarray_column_for_bool_field():
    # The per-column ndarray check reads `column.dtype` once rather than
    # walking `horizon` elements, so cover it separately from the list path.
    np = pytest.importorskip("numpy")
    portal = _chunk_portal()
    with pytest.raises(PortalError.DtypeMismatch) as info:
        portal.send_action_chunk(
            "act", {"gripper": np.array([0.0, 1.0], dtype=np.float32)}
        )
    assert "gripper" in str(info.value)


def test_send_chunk_accepts_correctly_typed_columns():
    np = pytest.importorskip("numpy")
    portal = _chunk_portal()
    try:
        portal.send_action_chunk(
            "act",
            {
                "j1": np.array([0.5, 0.25], dtype=np.float32),
                "gripper": np.array([True, False]),
                "mode": [3, -1],
            },
        )
    except PortalError.DtypeMismatch as e:
        pytest.fail(f"correctly typed chunk columns rejected: {e}")
    except PortalError:
        # No connected peer — validation passed, which is what we test.
        pass


def test_send_chunk_accepts_int_column_for_float_field():
    # Same numeric promotion the scalar path allows.
    portal = _chunk_portal()
    try:
        portal.send_action_chunk("act", {"j1": [1, 2]})
    except PortalError.DtypeMismatch:
        pytest.fail("int column should coerce into a float-declared field")
    except PortalError:
        pass


def test_send_chunk_unknown_column_is_not_blocked_by_validator():
    portal = _chunk_portal()
    try:
        portal.send_action_chunk("act", {"j1": [0.5, 0.25], "nope": [1.0, 2.0]})
    except PortalError.DtypeMismatch:
        pytest.fail("unknown chunk columns must not trigger dtype validation")
    except PortalError:
        pass


def test_send_chunk_ndarray_form_waives_the_dtype_check():
    # A uniform f32 tensor against a schema with a BOOL and an I8 field.
    # Per-field checking would reject this, and it is the documented shape
    # for VLA output, so it has to pass.
    np = pytest.importorskip("numpy")
    portal = _chunk_portal()
    arr = np.array([[0.5, 1.0, 3.0], [0.25, 0.0, -1.0]], dtype=np.float32)
    try:
        portal.send_action_chunk("act", arr)
    except PortalError.DtypeMismatch as e:
        pytest.fail(f"uniform ndarray form must not be dtype-checked: {e}")
    except PortalError:
        pass


def test_send_chunk_short_column_still_accepted():
    # Length is a warn-and-pad in the core, not a rejection. Only the dtype
    # check raises.
    portal = _chunk_portal()
    try:
        portal.send_action_chunk("act", {"j1": [0.5]})
    except PortalError.DtypeMismatch:
        pytest.fail("a short column must not raise DtypeMismatch")
    except PortalError:
        pass


def test_send_action_chunk_unknown_name_raises():
    cfg = PortalConfig("vla", Role.OPERATOR)
    cfg.add_action_chunk("act", horizon=3, fields=[("j1", DType.F32)])
    portal = Portal(cfg)
    with pytest.raises(PortalError.UnknownChunk):
        portal.send_action_chunk("never_declared", {"j1": [0.0, 0.1, 0.2]})


def test_send_action_passes_in_reply_to_ts_us_through_validator():
    cfg = PortalConfig("act", Role.OPERATOR)
    cfg.add_action_typed([("j1", DType.F32)])
    portal = Portal(cfg)
    # Passing the kwarg shouldn't trigger a DtypeMismatch path. We expect
    # any non-mismatch PortalError (no peer) to be fine.
    try:
        portal.send_action({"j1": 0.5}, in_reply_to_ts_us=12345)
    except PortalError.DtypeMismatch:
        pytest.fail("in_reply_to_ts_us must not affect dtype validation")
    except PortalError:
        pass
