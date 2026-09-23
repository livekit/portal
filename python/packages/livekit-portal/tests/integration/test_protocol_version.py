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

"""Peers only recognise each other when their `lk.portal.version` matches.

A `RawPeer` claims the operator role with a missing or foreign version. It
replays a real operator's action packet so the only thing separating it from
a legitimate operator is the version attribute.
"""
from __future__ import annotations

import asyncio

import pytest

from livekit.portal import DType

from .conftest import RawPeer, wait_for

pytestmark = pytest.mark.asyncio

ACTION_TOPIC = "portal_action"


async def _captured_action(pair, raw: RawPeer) -> bytes:
    """A genuine action payload, sent by the real operator and overheard."""
    ok = await wait_for(lambda: raw.payloads(ACTION_TOPIC, "operator"), timeout_s=5)
    if not ok:
        pair.operator.send_action({"a": 1.0})
        await wait_for(lambda: raw.payloads(ACTION_TOPIC, "operator"), timeout_s=5)
    return raw.payloads(ACTION_TOPIC, "operator")[0]


@pytest.mark.parametrize("version", [None, "2"], ids=["missing", "foreign"])
async def test_mismatched_peer_is_ignored(pair, version):
    pair.robot_cfg.add_action_typed([("a", DType.F32)])
    pair.operator_cfg.add_action_typed([("a", DType.F32)])
    attrs = {"lk.portal.role": "operator"}
    if version is not None:
        attrs["lk.portal.version"] = version
    raw = RawPeer(pair.room, "old-operator", attributes=attrs)

    received = []
    try:
        await pair.start()
        pair.robot.on_action(lambda a: received.append(a.sender))
        await raw.connect()

        pair.operator.send_action({"a": 1.0})
        payload = await _captured_action(pair, raw)
        assert await wait_for(lambda: "operator" in received)

        # Point the gate at the raw peer and let it replay a valid packet.
        await pair.robot.set_active_operator("old-operator")
        received.clear()
        for _ in range(10):
            await raw.publish(ACTION_TOPIC, payload)
            await asyncio.sleep(0.05)
        await asyncio.sleep(0.5)

        assert received == []
        assert "old-operator" not in pair.robot.operators()
        assert pair.robot.operators() == ["operator"]
    finally:
        await raw.disconnect()


async def test_matching_peer_is_accepted(pair):
    pair.robot_cfg.add_action_typed([("a", DType.F32)])
    pair.operator_cfg.add_action_typed([("a", DType.F32)])
    raw = RawPeer(
        pair.room,
        "raw-operator",
        attributes={"lk.portal.role": "operator", "lk.portal.version": "3"},
    )

    received = []
    try:
        await pair.start()
        pair.robot.on_action(lambda a: received.append(a.sender))
        await raw.connect()

        pair.operator.send_action({"a": 1.0})
        payload = await _captured_action(pair, raw)
        assert await wait_for(lambda: "raw-operator" in pair.robot.operators())

        await pair.robot.set_active_operator("raw-operator")
        received.clear()
        await raw.publish(ACTION_TOPIC, payload)

        assert await wait_for(lambda: "raw-operator" in received, timeout_s=5)
    finally:
        await raw.disconnect()
