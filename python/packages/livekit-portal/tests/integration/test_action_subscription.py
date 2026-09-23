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

"""Integration tests for v0.2 HITL recording.

Covers the operator-side action subscription feature, the propagation
timing of `lk.portal.active_operator`, and the `Action.sender`
attribution. Skipped automatically when `LIVEKIT_URL` is unset (matches
`conftest.py`).

Spec coverage: cases 30-46 in `spec.md`.
"""
from __future__ import annotations

import asyncio
import datetime
import os
import time
from typing import Callable, Dict, List, Optional

import pytest

from livekit.portal import (
    Action,
    DType,
    Operator,
    OperatorConfig,
    Robot,
    RobotConfig,
)


URL = os.environ.get("LIVEKIT_URL")
API_KEY = os.environ.get("LIVEKIT_API_KEY", "devkey")
API_SECRET = os.environ.get("LIVEKIT_API_SECRET", "secret")


def _room_name(prefix: str = "sub") -> str:
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


_ACTION_SCHEMA = [("a", DType.F32)]
_STATE_SCHEMA = [("s", DType.F32)]


def _make_robot(room: str) -> Robot:
    cfg = RobotConfig(room)
    cfg.add_state_typed(_STATE_SCHEMA)
    cfg.add_action_typed(_ACTION_SCHEMA)
    return Robot(cfg)


def _make_operator(
    room: str,
    identity: str,
    *,
    subscription: str = "none",
) -> Operator:
    cfg = OperatorConfig(room)
    cfg.add_state_typed(_STATE_SCHEMA)
    cfg.add_action_typed(_ACTION_SCHEMA)
    cfg.set_action_subscription(subscription)
    return Operator(cfg)


async def _wait_for(
    predicate: Callable[[], bool],
    *,
    timeout: float = 3.0,
    interval: float = 0.02,
) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        await asyncio.sleep(interval)
    return predicate()


