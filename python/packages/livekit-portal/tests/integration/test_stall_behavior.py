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

"""Stalled-track handling against a live LiveKit server.

The unit tests drive `SyncBuffer` directly. These drive the whole path —
publish, SFU, receive, decode, sync — because the thing being fixed is an
operational failure (a camera dies mid-session), and the interesting question
is what the *consumer* sees when it happens.

Timestamps are supplied explicitly so the stream clock is deterministic:
`max_lag` is measured in sender-clock time, so a test that relied on wall-clock
arrival would be racing the network for no reason.

Skipped automatically when `LIVEKIT_URL` isn't set (see conftest).
"""
from __future__ import annotations

import asyncio

import numpy as np
import pytest

from livekit.portal import (
    FrameSource,
    Observation,
    StallBehavior,
    VideoCodec,
)

pytestmark = pytest.mark.asyncio

# Byte-stream receive races a few hops (stream open → read → decode → sync →
# callback hop to the asyncio loop). Generous on localhost.
SETTLE_S = 0.6

# Sender-clock spacing between moments, in microseconds. Comfortably wider
# than the default match window (tolerance/fps = 50ms) so nothing matches by
# accident, and wider than MAX_LAG so one silent step trips the budget.
STEP_US = 100_000
MAX_LAG_MS = 50


def _frame(width: int = 32, height: int = 24, seed: int = 0) -> np.ndarray:
    x = np.arange(width, dtype=np.int32)
    y = np.arange(height, dtype=np.int32)[:, None]
    r = np.broadcast_to(((x + seed) % 256).astype(np.uint8), (height, width))
    g = np.broadcast_to(((y + seed) % 256).astype(np.uint8), (height, width))
    b = ((x + y + seed) % 256).astype(np.uint8)
    return np.stack([r, g, b], axis=-1)


def _declare(pair, policy: StallBehavior | None = None) -> None:
    """Two cameras on the byte-stream path, so frames arrive deterministically
    rather than through libwebrtc's rate-adapting encoder.

    The stall knobs go on the **operator only**. Observations are assembled
    where they are received, so that is the side that reads them; leaving the
    robot at its defaults keeps every test below an implicit check that the
    receiver alone is in charge.
    """
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        cfg.add_video("cam1", codec=VideoCodec.RAW)
        cfg.add_video("cam2", codec=VideoCodec.RAW)
    pair.operator_cfg.set_max_lag_ms(MAX_LAG_MS)
    if policy is not None:
        pair.operator_cfg.set_stall_behavior(policy)


async def _run_cam2_dies(pair, obs: list[Observation], base: int) -> None:
    """Three moments: both cameras healthy, then cam2 goes silent while cam1
    keeps carrying the stream clock forward past the lag budget."""
    pair.robot.send_video_frame("cam1", _frame(seed=1), timestamp_us=base)
    pair.robot.send_video_frame("cam2", _frame(seed=2), timestamp_us=base)
    pair.robot.send_state({"j": 0.0}, timestamp_us=base)
    await asyncio.sleep(SETTLE_S)

    # cam2 is now dead. This moment is still inside its budget, so it waits.
    pair.robot.send_video_frame("cam1", _frame(seed=3), timestamp_us=base + STEP_US)
    pair.robot.send_state({"j": 1.0}, timestamp_us=base + STEP_US)
    await asyncio.sleep(SETTLE_S)

    # cam1 alone carries the clock past the budget, which is the case
    # capacity eviction could never reach — no further state is sent.
    pair.robot.send_video_frame("cam1", _frame(seed=4), timestamp_us=base + 2 * STEP_US)
    await asyncio.sleep(SETTLE_S)


async def test_omit_keeps_healthy_camera_visible(pair):
    """The headline case: one camera dying must not blank the other.

    Under `OMIT` the moment survives with cam1 live and cam2 carrying a
    placeholder — and the `cam2` key is still present, so consumer code
    indexing it does not start raising.
    """
    _declare(pair, StallBehavior.OMIT)
    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    await _run_cam2_dies(pair, obs, base=1_000_000)

    assert len(obs) >= 2, f"expected the stalled moment to resolve, got {len(obs)}"
    last = obs[-1]
    assert set(last.frames) == {"cam1", "cam2"}, "every declared track keeps its key"
    assert last.frames["cam1"].source is FrameSource.LIVE
    assert last.frames["cam2"].source is FrameSource.OMITTED

    # The placeholder inherits the dead camera's geometry and the timestamp of
    # its last real frame, so staleness stays measurable.
    assert last.frames["cam2"].width == 32
    assert last.frames["cam2"].height == 24
    assert last.frames["cam2"].timestamp_us == 1_000_000


