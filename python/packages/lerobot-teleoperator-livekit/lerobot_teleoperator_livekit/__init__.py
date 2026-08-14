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

"""LiveKit Portal teleoperator plugin for lerobot.

Deployed on the **robot side**. Wraps a `livekit.portal.Robot` so lerobot
can drive a remote physical robot by running a teleop loop that pushes
actions over LiveKit. Importing this module registers
``LiveKitTeleoperator`` as ``--teleop.type=livekit``.
"""
from __future__ import annotations

from .teleoperator import LiveKitTeleoperator, LiveKitTeleoperatorConfig

__all__ = ["LiveKitTeleoperator", "LiveKitTeleoperatorConfig"]
