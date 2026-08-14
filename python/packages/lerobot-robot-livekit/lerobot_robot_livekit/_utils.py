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

from __future__ import annotations


def split_observation_features(
    features: dict,
) -> tuple[list[str], dict[str, tuple[int, ...]]]:
    """Split a lerobot observation_features dict into motor keys and cameras.

    Scalar-valued entries are motor keys; tuple-valued entries are camera
    names mapped to their shape. Returns ``(sorted_motor_keys, cameras)``.
    """
    motor_keys: list[str] = []
    cameras: dict[str, tuple[int, ...]] = {}
    for key, val in features.items():
        if isinstance(val, tuple):
            cameras[key] = val
        else:
            motor_keys.append(key)
    return sorted(motor_keys), cameras
