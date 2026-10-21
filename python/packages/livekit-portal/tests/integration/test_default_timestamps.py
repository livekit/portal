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

"""Sends default to `now_us()`, which is on the robot's clock.

The robot's clock is skewed by `SKEW_US`, so an operator that stamped with
its own clock would land seconds away from where the robot expects.
"""
from __future__ import annotations

import asyncio
import time

import pytest

from livekit.portal import DType, Operator, OperatorConfig, TimeSyncSource

from .conftest import URL, _make_token, wait_for

pytestmark = pytest.mark.asyncio

SKEW_US = 5_000_000


def _schemas(pair) -> None:
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        cfg.add_action_typed([("a", DType.F32)])
    pair.robot_cfg._set_clock_skew_us(SKEW_US)


async def _synced(pair) -> None:
    assert await wait_for(lambda: pair.operator.metrics().time_sync.synced, timeout_s=3)


async def test_default_action_timestamp_is_on_robot_clock(pair):
    _schemas(pair)
    received = []
    await pair.start()
    pair.robot.on_action(lambda a: received.append((a.timestamp_us, pair.robot.now_us())))
    await _synced(pair)

    pair.operator.send_action({"a": 1.0})

    assert await wait_for(lambda: received)
    sent_ts, robot_now = received[0]
    assert 0 <= robot_now - sent_ts < 50_000


async def test_explicit_timestamps_pass_through(pair):
    _schemas(pair)
    received = []
    await pair.start()
    pair.robot.on_action(lambda a: received.append(a))
    await _synced(pair)

    pair.operator.send_action({"a": 1.0}, timestamp_us=123_456, in_reply_to_ts_us=654_321)

    assert await wait_for(lambda: received)
    assert (received[0].timestamp_us, received[0].in_reply_to_ts_us) == (123_456, 654_321)


async def test_in_reply_to_is_the_exact_state_timestamp(pair):
    _schemas(pair)
    states, actions = [], []
    await pair.start()
    pair.operator.on_state(lambda s: states.append(s.timestamp_us))
    pair.robot.on_action(lambda a: actions.append(a.in_reply_to_ts_us))
    await _synced(pair)

    pair.robot.send_state({"j": 0.5})
    assert await wait_for(lambda: states)
    pair.operator.send_action({"a": 1.0}, in_reply_to_ts_us=states[0])

    assert await wait_for(lambda: actions)
    assert actions[0] == states[0]


async def test_default_state_timestamps_strictly_increase(pair):
    _schemas(pair)
    states = []
    await pair.start()
    pair.operator.on_state(lambda s: states.append(s.timestamp_us))

    for _ in range(50):
        pair.robot.send_state({"j": 0.5})
    robot_now = pair.robot.now_us()

    assert await wait_for(lambda: len(states) == 50)
    assert all(b > a for a, b in zip(states, states[1:]))
    assert 0 <= robot_now - states[-1] < 50_000


async def test_system_source_measures_but_does_not_apply(pair):
    _schemas(pair)
    pair.operator_cfg.set_time_sync_source(TimeSyncSource.SYSTEM)
    await pair.start()

    m = pair.operator.metrics().time_sync
    assert m.source == TimeSyncSource.SYSTEM
    assert m.synced and m.offset_us == 0

    assert await wait_for(
        lambda: pair.operator.metrics().time_sync.measured_offset_us is not None, timeout_s=3
    )
    measured = pair.operator.metrics().time_sync.measured_offset_us
    assert abs(measured - SKEW_US) < 50_000
    assert abs(pair.operator.now_us() - time.time_ns() // 1_000) < 50_000


async def test_portal_and_system_peers_share_a_room(pair):
    _schemas(pair)
    received = []
    await pair.start()
    pair.robot.on_action(lambda a: received.append(a.sender))

    cfg = OperatorConfig(pair.room)
    cfg.add_state_typed([("j", DType.F32)])
    cfg.add_action_typed([("a", DType.F32)])
    cfg.set_time_sync_source(TimeSyncSource.SYSTEM)
    lab = Operator(cfg)
    try:
        await lab.connect(URL, _make_token("lab", pair.room))
        assert await wait_for(lambda: "lab" in pair.robot.operators())

        pair.operator.send_action({"a": 1.0})
        assert await wait_for(lambda: "operator" in received)
        await lab.set_active_operator("lab")
        await asyncio.sleep(0.2)
        lab.send_action({"a": 2.0})
        assert await wait_for(lambda: "lab" in received)
    finally:
        await lab.disconnect()
