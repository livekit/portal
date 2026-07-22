"""Direct end-to-end test of the raw FFI role-split surface.

Every other integration test drives the Python facades in
`livekit.portal.__init__` (`Robot` / `Operator`), which wrap the unified
`_ffi.Portal`. This file instead exercises the role-split UniFFI objects
themselves — `_ffi.RobotConfig` / `_ffi.OperatorConfig` / `_ffi.Robot` /
`_ffi.Operator` — because those are what *other* language bindings (C#,
Swift, Kotlin) consume directly, and nothing else covers them.

It proves the Rust forwarding wired up in `livekit-portal-ffi/src/lib.rs`
is correct: each role exposes the right methods, they delegate to the
shared core `Portal`, and a full robot↔operator session works through the
new types end to end.

Skipped automatically without `LIVEKIT_URL` (see conftest).
"""
from __future__ import annotations

import asyncio

import pytest

from integration.conftest import URL, _make_token
from livekit.portal import livekit_portal_ffi as ffi

pytestmark = pytest.mark.asyncio


class _Recorder(ffi.PortalCallbacks):
    """Records every delivered event. FFI trait methods fire on the tokio
    worker thread — plain list appends are fine under CPython's GIL, and
    the test reads them only after an `asyncio.sleep` settle.
    """

    def __init__(self) -> None:
        self.actions: list = []
        self.states: list = []
        self.observations: list = []
        self.frames: list = []
        self.chunks: list = []
        self.active_changes: list = []

    def on_action(self, action) -> None:
        self.actions.append(action)

    def on_state(self, state) -> None:
        self.states.append(state)

    def on_observation(self, observation) -> None:
        self.observations.append(observation)

    def on_video_frame(self, track_name, frame) -> None:
        self.frames.append((track_name, frame))

    def on_drop(self, dropped) -> None:
        pass

    def on_action_chunk(self, chunk) -> None:
        self.chunks.append(chunk)

    def on_operator_joined(self, identity) -> None:
        pass

    def on_operator_left(self, identity) -> None:
        pass

    def on_active_operator_changed(self, identity) -> None:
        self.active_changes.append(identity)


def _f(name: str):
    return ffi.FieldSpec(name=name, dtype=ffi.DType.F32)


@pytest.fixture
async def ffi_pair():
    """Robot + Operator built from the raw FFI role objects, connected to a
    fresh room. State and action schemas match on both sides so state,
    observations, and actions all flow.
    """
    room = f"ffi-role-{int.from_bytes(__import__('os').urandom(4), 'big')}"

    robot_cfg = ffi.RobotConfig(room)
    robot_cfg.add_state_typed([_f("j")])
    robot_cfg.add_action_typed([_f("a")])
    robot_cfg.add_video("cam", ffi.VideoCodec.MJPEG, 90, None)

    operator_cfg = ffi.OperatorConfig(room)
    operator_cfg.add_state_typed([_f("j")])
    operator_cfg.add_action_typed([_f("a")])
    operator_cfg.add_video("cam", ffi.VideoCodec.MJPEG, 90, None)

    robot_cb = _Recorder()
    operator_cb = _Recorder()
    robot = ffi.Robot(robot_cfg, robot_cb)
    operator = ffi.Operator(operator_cfg, operator_cb)

    # Foreign async-trait dispatch (the RPC handler) needs a bound loop.
    ffi.uniffi_set_event_loop(asyncio.get_running_loop())

    await robot.connect(URL, _make_token("robot", room))
    await asyncio.sleep(0.2)
    await operator.connect(URL, _make_token("operator", room))
    await operator.set_active_operator(operator.local_identity())
    await asyncio.sleep(0.2)
    try:
        yield robot, operator, robot_cb, operator_cb
    finally:
        for side in (operator, robot):
            try:
                await side.disconnect()
            except Exception:  # noqa: BLE001
                pass


async def test_role_split_surface_present(ffi_pair):
    """The role objects expose only their role-appropriate publish methods
    and share the control plane. Asserts presence/absence rather than just
    behavior so an accidental method drop is caught.
    """
    robot, operator, _, _ = ffi_pair

    # Robot publishes state + video, receives actions; no send_action.
    assert hasattr(robot, "send_state")
    assert hasattr(robot, "send_video_frame")
    assert hasattr(robot, "get_action")
    assert not hasattr(robot, "send_action")
    assert not hasattr(robot, "get_observation")

    # Operator publishes actions, receives observations/state; no send_state.
    assert hasattr(operator, "send_action")
    assert hasattr(operator, "get_observation")
    assert hasattr(operator, "get_state")
    assert hasattr(operator, "robot_identity")
    assert not hasattr(operator, "send_state")
    assert not hasattr(operator, "send_video_frame")


