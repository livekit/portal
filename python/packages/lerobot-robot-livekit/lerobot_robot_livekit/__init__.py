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

"""LiveKit Portal robot plugin for lerobot.

Deployed on the **operator side**. Makes a remote physical robot appear as a
local ``Robot`` to any lerobot workflow (teleoperation, data recording,
policy evaluation). Importing this module registers ``LiveKitRobot`` as
``--robot.type=livekit``.
"""
from __future__ import annotations

from .robot import LiveKitRobot, LiveKitRobotConfig

__all__ = ["LiveKitRobot", "LiveKitRobotConfig"]