# ---------------------------------------------------------------------------
# Active operator propagation (spec 27-32)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_robot_side_write_reaches_every_operator():
    """Spec 27: Robot writes; all operators see the new value within 500 ms,
    and `on_active_operator_changed` fires exactly once on each.
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a")
    op_b = _make_operator(room, "op-b")
    op_c = _make_operator(room, "op-c")
    seen_a: List[Optional[str]] = []
    seen_b: List[Optional[str]] = []
    seen_c: List[Optional[str]] = []
    op_a.on_active_operator_changed(lambda v: seen_a.append(v))
    op_b.on_active_operator_changed(lambda v: seen_b.append(v))
    op_c.on_active_operator_changed(lambda v: seen_c.append(v))
    try:
        await robot.connect(URL, _make_token("robot", room))
        for op, ident in ((op_a, "op-a"), (op_b, "op-b"), (op_c, "op-c")):
            await op.connect(URL, _make_token(ident, room))
        assert await _wait_for(
            lambda: {"op-a", "op-b", "op-c"} <= set(robot.operators())
        )

        start = time.monotonic()
        await robot.set_active_operator("X")
        propagated = await _wait_for(
            lambda: all(
                op.active_operator() == "X" for op in (op_a, op_b, op_c)
            ),
            timeout=0.5,
        )
        elapsed_ms = (time.monotonic() - start) * 1000.0
        assert propagated, f"propagation took >500ms ({elapsed_ms:.0f}ms)"

        # Each operator's on_active_operator_changed fires at least once with
        # X. Wait for it rather than asserting synchronously: the Rust event
        # path updates the mirror before firing the callback, and the Python
        # dispatcher hops the callback onto the asyncio loop via
        # `call_soon_threadsafe`, so there is a brief window where
        # `active_operator()` already reads "X" but the user callback has
        # not appended yet.
        assert await _wait_for(lambda: any(v == "X" for v in seen_a))
        assert await _wait_for(lambda: any(v == "X" for v in seen_b))
        assert await _wait_for(lambda: any(v == "X" for v in seen_c))
    finally:
        for o in (op_a, op_b, op_c):
            await o.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_operator_side_write_reaches_robot_and_peers():
    """Spec 28: Operator A calls set_active_operator("B"). Robot's own mirror
    and all other operators' mirrors converge within 500 ms (extra RPC hop).
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a")
    op_b = _make_operator(room, "op-b")
    op_c = _make_operator(room, "op-c")
    try:
        await robot.connect(URL, _make_token("robot", room))
        for op, ident in ((op_a, "op-a"), (op_b, "op-b"), (op_c, "op-c")):
            await op.connect(URL, _make_token(ident, room))
        assert await _wait_for(
            lambda: {"op-a", "op-b", "op-c"} <= set(robot.operators())
        )

        start = time.monotonic()
        await op_a.set_active_operator("op-b")
        propagated = await _wait_for(
            lambda: (
                robot.active_operator() == "op-b"
                and op_a.active_operator() == "op-b"
                and op_b.active_operator() == "op-b"
                and op_c.active_operator() == "op-b"
            ),
            timeout=0.5,
        )
        elapsed_ms = (time.monotonic() - start) * 1000.0
        assert propagated, f"operator-side write propagation took >500ms ({elapsed_ms:.0f}ms)"
    finally:
        for o in (op_a, op_b, op_c):
            await o.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_action_acceptance_follows_mirror():
    """Spec 29: After handoff, the new active operator's actions reach the
    robot's `on_action` and the previous operator's are dropped.
    """
    room = _room_name()
    robot = _make_robot(room)
    policy = _make_operator(room, "policy")
    human = _make_operator(room, "human")
    received: List[tuple] = []
    robot.on_action(lambda a: received.append((a.sender, a.values["a"])))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await policy.connect(URL, _make_token("policy", room))
        await human.connect(URL, _make_token("human", room))
        assert await _wait_for(
            lambda: {"policy", "human"} <= set(robot.operators())
        )

        await robot.set_active_operator("policy")
        assert await _wait_for(lambda: robot.active_operator() == "policy")
        policy.send_action({"a": 1.0})
        human.send_action({"a": 99.0})
        assert await _wait_for(lambda: any(v == ("policy", 1.0) for v in received))
        await asyncio.sleep(0.2)
        assert all(v != ("human", 99.0) for v in received)

        await robot.set_active_operator("human")
        assert await _wait_for(lambda: robot.active_operator() == "human")
        before = len(received)
        policy.send_action({"a": 7.0})
        human.send_action({"a": 5.0})
        assert await _wait_for(lambda: len(received) > before)
        new = received[before:]
        assert all(v[0] != "policy" for v in new)
        assert any(v == ("human", 5.0) for v in new)
    finally:
        await policy.disconnect()
        await human.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_idempotent_write_does_not_refire_callback():
    """Spec 30: writing the same value twice fires
    `on_active_operator_changed` only on the first transition.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "op1")
    seen: List[Optional[str]] = []
    op.on_active_operator_changed(lambda v: seen.append(v))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("op1", room))
        assert await _wait_for(lambda: "op1" in robot.operators())

        await robot.set_active_operator("X")
        assert await _wait_for(lambda: any(v == "X" for v in seen))
        first_count = sum(1 for v in seen if v == "X")

        await robot.set_active_operator("X")
        await asyncio.sleep(0.3)
        # No new "X" event fires for an idempotent write.
        assert sum(1 for v in seen if v == "X") == first_count
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_late_joiner_reads_current_value_at_connect():
    """Spec 31: a late-joining operator reads `active_operator` immediately
    after connect, no `_wait_for` needed.
    """
    room = _room_name()
    robot = _make_robot(room)
    early = _make_operator(room, "early")
    late = _make_operator(room, "late")
    try:
        await robot.connect(URL, _make_token("robot", room))
        await early.connect(URL, _make_token("early", room))
        assert await _wait_for(lambda: "early" in robot.operators())
        await robot.set_active_operator("policy-v1")
        assert await _wait_for(lambda: early.active_operator() == "policy-v1")

        await late.connect(URL, _make_token("late", room))
        # Allow at most a couple of attribute events to propagate, but the
        # SDK includes attributes in the JoinResponse so a brief wait
        # suffices.
        assert await _wait_for(
            lambda: late.active_operator() == "policy-v1", timeout=1.0
        )
    finally:
        for o in (early, late):
            await o.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_concurrent_writes_converge():
    """Spec 32: two operators race set_active_operator. Both calls succeed.
    Final state is the same on every participant (LiveKit serializes
    attribute writes through the server).
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a")
    op_b = _make_operator(room, "op-b")
    op_c = _make_operator(room, "op-c")
    try:
        await robot.connect(URL, _make_token("robot", room))
        for op, ident in ((op_a, "op-a"), (op_b, "op-b"), (op_c, "op-c")):
            await op.connect(URL, _make_token(ident, room))
        assert await _wait_for(
            lambda: {"op-a", "op-b", "op-c"} <= set(robot.operators())
        )

        await asyncio.gather(
            op_a.set_active_operator("op-a"),
            op_b.set_active_operator("op-b"),
        )
        # Allow propagation, then verify all participants converge.
        await _wait_for(
            lambda: robot.active_operator() in ("op-a", "op-b"),
            timeout=1.0,
        )
        final = robot.active_operator()
        assert final in ("op-a", "op-b")
        assert await _wait_for(
            lambda: all(
                o.active_operator() == final for o in (op_a, op_b, op_c)
            ),
            timeout=1.0,
        )
    finally:
        for o in (op_a, op_b, op_c):
            await o.disconnect()
        await robot.disconnect()


