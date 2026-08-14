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

"""Frame input/output helpers.

Send side (`normalize_rgb`): the Rust FFI takes packed RGB24 (W*H*3 bytes).
We accept either raw `bytes` / `bytearray` / memoryview, or a numpy array of
shape `(H, W, 3)` dtype uint8 and infer W/H from the array.

Receive side: `VideoFrameData.data` carries packed RGB24 (R,G,B byte order)
regardless of transport. WebRTC frames are color-converted from I420 by the
Rust core before delivery, frame-video frames are codec-decoded back to RGB.
`frame_bytes_to_numpy_rgb` turns the bytes into a typed `(H, W, 3)` view.
"""
from __future__ import annotations

from typing import Optional, Tuple, Union

import numpy as np

FrameLike = Union[bytes, bytearray, memoryview, np.ndarray]


def normalize_rgb(
    frame: FrameLike,
    width: Optional[int],
    height: Optional[int],
) -> Tuple[bytes, int, int]:
    if isinstance(frame, np.ndarray):
        if frame.dtype != np.uint8 or frame.ndim != 3 or frame.shape[2] != 3:
            raise ValueError(
                f"numpy RGB frame must be uint8 with shape (H, W, 3); got dtype={frame.dtype}, "
                f"shape={frame.shape}"
            )
        h, w, _ = frame.shape
        if width is not None and width != w:
            raise ValueError(f"width mismatch: array is {w}, argument is {width}")
        if height is not None and height != h:
            raise ValueError(f"height mismatch: array is {h}, argument is {height}")
        return frame.tobytes(), w, h

    if isinstance(frame, (bytes, bytearray, memoryview)):
        if width is None or height is None:
            raise ValueError("width and height are required when frame is raw bytes")
        expected = width * height * 3
        if len(frame) != expected:
            raise ValueError(
                f"raw RGB frame size mismatch: expected {expected} bytes "
                f"(W*H*3 = {width}*{height}*3), got {len(frame)}"
            )
        return frame if isinstance(frame, bytes) else bytes(frame), width, height

    raise TypeError(
        f"unsupported frame type: {type(frame).__name__}. Pass bytes or np.ndarray (H,W,3) uint8."
    )


def frame_bytes_to_numpy_rgb(data: bytes, width: int, height: int) -> np.ndarray:
    """Wrap a `VideoFrameData.data` byte string as a `(H, W, 3)` uint8 RGB array.

    Zero-copy: the returned array views the input bytes directly. If you plan
    to mutate the array, copy it first (`arr.copy()`).
    """
    expected = width * height * 3
    if len(data) != expected:
        raise ValueError(
            f"RGB frame size mismatch: expected {expected} bytes "
            f"(W*H*3 = {width}*{height}*3), got {len(data)}"
        )
    return np.frombuffer(data, dtype=np.uint8).reshape(height, width, 3)
