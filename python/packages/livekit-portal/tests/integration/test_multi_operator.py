"""Integration tests for the v0.2 multi-controller surface.

Covers happy paths, handoff semantics, disconnect-stays-pinned,
reconnect-resumes, the action gate, and the supervisor pattern. Each test
runs against a real LiveKit server (`LIVEKIT_URL`) using the dev creds
defaulted by `conftest.py`. Skipped automatically when the URL is unset.
"""
from __future__ import annotations

import asyncio
import datetime
import os
import time
import uuid
from typing import Awaitable, Callable, List, Optional

import pytest

from livekit.portal import (
    DType,
    Operator,
    OperatorConfig,
    Robot,
    RobotConfig,
)


URL = os.environ.get("LIVEKIT_URL")
API_KEY = os.environ.get("LIVEKIT_API_KEY", "devkey")
API_SECRET = os.environ.get("LIVEKIT_API_SECRET", "secret")


def _room_name(prefix: str = "multi-op") -> str:
    return f"{prefix}-{int(time.time()*1000)}-{os.urandom(2).hex()}"


def _make_token(
    identity: str,
    room: str,
    *,
    attributes: Optional[dict] = None,
) -> str:
    from livekit import api

    grants = api.VideoGrants(
        room_join=True,
        room=room,
        can_publish=True,
        can_subscribe=True,
        can_update_own_metadata=True,
    )
    builder = (
        api.AccessToken(API_KEY, API_SECRET)
        .with_identity(identity)
        .with_grants(grants)
        .with_ttl(datetime.timedelta(hours=1))
    )
    if attributes:
        builder = builder.with_attributes(attributes)
    return builder.to_jwt()


def _action_schema() -> List[tuple]:
    return [("a", DType.F32)]


def _state_schema() -> List[tuple]:
    return [("s", DType.F32)]


def _make_robot(room: str, *, seed_active: Optional[str] = None) -> Robot:
    cfg = RobotConfig(room)
    cfg.add_state_typed(_state_schema())
    cfg.add_action_typed(_action_schema())
    return Robot(cfg)


def _make_operator(room: str, identity: str) -> Operator:
    cfg = OperatorConfig(room)
    cfg.add_state_typed(_state_schema())
    cfg.add_action_typed(_action_schema())
    return Operator(cfg)


