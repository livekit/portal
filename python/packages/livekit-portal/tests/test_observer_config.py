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

"""Observer surface and defaults; no server needed."""
from __future__ import annotations

import pytest

from livekit.portal import (
    ActionSubscription,
    Observer,
    ObserverConfig,
    OperatorConfig,
    PortalError,
    Role,
)


def test_observer_defaults():
    cfg = ObserverConfig("demo")
    assert cfg.role == Role.OBSERVER
    assert cfg.action_subscription == ActionSubscription.ACTIVE
    assert cfg.observation_sync is False
    # Operators keep their own defaults.
    op = OperatorConfig("demo")
    assert (op.action_subscription, op.observation_sync) == (ActionSubscription.NONE, True)


def test_observer_defaults_survive_yaml():
    cfg = ObserverConfig.from_yaml_str("version: 1\nfps: 15\n", "demo")
    assert cfg.action_subscription == ActionSubscription.ACTIVE
    assert cfg.observation_sync is False
    cfg = ObserverConfig.from_yaml_str("version: 1\naction_subscription: all\n", "demo")
    assert cfg.action_subscription == ActionSubscription.ALL


def test_observer_cannot_send():
    obs = Observer(ObserverConfig("demo"))
    try:
        for send in (
            lambda: obs.send_action({"a": 1.0}),
            lambda: obs.send_state({"j": 1.0}),
            lambda: obs.send_video_frame("cam", b""),
        ):
            with pytest.raises(PortalError.WrongRole):
                send()
        with pytest.raises(PortalError.ObservationSyncDisabled):
            obs.get_observation()
    finally:
        obs.close()


def test_ffi_observer_has_no_send_methods():
    from livekit.portal import livekit_portal_ffi as ffi

    for name in ("send_action", "send_state", "send_video_frame"):
        assert not hasattr(ffi.Observer, name), name
    for name in ("set_active_operator", "operators", "observers", "on_action" if hasattr(ffi.Observer, "on_action") else "get_action"):
        assert hasattr(ffi.Observer, name), name