async def test_control_plane(ffi_pair):
    """`set_active_operator` (operator→robot RPC) lands on the robot, and
    both sides agree on identities."""
    robot, operator, robot_cb, _ = ffi_pair

    op_id = operator.local_identity()
    assert op_id is not None
    assert robot.local_identity() is not None

    # Operator claimed itself in the fixture; the robot's pointer must mirror.
    assert robot.active_operator() == op_id
    assert operator.active_operator() == op_id
    assert operator.robot_identity() == robot.local_identity()
    assert op_id in robot.operators()
    # The robot saw the pointer change fire through its callback trait.
    assert op_id in robot_cb.active_changes


async def test_state_and_observation_flow(ffi_pair):
    """Robot state reaches the operator via both the pull API (`get_state`)
    and the synchronized observation path, plus the push callback."""
    robot, operator, _, operator_cb = ffi_pair

    robot.send_state({"j": 1.5}, None)
    robot.send_video_frame("cam", bytes(2 * 2 * 3), 2, 2, None)
    await asyncio.sleep(0.4)

    state = operator.get_state()
    assert state is not None
    assert state.values["j"] == pytest.approx(1.5)
    assert len(operator_cb.states) >= 1

    # An observation forms once a frame and a state line up.
    obs = operator.get_observation()
    assert obs is not None
    assert obs.state["j"] == pytest.approx(1.5)
    assert "cam" in obs.frames


async def test_action_flow(ffi_pair):
    """Operator action reaches the robot through the new Operator.send_action
    forward and the robot's pull API."""
    robot, operator, robot_cb, _ = ffi_pair

    operator.send_action({"a": 0.25}, None, None)
    await asyncio.sleep(0.3)

    action = robot.get_action()
    assert action is not None
    assert action.values["a"] == pytest.approx(0.25)
    assert action.sender == operator.local_identity()
    assert len(robot_cb.actions) >= 1


async def test_rpc_roundtrip():
    """A method registered on the robot is invocable from the operator via
    the forwarded `register_rpc_method` / `perform_rpc`.

    The handler is registered BEFORE connect — the supported path. The core
    stores pre-connect handlers and applies them inside `connect()`, which
    runs on the tokio runtime. Registering a *foreign* async handler after
    connect panics ("no reactor running") from the asyncio thread; that is a
    pre-existing limitation of the underlying SDK path and is identical
    through the Python facade, unrelated to the role-split wrappers.
    """
    import os

    room = f"ffi-rpc-{os.urandom(4).hex()}"
    robot_cfg = ffi.RobotConfig(room)
    robot_cfg.add_state_typed([_f("j")])
    operator_cfg = ffi.OperatorConfig(room)
    operator_cfg.add_state_typed([_f("j")])

    robot = ffi.Robot(robot_cfg, _Recorder())
    operator = ffi.Operator(operator_cfg, _Recorder())
    ffi.uniffi_set_event_loop(asyncio.get_running_loop())

    class _Echo(ffi.RpcHandler):
        async def handle(self, data) -> str:
            return f"pong:{data.payload}"

    robot.register_rpc_method("ping", _Echo())  # before connect

    try:
        await robot.connect(URL, _make_token("robot", room))
        await asyncio.sleep(0.2)
        await operator.connect(URL, _make_token("operator", room))
        await operator.set_active_operator(operator.local_identity())
        await asyncio.sleep(0.2)

        result = await operator.perform_rpc(
            robot.local_identity(), "ping", "hi", None
        )
        assert result == "pong:hi"
    finally:
        for side in (operator, robot):
            try:
                await side.disconnect()
            except Exception:  # noqa: BLE001
                pass


async def test_metrics_available(ffi_pair):
    """Metrics forward through the role objects and reflect traffic."""
    robot, operator, _, _ = ffi_pair

    robot.send_state({"j": 2.0}, None)
    await asyncio.sleep(0.2)

    assert operator.metrics().transport.states_received >= 1
    assert robot.metrics().transport.states_sent >= 1
    operator.reset_metrics()
    assert operator.metrics().transport.states_received == 0
