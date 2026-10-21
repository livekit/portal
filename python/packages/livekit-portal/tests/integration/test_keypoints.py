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

"""Keypoints: `{type, payload}` from any role, to every role, on one clock."""
from __future__ import annotations

import asyncio
import json

import pytest

from livekit.portal import (
    DType,
    Keypoint,
    Observer,
    ObserverConfig,
    Operator,
    OperatorConfig,
    PortalError,
)

from .conftest import URL, RawPeer, _make_token, wait_for

pytestmark = pytest.mark.asyncio

KEYPOINT_TOPIC = "portal_keypoint"
SKEW_US = 5_000_000


def _peer_cfg(cls, room: str):
    cfg = cls(room)
    cfg.add_state_typed([("j", DType.F32)])
    return cfg


async def _room(pair):
    """Robot, operator, a second operator and an observer, each recording
    the keypoints it hears."""
    await pair.start()
    op2 = Operator(_peer_cfg(OperatorConfig, pair.room))
    obs = Observer(_peer_cfg(ObserverConfig, pair.room))
    await op2.connect(URL, _make_token("op2", pair.room))
    await obs.connect(URL, _make_token("observer", pair.room))
    peers = {"robot": pair.robot, "operator": pair.operator, "op2": op2, "observer": obs}
    heard: dict[str, list[Keypoint]] = {name: [] for name in peers}
    for name, peer in peers.items():
        peer.on_keypoint(lambda kp, name=name: heard[name].append(kp))
    assert await wait_for(lambda: pair.robot.observers() == ["observer"])
    assert await wait_for(lambda: {"operator", "op2"} <= set(pair.robot.operators()))
    assert await wait_for(lambda: "observer" in pair.operator.observers())
    return peers, heard


async def test_every_role_hears_the_same_keypoint(pair):
    peers, heard = await _room(pair)
    try:
        present = await pair.operator.send_keypoint(
            "recording", {"task_description": "pick the blue cube"}, timestamp_us=1234
        )
        assert present == ["observer"]
        assert await wait_for(lambda: all(heard.values()))
        expected = Keypoint("recording", {"task_description": "pick the blue cube"}, 1234, "operator")
        for name, kps in heard.items():
            assert kps == [expected], name
    finally:
        for name in ("op2", "observer"):
            await peers[name].disconnect()


async def test_robot_and_observer_can_send(pair):
    peers, heard = await _room(pair)
    try:
        await pair.robot.send_keypoint("subtask", {"subtask_description": "grasp"})
        await peers["observer"].send_keypoint("idle")
        assert await wait_for(lambda: len(heard["op2"]) == 2)
        by_type = {kp.type: kp for kp in heard["op2"]}
        assert by_type["subtask"].sender == "robot"
        assert (by_type["idle"].sender, by_type["idle"].payload) == ("observer", {})
    finally:
        for name in ("op2", "observer"):
            await peers[name].disconnect()


async def test_default_timestamp_is_on_the_robot_clock(pair):
    pair.robot_cfg._set_clock_skew_us(SKEW_US)
    heard = []
    await pair.start()
    pair.robot.on_keypoint(lambda kp: heard.append(kp))
    assert await wait_for(lambda: pair.operator.metrics().time_sync.synced, timeout_s=3)

    await pair.operator.send_keypoint("recording", {"task_description": "t"})
    robot_now = pair.robot.now_us()

    assert await wait_for(lambda: heard)
    assert abs(robot_now - heard[0].timestamp_us) < 50_000


async def test_no_observers_returns_empty(pair):
    await pair.start()
    assert await pair.operator.send_keypoint("idle") == []


async def test_burst_arrives_in_order(pair):
    heard = []
    await pair.start()
    pair.robot.on_keypoint(lambda kp: heard.append(kp.payload["i"]))
    for i in range(200):
        await pair.operator.send_keypoint("tick", {"i": i})
    assert await wait_for(lambda: len(heard) == 200, timeout_s=10)
    assert heard == list(range(200))


async def test_rich_payloads_roundtrip(pair):
    heard = []
    await pair.start()
    pair.robot.on_keypoint(lambda kp: heard.append(kp.payload))
    payload = {"text": "ünïcödé 🤖", "nested": {"xs": [1, 2.5, None], "ok": True}}
    await pair.operator.send_keypoint("note", payload)
    assert await wait_for(lambda: heard)
    assert heard[0] == payload


async def test_oversize_payload_fails_loudly(pair):
    await pair.start()
    with pytest.raises(PortalError.Room, match="exceeds the negotiated maximum message size"):
        await pair.operator.send_keypoint("blob", {"data": "x" * 200_000})


async def test_strangers_and_garbage_are_ignored(pair):
    heard = []
    await pair.start()
    pair.robot.on_keypoint(lambda kp: heard.append(kp.sender))
    stranger = RawPeer(pair.room, "stranger")
    peer = RawPeer(
        pair.room, "raw-op", attributes={"lk.portal.role": "operator", "lk.portal.version": "3"}
    )
    try:
        await stranger.connect()
        await peer.connect()
        assert await wait_for(lambda: "raw-op" in pair.robot.operators())
        good = json.dumps({"type": "idle", "payload": {}, "timestamp_us": 1}).encode()

        await stranger.publish(KEYPOINT_TOPIC, good)
        await peer.publish(KEYPOINT_TOPIC, b"not json")
        await peer.publish(KEYPOINT_TOPIC, good)

        assert await wait_for(lambda: heard)
        await asyncio.sleep(0.3)
        assert heard == ["raw-op"]
    finally:
        await stranger.disconnect()
        await peer.disconnect()