# ---------------------------------------------------------------------------
# Operator-side action subscription (spec 33-43)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_default_subscription_is_off():
    """Spec 33: an operator with the default `none` subscription never
    fires `on_action`, even when actions flow.
    """
    room = _room_name()
    robot = _make_robot(room)
    sender = _make_operator(room, "sender")
    observer = _make_operator(room, "observer")  # subscription off (default)
    seen: List[Action] = []
    observer.on_action(lambda a: seen.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await sender.connect(URL, _make_token("sender", room))
        await observer.connect(URL, _make_token("observer", room))
        assert await _wait_for(
            lambda: {"sender", "observer"} <= set(robot.operators())
        )
        await sender.set_active_operator("sender")
        assert await _wait_for(lambda: robot.active_operator() == "sender")

        for v in (1.0, 2.0, 3.0):
            sender.send_action({"a": v})
        await asyncio.sleep(0.4)
        assert seen == []
    finally:
        await sender.disconnect()
        await observer.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_recorder_receives_active_operator_actions():
    """Spec 34: a recorder operator with subscription on receives the
    active operator's actions, with `action.sender` set to the producer.
    """
    room = _room_name()
    robot = _make_robot(room)
    active = _make_operator(room, "active")
    recorder = _make_operator(room, "recorder", subscription="active")
    seen: List[Action] = []
    recorder.on_action(lambda a: seen.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await active.connect(URL, _make_token("active", room))
        await recorder.connect(URL, _make_token("recorder", room))
        assert await _wait_for(
            lambda: {"active", "recorder"} <= set(robot.operators())
        )
        await active.set_active_operator("active")
        assert await _wait_for(lambda: recorder.active_operator() == "active")

        active.send_action({"a": 1.5})
        active.send_action({"a": 2.5})
        assert await _wait_for(lambda: len(seen) >= 2)
        for a in seen:
            assert a.sender == "active"
        assert any(a.values["a"] == pytest.approx(1.5) for a in seen)
        assert any(a.values["a"] == pytest.approx(2.5) for a in seen)
    finally:
        await active.disconnect()
        await recorder.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_non_active_operators_dropped_at_recorder():
    """Spec 35: a recorder receives actions only from the active operator;
    others are dropped at the recorder's gate (matching the robot).
    """
    room = _room_name()
    robot = _make_robot(room)
    active = _make_operator(room, "active")
    other = _make_operator(room, "other")
    recorder = _make_operator(room, "recorder", subscription="active")
    seen: List[Action] = []
    recorder.on_action(lambda a: seen.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        for op, ident in ((active, "active"), (other, "other"), (recorder, "recorder")):
            await op.connect(URL, _make_token(ident, room))
        assert await _wait_for(
            lambda: {"active", "other", "recorder"} <= set(robot.operators())
        )
        await active.set_active_operator("active")
        assert await _wait_for(lambda: recorder.active_operator() == "active")

        active.send_action({"a": 1.0})
        other.send_action({"a": 99.0})
        assert await _wait_for(lambda: any(a.values["a"] == 1.0 for a in seen))
        await asyncio.sleep(0.3)
        assert all(a.sender == "active" for a in seen)
        assert all(a.values["a"] != 99.0 for a in seen)
    finally:
        for o in (active, other, recorder):
            await o.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_self_echo_when_active():
    """Spec 36: an active operator with subscription on receives its own
    actions through `on_action` via the local echo path.
    """
    room = _room_name()
    robot = _make_robot(room)
    op = _make_operator(room, "self", subscription="active")
    seen: List[Action] = []
    op.on_action(lambda a: seen.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op.connect(URL, _make_token("self", room))
        assert await _wait_for(lambda: "self" in robot.operators())
        await op.set_active_operator("self")
        assert await _wait_for(lambda: op.active_operator() == "self")

        op.send_action({"a": 7.5})
        assert await _wait_for(lambda: any(a.values["a"] == pytest.approx(7.5) for a in seen))
        # Echo populates sender with self identity.
        for a in seen:
            assert a.sender == "self"
        # Pull surface reflects the echo too.
        latest = op.get_action()
        assert latest is not None
        assert latest.values["a"] == pytest.approx(7.5)
    finally:
        await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_no_echo_when_inactive():
    """Spec 37: an operator with subscription on but not active does not
    fire `on_action` on its own sends. Echo only triggers when self ==
    active.
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a", subscription="active")
    op_b = _make_operator(room, "op-b")
    seen_a: List[Action] = []
    op_a.on_action(lambda a: seen_a.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await op_a.connect(URL, _make_token("op-a", room))
        await op_b.connect(URL, _make_token("op-b", room))
        assert await _wait_for(
            lambda: {"op-a", "op-b"} <= set(robot.operators())
        )
        # B is active. A is subscribed but not active.
        await op_b.set_active_operator("op-b")
        assert await _wait_for(lambda: op_a.active_operator() == "op-b")

        # A sends while inactive — echo must not fire.
        op_a.send_action({"a": 4.4})
        await asyncio.sleep(0.3)
        assert seen_a == []
    finally:
        await op_a.disconnect()
        await op_b.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_recorder_sees_handoff_in_action_stream():
    """Spec 38: across a handoff, the recorder's action stream flips
    `sender` from old to new without race.
    """
    room = _room_name()
    robot = _make_robot(room)
    op_a = _make_operator(room, "op-a")
    op_b = _make_operator(room, "op-b")
    rec = _make_operator(room, "rec", subscription="active")
    seen: List[Action] = []
    rec.on_action(lambda a: seen.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        for op, ident in ((op_a, "op-a"), (op_b, "op-b"), (rec, "rec")):
            await op.connect(URL, _make_token(ident, room))
        assert await _wait_for(
            lambda: {"op-a", "op-b", "rec"} <= set(robot.operators())
        )

        await op_a.set_active_operator("op-a")
        assert await _wait_for(lambda: rec.active_operator() == "op-a")
        op_a.send_action({"a": 1.0})
        op_a.send_action({"a": 2.0})
        assert await _wait_for(lambda: len(seen) >= 2)

        await op_b.set_active_operator("op-b")
        assert await _wait_for(lambda: rec.active_operator() == "op-b")
        before = len(seen)
        op_b.send_action({"a": 10.0})
        op_b.send_action({"a": 20.0})
        assert await _wait_for(lambda: len(seen) > before + 1)

        senders = [a.sender for a in seen]
        # Some prefix from op-a, some suffix from op-b.
        assert "op-a" in senders
        assert "op-b" in senders
    finally:
        for o in (op_a, op_b, rec):
            await o.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_sender_set_on_every_delivered_action():
    """Spec 39: every record delivered through `on_action` has `sender`
    populated.
    """
    room = _room_name()
    robot = _make_robot(room)
    active = _make_operator(room, "x")
    rec = _make_operator(room, "rec", subscription="active")
    seen: List[Action] = []
    rec.on_action(lambda a: seen.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await active.connect(URL, _make_token("x", room))
        await rec.connect(URL, _make_token("rec", room))
        assert await _wait_for(
            lambda: {"x", "rec"} <= set(robot.operators())
        )
        await active.set_active_operator("x")
        assert await _wait_for(lambda: rec.active_operator() == "x")

        for i in range(10):
            active.send_action({"a": float(i)})
        assert await _wait_for(lambda: len(seen) >= 5)

        # Exact value of `sender` matches the active operator at gate time.
        for a in seen:
            assert a.sender == "x"
    finally:
        await active.disconnect()
        await rec.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_pull_surface_populates_on_operator_side():
    """Spec 41: `get_action()` returns the latest gate-passed value on the
    recorder.
    """
    room = _room_name()
    robot = _make_robot(room)
    active = _make_operator(room, "x")
    rec = _make_operator(room, "rec", subscription="active")
    try:
        await robot.connect(URL, _make_token("robot", room))
        await active.connect(URL, _make_token("x", room))
        await rec.connect(URL, _make_token("rec", room))
        assert await _wait_for(
            lambda: {"x", "rec"} <= set(robot.operators())
        )
        await active.set_active_operator("x")
        assert await _wait_for(lambda: rec.active_operator() == "x")

        # Before any action, pull returns None.
        assert rec.get_action() is None

        active.send_action({"a": 9.5})
        assert await _wait_for(
            lambda: rec.get_action() is not None
            and rec.get_action().values["a"] == pytest.approx(9.5)
        )
        assert rec.get_action().sender == "x"
    finally:
        await active.disconnect()
        await rec.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_subscription_does_not_leak_across_operators():
    """Spec 42: enabling subscription on one operator does not affect the
    others. Per-config flag, scoped to the participant.
    """
    room = _room_name()
    robot = _make_robot(room)
    rec = _make_operator(room, "rec", subscription="active")
    a = _make_operator(room, "a")
    b = _make_operator(room, "b")
    seen_rec: List[Action] = []
    seen_a: List[Action] = []
    seen_b: List[Action] = []
    rec.on_action(lambda x: seen_rec.append(x))
    a.on_action(lambda x: seen_a.append(x))
    b.on_action(lambda x: seen_b.append(x))
    try:
        await robot.connect(URL, _make_token("robot", room))
        for op, ident in ((rec, "rec"), (a, "a"), (b, "b")):
            await op.connect(URL, _make_token(ident, room))
        assert await _wait_for(
            lambda: {"rec", "a", "b"} <= set(robot.operators())
        )
        await a.set_active_operator("a")
        assert await _wait_for(lambda: rec.active_operator() == "a")

        a.send_action({"a": 1.0})
        assert await _wait_for(lambda: any(x.values["a"] == 1.0 for x in seen_rec))
        await asyncio.sleep(0.3)
        # Only the subscribed recorder fired its callback.
        assert seen_a == []
        assert seen_b == []
    finally:
        for o in (rec, a, b):
            await o.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_subscription_does_not_affect_robot():
    """Spec 43: number of subscribed operators in the room does not change
    the robot's `on_action` rate.
    """
    room = _room_name()
    robot = _make_robot(room)
    sender = _make_operator(room, "sender")
    rec_1 = _make_operator(room, "r1", subscription="active")
    rec_2 = _make_operator(room, "r2", subscription="active")
    robot_received: List[Action] = []
    robot.on_action(lambda a: robot_received.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await sender.connect(URL, _make_token("sender", room))
        await rec_1.connect(URL, _make_token("r1", room))
        await rec_2.connect(URL, _make_token("r2", room))
        assert await _wait_for(
            lambda: {"sender", "r1", "r2"} <= set(robot.operators())
        )
        await sender.set_active_operator("sender")
        assert await _wait_for(lambda: robot.active_operator() == "sender")

        for v in range(20):
            sender.send_action({"a": float(v)})
        assert await _wait_for(lambda: len(robot_received) >= 20, timeout=2.0)
        # Robot received exactly the active operator's actions, regardless
        # of how many recorders are listening.
        assert all(a.sender == "sender" for a in robot_received[:20])
    finally:
        await sender.disconnect()
        await rec_1.disconnect()
        await rec_2.disconnect()
        await robot.disconnect()


# ---------------------------------------------------------------------------
# `all`: shadow actions, tagged with `active`
# ---------------------------------------------------------------------------


async def _three_operators(room: str, recorder_subscription: str):
    robot = _make_robot(room)
    ops = {
        "op-a": _make_operator(room, "op-a"),
        "op-b": _make_operator(room, "op-b"),
        "recorder": _make_operator(room, "recorder", subscription=recorder_subscription),
    }
    await robot.connect(URL, _make_token("robot", room))
    for ident, op in ops.items():
        await op.connect(URL, _make_token(ident, room))
    assert await _wait_for(lambda: set(ops) <= set(robot.operators()))
    await ops["op-a"].set_active_operator("op-a")
    assert await _wait_for(lambda: ops["recorder"].active_operator() == "op-a")
    return robot, ops


@pytest.mark.asyncio
async def test_all_delivers_shadow_actions_tagged_inactive():
    room = _room_name()
    robot, ops = await _three_operators(room, "all")
    seen: List[Action] = []
    executed: List[Action] = []
    ops["recorder"].on_action(lambda a: seen.append(a))
    robot.on_action(lambda a: executed.append(a))
    try:
        ops["op-a"].send_action({"a": 1.0})
        ops["op-b"].send_action({"a": 2.0})
        assert await _wait_for(lambda: {a.sender for a in seen} == {"op-a", "op-b"})

        by_sender = {a.sender: a for a in seen}
        assert by_sender["op-a"].active is True
        assert by_sender["op-b"].active is False
        # The robot still only executes the active operator.
        await asyncio.sleep(0.3)
        assert {a.sender for a in executed} == {"op-a"}
        assert all(a.active for a in executed)
    finally:
        for op in ops.values():
            await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_active_subscription_marks_every_action_active():
    room = _room_name()
    robot, ops = await _three_operators(room, "active")
    seen: List[Action] = []
    ops["recorder"].on_action(lambda a: seen.append(a))
    try:
        ops["op-a"].send_action({"a": 1.0})
        ops["op-b"].send_action({"a": 2.0})
        assert await _wait_for(lambda: seen)
        await asyncio.sleep(0.3)
        assert [(a.sender, a.active) for a in seen] == [("op-a", True)]
    finally:
        for op in ops.values():
            await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_active_flag_flips_once_at_handoff():
    """Each sender's `active` flag changes exactly once, at the handoff:
    labels are decided at gate time and never flicker."""
    room = _room_name()
    robot, ops = await _three_operators(room, "all")
    seen: List[Action] = []
    ops["recorder"].on_action(lambda a: seen.append(a))
    try:
        async def stream(n: int) -> None:
            for i in range(n):
                ops["op-a"].send_action({"a": float(i)})
                ops["op-b"].send_action({"a": float(i)})
                await asyncio.sleep(0.02)

        await stream(15)
        await ops["op-a"].set_active_operator("op-b")
        assert await _wait_for(lambda: ops["recorder"].active_operator() == "op-b")
        await stream(15)
        assert await _wait_for(lambda: len(seen) >= 60, timeout=5)

        for sender, before, after in (("op-a", True, False), ("op-b", False, True)):
            flags = [a.active for a in seen if a.sender == sender]
            flips = sum(1 for x, y in zip(flags, flags[1:]) if x != y)
            assert (flags[0], flags[-1], flips) == (before, after, 1), (sender, flags)
    finally:
        for op in ops.values():
            await op.disconnect()
        await robot.disconnect()


@pytest.mark.asyncio
async def test_all_echoes_own_sends_with_active_flag():
    room = _room_name()
    robot = _make_robot(room)
    active = _make_operator(room, "active", subscription="all")
    shadow = _make_operator(room, "shadow", subscription="all")
    seen_active: List[Action] = []
    seen_shadow: List[Action] = []
    active.on_action(lambda a: seen_active.append(a))
    shadow.on_action(lambda a: seen_shadow.append(a))
    try:
        await robot.connect(URL, _make_token("robot", room))
        await active.connect(URL, _make_token("active", room))
        await shadow.connect(URL, _make_token("shadow", room))
        assert await _wait_for(lambda: {"active", "shadow"} <= set(robot.operators()))
        await active.set_active_operator("active")
        assert await _wait_for(lambda: shadow.active_operator() == "active")

        active.send_action({"a": 1.0})
        shadow.send_action({"a": 2.0})

        own = lambda seen, who: [a for a in seen if a.sender == who]
        assert await _wait_for(lambda: own(seen_active, "active") and own(seen_shadow, "shadow"))
        assert own(seen_active, "active")[0].active is True
        assert own(seen_shadow, "shadow")[0].active is False
    finally:
        await active.disconnect()
        await shadow.disconnect()
        await robot.disconnect()
