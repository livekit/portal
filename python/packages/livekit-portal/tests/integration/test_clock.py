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

"""Time sync against a real server.

Every peer runs on this host, so the robot's clock is skewed with the
`_set_clock_skew_us` test hook to give the operator something to find.
"""
from __future__ import annotations

import asyncio
import struct

import pytest

from livekit.portal import Operator, OperatorConfig, Robot, RobotConfig

from .conftest import URL, RawPeer, _make_token, wait_for

pytestmark = pytest.mark.asyncio

SKEW_US = 5_000_000
CLOCK_TOPIC = "portal_clock"


def _offset_error_us(robot: Robot, operator: Operator) -> int:
    # Sample the operator between two robot reads so the robot's clock
    # brackets it; the midpoint removes the time between the calls.
    before = robot.now_us()
    op = operator.now_us()
    after = robot.now_us()
    return abs(op - (before + after) // 2)


async def test_operator_syncs_to_skewed_robot(pair):
    pair.robot_cfg._set_clock_skew_us(SKEW_US)
    synced = asyncio.Event()
    await pair.start()
    pair.operator.on_time_synced(synced.set)

    await asyncio.wait_for(synced.wait(), timeout=3)
    m = pair.operator.metrics().time_sync
    assert m.synced
    assert abs(m.offset_us - SKEW_US) < 50_000
    assert m.uncertainty_us is not None
    assert _offset_error_us(pair.robot, pair.operator) <= m.uncertainty_us + 5_000

    robot_m = pair.robot.metrics().time_sync
    assert robot_m.synced and robot_m.offset_us == 0 and robot_m.uncertainty_us == 0


async def test_rtt_comes_from_time_sync(pair):
    await pair.start()
    assert await wait_for(lambda: pair.operator.metrics().rtt.pongs_received >= 4, timeout_s=5)
    rtt = pair.operator.metrics().rtt
    assert rtt.rtt_us_last is not None and rtt.pings_sent >= rtt.pongs_received
    # The robot answers but never pings.
    assert pair.robot.metrics().rtt.pings_sent == 0


async def test_reset_metrics_keeps_sync_state(pair):
    pair.robot_cfg._set_clock_skew_us(SKEW_US)
    await pair.start()
    assert await wait_for(lambda: pair.operator.metrics().time_sync.synced, timeout_s=3)
    before = pair.operator.metrics().time_sync

    pair.operator.reset_metrics()

    after = pair.operator.metrics().time_sync
    assert after.synced
    assert after.offset_us == before.offset_us
    assert pair.operator.metrics().rtt.pongs_received == 0


async def test_unsynced_before_robot_joins():
    room = "clock-alone"
    operator = Operator(OperatorConfig(f"{room}-{id(object())}"))
    try:
        await operator.connect(URL, _make_token("operator", f"{room}-{id(operator)}"))
        await asyncio.sleep(0.5)
        m = operator.metrics().time_sync
        assert not m.synced and m.uncertainty_us is None
        a, b = operator.now_us(), operator.now_us()
        assert b > a
    finally:
        await operator.disconnect()


async def test_resyncs_after_robot_restarts(pair):
    pair.robot_cfg._set_clock_skew_us(SKEW_US)
    await pair.start()
    assert await wait_for(lambda: pair.operator.metrics().time_sync.synced, timeout_s=3)

    await pair.robot.disconnect()
    cfg = RobotConfig(pair.room)
    cfg._set_clock_skew_us(2 * SKEW_US)
    pair.robot = Robot(cfg)
    await pair.robot.connect(URL, _make_token("robot", pair.room))

    # A forward correction applies at once, so the operator lands on the
    # new robot's clock as soon as the new estimate is in.
    assert await wait_for(
        lambda: abs(pair.operator.metrics().time_sync.offset_us - 2 * SKEW_US) < 50_000,
        timeout_s=5,
    )


async def test_pings_go_only_to_the_robot(pair):
    await pair.start()
    listener = RawPeer(
        pair.room,
        "listener",
        attributes={"lk.portal.role": "operator", "lk.portal.version": "3"},
    )
    try:
        await listener.connect()
        assert await wait_for(lambda: pair.operator.metrics().rtt.pongs_received >= 4, timeout_s=5)
        await asyncio.sleep(1.5)
        # Pings are addressed to the robot and pongs to their pinger, so a
        # third peer hears neither.
        assert listener.payloads(CLOCK_TOPIC) == []
    finally:
        await listener.disconnect()


async def test_robot_ignores_malformed_clock_packets(pair):
    await pair.start()
    raw = RawPeer(pair.room, "noise")
    try:
        await raw.connect()
        for payload in (b"", b"\x00", b"\x07" * 13, struct.pack("<BIQ", 1, 0, 0), b"\xff" * 64):
            await raw.publish(CLOCK_TOPIC, payload, reliable=False)
        await asyncio.sleep(0.5)
        # Still answering the real operator.
        base = pair.operator.metrics().rtt.pongs_received
        assert await wait_for(
            lambda: pair.operator.metrics().rtt.pongs_received > base, timeout_s=3
        )
    finally:
        await raw.disconnect()