async def test_drop_loses_the_healthy_camera_too(pair):
    """The contrast that makes `OMIT` worth having.

    With the default policy the same failure yields no observation at all, so
    the operator loses every camera because one died. Asserting it here keeps
    the two policies honest about how they differ.
    """
    _declare(pair, StallBehavior.DROP)
    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    await _run_cam2_dies(pair, obs, base=2_000_000)

    assert len(obs) == 1, f"only the healthy moment should emit, got {len(obs)}"
    assert obs[0].frames["cam1"].source is FrameSource.LIVE
    assert obs[0].frames["cam2"].source is FrameSource.LIVE


async def test_freeze_substitutes_the_last_good_frame(pair):
    """`FREEZE` keeps the moment too, but with real (older) pixels rather than
    a placeholder — tagged `STALE` so a consumer can tell the difference."""
    _declare(pair, StallBehavior.FREEZE)
    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    await _run_cam2_dies(pair, obs, base=3_000_000)

    assert len(obs) >= 2
    last = obs[-1]
    assert last.frames["cam1"].source is FrameSource.LIVE
    assert last.frames["cam2"].source is FrameSource.STALE
    assert last.frames["cam2"].timestamp_us == 3_000_000, "cam2's last real frame"


async def test_policy_is_per_track_over_the_wire(pair):
    """Per-track policy is the reason the knob exists: a load-bearing camera
    can stay strict while a secondary one degrades gracefully."""
    # Declared inline rather than through the setters: this is the form the
    # Python docs lead with, so it needs to survive the round trip through the
    # FFI and reach the sync buffer's per-track resolution.
    pair.robot_cfg.add_video("cam1", codec=VideoCodec.RAW)
    pair.robot_cfg.add_video("cam2", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video("cam1", codec=VideoCodec.RAW)
    pair.operator_cfg.add_video(
        "cam2", codec=VideoCodec.RAW, stall_behavior=StallBehavior.OMIT
    )
    pair.operator_cfg.set_stall_behavior(StallBehavior.DROP)
    pair.operator_cfg.set_max_lag_ms(MAX_LAG_MS)

    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    await _run_cam2_dies(pair, obs, base=4_000_000)

    assert len(obs) >= 2, "cam2 is the lenient track, so the moment survives"
    assert obs[-1].frames["cam2"].source is FrameSource.OMITTED


async def test_omission_is_counted_per_track(pair):
    """Ops need to know *which* camera is down without diffing key sets."""
    _declare(pair, StallBehavior.OMIT)
    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    await _run_cam2_dies(pair, obs, base=5_000_000)

    omitted = pair.operator.metrics().sync.frames_omitted
    assert omitted.get("cam2", 0) >= 1, f"cam2 omissions not counted: {omitted}"
    assert omitted.get("cam1", 0) == 0, "healthy camera must not be counted"


async def test_recovery_returns_to_live(pair):
    """A camera coming back must go straight to `LIVE`; the placeholder is not
    sticky, or a recovered session would look permanently broken."""
    _declare(pair, StallBehavior.OMIT)
    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    base = 6_000_000
    await _run_cam2_dies(pair, obs, base=base)
    assert obs[-1].frames["cam2"].source is FrameSource.OMITTED

    # cam2 comes back, in range of a fresh moment.
    ts = base + 3 * STEP_US
    pair.robot.send_video_frame("cam1", _frame(seed=5), timestamp_us=ts)
    pair.robot.send_video_frame("cam2", _frame(seed=6), timestamp_us=ts)
    pair.robot.send_state({"j": 2.0}, timestamp_us=ts)
    await asyncio.sleep(SETTLE_S)

    assert obs[-1].frames["cam2"].source is FrameSource.LIVE
    assert obs[-1].frames["cam1"].source is FrameSource.LIVE


async def test_policy_is_read_on_the_receiving_side(pair):
    """Setting the policy on the publisher must do nothing.

    Nothing about a stall crosses the wire: a silent camera is by definition
    sending nothing, so `OMIT` cannot be a message. The substitute is
    synthesized by whoever is assembling observations, which is the operator.
    Configuring only the robot therefore leaves the operator on its default
    `DROP`, and the moment is discarded.

    This is the mirror of `test_omit_keeps_healthy_camera_visible`: identical
    traffic, policy on the other side, opposite outcome.
    """
    for cfg in (pair.robot_cfg, pair.operator_cfg):
        cfg.add_video("cam1", codec=VideoCodec.RAW)
        cfg.add_video("cam2", codec=VideoCodec.RAW)
    # Deliberately the wrong side.
    pair.robot_cfg.set_max_lag_ms(MAX_LAG_MS)
    pair.robot_cfg.set_stall_behavior(StallBehavior.OMIT)

    obs: list[Observation] = []
    await pair.start()
    pair.operator.on_observation(lambda o: obs.append(o))

    await _run_cam2_dies(pair, obs, base=7_000_000)

    assert len(obs) == 1, "the robot-side policy must have no effect"
    assert all(f.source is FrameSource.LIVE for f in obs[0].frames.values())