async def _wait_for(
    predicate: Callable[[], bool],
    *,
    timeout: float = 3.0,
    interval: float = 0.02,
) -> bool:
    """Poll-with-timeout helper. Used to wait on attribute propagation
    rather than guessing at sleep durations.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        await asyncio.sleep(interval)
    return predicate()


# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_single_operator_self_claim_drives():
    """Happy path: one operator claims, sends action, robot's
    `on_action` fires.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    received: List[float] = []

    def on_action(action) -> None:
        received.append(action.values["a"])

    robot.on_action(on_action)
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        # Wait for role attributes to propagate so robot sees op1.
        assert await _wait_for(lambda: "op1" in robot.operators())
        assert op.robot_identity() == "robot"

        # Operator-side claim: RPCs the robot, robot sets attribute.
        await op.set_active_operator("op1")
        assert await _wait_for(lambda: robot.active_operator() == "op1")
        assert await _wait_for(lambda: op.active_operator() == "op1")

        op.send_action({"a": 1.5})
        assert await _wait_for(lambda: received and received[-1] == pytest.approx(1.5))
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_no_active_operator_drops_actions():
    """With `active_operator` unset (default `None`), the robot drops every
    action. `on_action` must not fire.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    received: List[float] = []
    robot.on_action(lambda a: received.append(a.values["a"]))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        assert await _wait_for(lambda: "op1" in robot.operators())
        assert robot.active_operator() is None

        # Send several actions; none should reach `on_action`.
        for v in (1.0, 2.0, 3.0):
            op.send_action({"a": v})
        await asyncio.sleep(0.3)
        assert received == []
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_two_operators_handoff():
    """A drives, B preempts, A's actions are now silently dropped at the
    gate, B's are accepted.
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a")
    op_b = _make_operator(room, "op-b")
    received: List[tuple] = []

    def on_action(action) -> None:
        received.append((robot.active_operator(), action.values["a"]))

    robot.on_action(on_action)
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op_a.connect(URL, _make_token("op-a", room))
        await op_b.connect(URL, _make_token("op-b", room))
        assert await _wait_for(
            lambda: {"op-a", "op-b"} <= set(robot.operators())
        )

        await op_a.set_active_operator("op-a")
        assert await _wait_for(lambda: robot.active_operator() == "op-a")

        op_a.send_action({"a": 1.0})
        op_b.send_action({"a": 99.0})  # gated, dropped
        assert await _wait_for(lambda: any(v[1] == pytest.approx(1.0) for v in received))
        # 99.0 must not appear.
        await asyncio.sleep(0.2)
        assert all(v[1] != 99.0 for v in received)

        # Hand off to B. From B's side, sets robot's attribute via RPC.
        await op_b.set_active_operator("op-b")
        assert await _wait_for(lambda: robot.active_operator() == "op-b")
        # Both operators see the change via attribute mirror.
        assert await _wait_for(lambda: op_a.active_operator() == "op-b")

        before_count = len(received)
        op_a.send_action({"a": 7.0})  # gated, dropped
        op_b.send_action({"a": 5.0})
        assert await _wait_for(lambda: len(received) > before_count)
        new = received[before_count:]
        assert all(v[1] != 7.0 for v in new)
        assert any(v[1] == pytest.approx(5.0) for v in new)
    finally:
        await op_a.disconnect()
        await op_b.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_token_seeded_active_operator():
    """Robot's token seeds `lk.portal.active_operator=op1`. Operator
    connects after and drives immediately without any RPC.
    """
    room = _room_name()
    cfg = RobotConfig(room)
    cfg.add_state_typed(_state_schema())
    cfg.add_action_typed(_action_schema())
    robot = Robot(cfg)
    op = _make_operator(room, "op1")
    received: List[float] = []
    robot.on_action(lambda a: received.append(a.values["a"]))
    try:
        await robot.connect(
            URL,
            _make_token("robot", room, attributes={"lk.portal.active_operator": "op1"}),
        )
        # Robot reads its own seeded attribute on connect.
        assert robot.active_operator() == "op1"

        await op.connect(URL, _make_token("op1", room))
        assert await _wait_for(lambda: "op1" in robot.operators())
        assert await _wait_for(lambda: op.active_operator() == "op1")

        op.send_action({"a": 4.2})
        assert await _wait_for(
            lambda: received and received[-1] == pytest.approx(4.2)
        )
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_active_operator_disconnect_stays_pinned():
    """Spec: when the active operator disconnects, the pointer stays
    pinned at the disconnected identity (not auto-cleared).
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        await op.set_active_operator("op1")
        assert await _wait_for(lambda: robot.active_operator() == "op1")

        await op.disconnect()
        # Operator left — pointer must NOT auto-clear.
        assert await _wait_for(lambda: "op1" not in robot.operators())
        assert robot.active_operator() == "op1"
    finally:
        try:
            await op.disconnect()
        except Exception:
            pass
        await robot.disconnect()


@pytest.mark.asyncio
async def test_reconnect_resumes_control():
    """Spec: with the pointer staying pinned, a disconnected operator can
    rejoin with the same identity and resume control with no re-claim.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    received: List[float] = []
    robot.on_action(lambda a: received.append(a.values["a"]))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        await op.set_active_operator("op1")
        assert await _wait_for(lambda: robot.active_operator() == "op1")
        op.send_action({"a": 1.0})
        assert await _wait_for(lambda: received and received[-1] == 1.0)

        await op.disconnect()
        # Pointer pinned — confirm before reconnect.
        assert robot.active_operator() == "op1"

        # Reconnect: new Operator instance, same identity.
        op2 = _make_operator(room, "op1")
        await op2.connect(URL, _make_token("op1", room))
        assert await _wait_for(lambda: "op1" in robot.operators())
        op2.send_action({"a": 9.9})
        assert await _wait_for(
            lambda: any(v == pytest.approx(9.9) for v in received)
        )
        await op2.disconnect()
    finally:
        await robot.disconnect()


