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

"""The observer role: sees everything, never drives, may hand control."""
from __future__ import annotations

import asyncio

import numpy as np
import pytest

from livekit.portal import DType, Observer, ObserverConfig, Operator, OperatorConfig, VideoCodec

from .conftest import URL, RawPeer, _make_token, wait_for

pytestmark = pytest.mark.asyncio


def _declare(cfg) -> None:
    cfg.add_action_typed([("a", DType.F32)])
    cfg.add_video("cam", codec=VideoCodec.RAW)


def _observer(room: str) -> Observer:
    cfg = ObserverConfig(room)
    cfg.add_state_typed([("j", DType.F32)])
    _declare(cfg)
    return Observer(cfg)


async def _with_observer(pair, identity: str = "observer") -> Observer:
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    await pair.start()
    obs = _observer(pair.room)
    await obs.connect(URL, _make_token(identity, pair.room))
    assert await wait_for(lambda: identity in pair.robot.observers())
    return obs


async def test_observers_are_listed_apart_from_operators(pair):
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    joined = []
    await pair.start()
    pair.robot.on_operator_joined(lambda i: joined.append(i))
    obs = _observer(pair.room)
    try:
        await obs.connect(URL, _make_token("observer", pair.room))
        assert await wait_for(lambda: pair.robot.observers() == ["observer"])
        assert await wait_for(lambda: "observer" in pair.operator.observers())
        assert pair.robot.operators() == ["operator"]
        assert obs.operators() == ["operator"]
        assert obs.robot_identity() == "robot"
        await asyncio.sleep(0.2)
        assert joined == []
    finally:
        await obs.disconnect()
    assert await wait_for(lambda: pair.robot.observers() == [])


async def test_observer_sees_state_frames_and_executed_actions(pair):
    obs = await _with_observer(pair)
    states, frames, actions = [], [], []
    obs.on_state(lambda s: states.append(s))
    obs.on_video_frame("cam", lambda n, f: frames.append(f))
    obs.on_action(lambda a: actions.append(a))
    try:
        assert await wait_for(lambda: obs.active_operator() == "operator")
        pair.robot.send_state({"j": 0.5})
        pair.robot.send_video_frame("cam", np.zeros((24, 32, 3), dtype=np.uint8))
        pair.operator.send_action({"a": 1.0})

        assert await wait_for(lambda: states and frames and actions)
        assert (actions[0].sender, actions[0].active) == ("operator", True)
        assert obs.metrics().sync.observations_emitted == 0
    finally:
        await obs.disconnect()


async def test_observer_hands_control(pair):
    obs = await _with_observer(pair)
    executed = []
    pair.robot.on_action(lambda a: executed.append(a.sender))
    try:
        await obs.set_active_operator(None)
        assert await wait_for(lambda: pair.robot.active_operator() is None)
        pair.operator.send_action({"a": 1.0})
        await asyncio.sleep(0.4)
        assert executed == []

        await obs.set_active_operator("operator")
        assert await wait_for(lambda: pair.robot.active_operator() == "operator")
        pair.operator.send_action({"a": 2.0})
        assert await wait_for(lambda: executed == ["operator"])
    finally:
        await obs.disconnect()


async def test_observer_can_steer_right_after_connecting(pair):
    """The observer's RPC can reach the robot before its role attribute; the
    robot waits for the roster instead of refusing."""
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    await pair.start()
    obs = _observer(pair.room)
    try:
        await obs.connect(URL, _make_token("observer", pair.room))
        await obs.set_active_operator(None)
        assert await wait_for(lambda: pair.robot.active_operator() is None)
    finally:
        await obs.disconnect()


async def test_robot_refuses_steering_from_unknown_role(pair):
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        _declare(cfg)
    await pair.start()
    raw = RawPeer(pair.room, "stranger")
    try:
        await raw.connect()
        with pytest.raises(Exception, match="only operators and observers"):
            await raw.room.local_participant.perform_rpc(
                destination_identity="robot",
                method="portal.set_active_operator",
                payload="stranger",
                response_timeout=5.0,
            )
        assert pair.robot.active_operator() == "operator"
    finally:
        await raw.disconnect()


async def test_two_observers_record_and_orchestrate(pair):
    recorder = await _with_observer(pair, "recorder")
    orchestrator = _observer(pair.room)
    seen = []
    recorder.on_action(lambda a: seen.append((a.sender, a.active)))
    try:
        await orchestrator.connect(URL, _make_token("orchestrator", pair.room))
        assert await wait_for(lambda: set(pair.robot.observers()) == {"recorder", "orchestrator"})

        cfg = OperatorConfig(pair.room)
        cfg.add_state_typed([("j", DType.F32)])
        _declare(cfg)
        policy = Operator(cfg)
        await policy.connect(URL, _make_token("policy", pair.room))
        assert await wait_for(lambda: "policy" in pair.robot.operators())

        await orchestrator.set_active_operator("policy")
        assert await wait_for(lambda: recorder.active_operator() == "policy")
        policy.send_action({"a": 1.0})
        pair.operator.send_action({"a": 2.0})
        assert await wait_for(lambda: ("policy", True) in seen)
        await asyncio.sleep(0.3)
        assert ("operator", True) not in seen, "active subscription skips shadow actions"
        await policy.disconnect()
    finally:
        await orchestrator.disconnect()
        await recorder.disconnect()


async def test_observer_reconnect_updates_rosters(pair):
    obs = await _with_observer(pair)
    await obs.disconnect()
    assert await wait_for(lambda: pair.robot.observers() == [])
    obs = _observer(pair.room)
    try:
        await obs.connect(URL, _make_token("observer", pair.room))
        assert await wait_for(lambda: pair.robot.observers() == ["observer"])
    finally:
        await obs.disconnect()
