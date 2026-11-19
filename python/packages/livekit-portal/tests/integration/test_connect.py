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

"""Connect fails loudly when Portal cannot publish its role attributes."""
import os

import pytest

from livekit.portal import PortalError, Robot, RobotConfig

from .conftest import URL, _make_token


async def test_connect_fails_without_metadata_grant():
    room = f"connect-no-grant-{os.urandom(3).hex()}"
    robot = Robot(RobotConfig(room))
    token = _make_token("robot", room, can_update_own_metadata=False)
    with pytest.raises(PortalError.Room, match="canUpdateOwnMetadata"):
        await robot.connect(URL, token)