@pytest.mark.asyncio
async def test_set_active_operator_to_none_drops_all():
    """Clearing the pointer (set to None / empty) silently drops every
    incoming action.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    received: List[float] = []
    robot.on_action(lambda a: received.append(a.values["a"]))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        await op.set_active_operator("op1")
        assert await _wait_for(lambda: robot.active_operator() == "op1")

        op.send_action({"a": 1.0})
        assert await _wait_for(lambda: received == [1.0])

        await op.set_active_operator(None)
        assert await _wait_for(lambda: robot.active_operator() is None)

        op.send_action({"a": 2.0})
        op.send_action({"a": 3.0})
        await asyncio.sleep(0.2)
        # Only the pre-clear value should be present.
        assert received == [1.0]
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_supervisor_pattern_third_party_arbitrates():
    """A third operator-role participant who never sends actions can still
    arbitrate by calling `set_active_operator`.
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a")
    op_b = _make_operator(room, "op-b")
    supervisor = _make_operator(room, "super")
    received: List[float] = []
    robot.on_action(lambda a: received.append(a.values["a"]))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op_a.connect(URL, _make_token("op-a", room))
        await op_b.connect(URL, _make_token("op-b", room))
        await supervisor.connect(URL, _make_token("super", room))
        assert await _wait_for(
            lambda: {"op-a", "op-b", "super"} <= set(robot.operators())
        )

        # Supervisor selects op-b without ever sending actions.
        await supervisor.set_active_operator("op-b")
        assert await _wait_for(lambda: robot.active_operator() == "op-b")
        op_a.send_action({"a": 1.0})  # dropped
        op_b.send_action({"a": 2.0})
        assert await _wait_for(lambda: any(v == 2.0 for v in received))
        await asyncio.sleep(0.2)
        assert all(v != 1.0 for v in received)
    finally:
        await supervisor.disconnect()
        await op_a.disconnect()
        await op_b.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_on_active_operator_changed_fires():
    """Operators get `on_active_operator_changed(new_identity)` events
    when the robot's pointer moves.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    seen: List[Optional[str]] = []
    op.on_active_operator_changed(lambda v: seen.append(v))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        assert await _wait_for(lambda: op.robot_identity() == "robot")

        await op.set_active_operator("op1")
        assert await _wait_for(lambda: any(v == "op1" for v in seen))

        await op.set_active_operator(None)
        assert await _wait_for(lambda: any(v is None for v in seen))
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_on_operator_joined_left_fires():
    """Robot sees `on_operator_joined` / `on_operator_left` for every
    operator joining and leaving.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    joined: List[str] = []
    left: List[str] = []
    robot.on_operator_joined(lambda i: joined.append(i))
    robot.on_operator_left(lambda i: left.append(i))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        assert await _wait_for(lambda: "op1" in joined)

        await op.disconnect()
        assert await _wait_for(lambda: "op1" in left)
    finally:
        try:
            await op.disconnect()
        except Exception:
            pass
        await robot.disconnect()


@pytest.mark.asyncio
async def test_chunk_dropped_when_sender_not_active():
    """Action chunks are gated at delivery time on the same rule as
    scalar actions: if `sender != active_operator` when the byte stream
    completes, the chunk is dropped before `on_action_chunk` fires.
    """
    room = _room_name()
    cfg_robot = RobotConfig(room)
    cfg_robot.add_state_typed(_state_schema())
    cfg_robot.add_action_chunk("ck", horizon=4, fields=[("a", DType.F32)])
    robot = Robot(cfg_robot)

    cfg_op_a = OperatorConfig(room)
    cfg_op_a.add_state_typed(_state_schema())
    cfg_op_a.add_action_chunk("ck", horizon=4, fields=[("a", DType.F32)])
    op_a = Operator(cfg_op_a)

    cfg_op_b = OperatorConfig(room)
    cfg_op_b.add_state_typed(_state_schema())
    cfg_op_b.add_action_chunk("ck", horizon=4, fields=[("a", DType.F32)])
    op_b = Operator(cfg_op_b)

    received: List[List[float]] = []

    def on_chunk(chunk) -> None:
        received.append(list(chunk.raw_data["a"]))

    robot.on_action_chunk("ck", on_chunk)

    try:
        await robot.connect(URL, _make_token("robot", room))
        await op_a.connect(URL, _make_token("op-a", room))
        await op_b.connect(URL, _make_token("op-b", room))
        assert await _wait_for(
            lambda: {"op-a", "op-b"} <= set(robot.operators())
        )

        await op_a.set_active_operator("op-a")
        assert await _wait_for(lambda: robot.active_operator() == "op-a")

        op_a.send_action_chunk("ck", {"a": [1.0, 2.0, 3.0, 4.0]})
        op_b.send_action_chunk("ck", {"a": [10.0, 20.0, 30.0, 40.0]})
        await asyncio.sleep(0.5)
        # Only A's chunk should have made it through.
        assert any(c[0] == pytest.approx(1.0) for c in received)
        assert all(c[0] != 10.0 for c in received)
    finally:
        await op_a.disconnect()
        await op_b.disconnect()
        await robot.disconnect()
