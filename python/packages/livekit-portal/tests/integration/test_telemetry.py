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

"""Telemetry: user-defined fields from any role, tagged with the sender and
kept out of observations."""
from __future__ import annotations

import pytest

from livekit.portal import DType, Observer, ObserverConfig, Telemetry

from .conftest import URL, _make_token, wait_for

pytestmark = pytest.mark.asyncio

SCHEMA = [("power/battery_v", DType.F32), ("estop", DType.BOOL)]


def _declare(pair) -> None:
    pair.robot_cfg.add_telemetry_typed(SCHEMA)
    pair.operator_cfg.add_telemetry_typed(SCHEMA)


async def test_robot_telemetry_reaches_the_operator(pair):
    _declare(pair)
    await pair.start()
    heard: list[Telemetry] = []
    pair.operator.on_telemetry(heard.append)

    pair.robot.send_telemetry({"power/battery_v": 24.5, "estop": True})

    assert await wait_for(lambda: heard)
    t = heard[0]
    assert t.sender == "robot"
    assert t.values == {"power/battery_v": 24.5, "estop": True}
    assert pair.operator.get_telemetry("robot") == t
    assert pair.operator.get_telemetry("nobody") is None
    assert pair.operator.get_observation() is None
    assert pair.operator.metrics().transport.telemetry_received >= 1


async def test_unsent_fields_carry_forward(pair):
    _declare(pair)
    await pair.start()
    heard: list[Telemetry] = []
    pair.operator.on_telemetry(heard.append)

    pair.robot.send_telemetry({"estop": True})
    assert await wait_for(lambda: heard)
    pair.robot.send_telemetry({"power/battery_v": 12.0})
    assert await wait_for(lambda: len(heard) >= 2)

    assert heard[-1].values == {"power/battery_v": 12.0, "estop": True}


async def test_operator_and_observer_can_send(pair):
    _declare(pair)
    await pair.start()
    obs_cfg = ObserverConfig(pair.room)
    obs_cfg.add_telemetry_typed(SCHEMA)
    obs = Observer(obs_cfg)
    await obs.connect(URL, _make_token("observer", pair.room))
    try:
        heard: list[Telemetry] = []
        pair.robot.on_telemetry(heard.append)
        assert await wait_for(lambda: pair.robot.observers() == ["observer"])

        pair.operator.send_telemetry({"power/battery_v": 1.0})
        obs.send_telemetry({"power/battery_v": 2.0})

        assert await wait_for(lambda: {t.sender for t in heard} == {"operator", "observer"})
        assert pair.robot.get_telemetry("observer").values["power/battery_v"] == 2.0
    finally:
        await obs.disconnect()
