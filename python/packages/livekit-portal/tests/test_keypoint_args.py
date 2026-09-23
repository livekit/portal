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

"""Keypoint arguments are checked before anything is sent."""
from __future__ import annotations

import pytest

from livekit.portal import Operator, OperatorConfig


@pytest.mark.asyncio
async def test_invalid_arguments_fail_before_sending():
    op = Operator(OperatorConfig("demo"))
    try:
        with pytest.raises(TypeError, match="type must be a str"):
            await op.send_keypoint(1)  # type: ignore[arg-type]
        with pytest.raises(TypeError, match="payload must be a dict"):
            await op.send_keypoint("idle", ["not", "a", "dict"])  # type: ignore[arg-type]
    finally:
        op.close()
