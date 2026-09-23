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

"""`observation_sync` is a local switch; these checks need no server."""
from __future__ import annotations

import pytest

from livekit.portal import Operator, OperatorConfig, PortalError


def test_on_by_default():
    assert OperatorConfig("demo").observation_sync is True


def test_yaml_turns_it_off():
    cfg = OperatorConfig.from_yaml_str("version: 1\nobservation_sync: false\n", "demo")
    assert cfg.observation_sync is False


def test_observation_api_raises_when_off():
    cfg = OperatorConfig("demo")
    cfg.set_observation_sync(False)
    op = Operator(cfg)
    try:
        with pytest.raises(PortalError.ObservationSyncDisabled):
            op.get_observation()
        with pytest.raises(PortalError.ObservationSyncDisabled):
            op.on_observation(lambda o: None)
        # Everything that doesn't need bundling keeps working.
        op.on_state(lambda s: None)
        op.on_drop(lambda d: None)
        assert op.get_state() is None
    finally:
        op.close()


def test_observation_api_works_when_on():
    op = Operator(OperatorConfig("demo"))
    try:
        op.on_observation(lambda o: None)
        assert op.get_observation() is None
    finally:
        op.close()
