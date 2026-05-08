"""Integration tests for E2EE shared-key support against a live LiveKit server.

Three scenarios:

  * Matching key on both sides — state and frames arrive, observations form.
  * Mismatched keys — encrypted packets arrive but decrypt as garbage;
    Portal's deserialization rejects them and no observations form.
  * No key on one side — unencrypted packets can't be read by an E2EE-enabled
    peer (or vice versa); no observations form.

Skipped automatically when `LIVEKIT_URL` isn't set (see conftest).
"""
from __future__ import annotations

import asyncio
import os
import time

import numpy as np
import pytest

from livekit.portal import (
    DType,
    Observation,
    Portal,
    PortalConfig,
    Role,
    VideoCodec,
)

pytestmark = pytest.mark.asyncio

KEY_A = os.urandom(32)
KEY_B = os.urandom(32)  # guaranteed different from A (astronomically)

# E2EE adds frame-cryptor setup on track publish/subscribe; give it more
# settle time than plain data-channel tests need.
SETTLE_S = 1.2


def _rgb_frame(seed: int = 0) -> np.ndarray:
    x = np.arange(32, dtype=np.int32)
    y = np.arange(32, dtype=np.int32)[:, None]
    r = np.broadcast_to((x + seed) % 256, (32, 32)).astype(np.uint8)
    g = np.broadcast_to((y + seed) % 256, (32, 32)).astype(np.uint8)
    b = np.broadcast_to((x + y + seed) % 256, (32, 32)).astype(np.uint8)
    return np.stack([r, g, b], axis=-1)


_URL = os.environ.get("LIVEKIT_URL")
_API_KEY = os.environ.get("LIVEKIT_API_KEY", "devkey")
_API_SECRET = os.environ.get("LIVEKIT_API_SECRET", "secret")


def _e2ee_pair(key: bytes | None, operator_key: bytes | None = "same") -> tuple:
    """Return (robot_cfg, operator_cfg, url, robot_tok, op_tok) for a fresh room.
    `operator_key` defaults to `"same"` meaning use the same key as robot.
    Pass an explicit bytes value (or None) to override."""
    import datetime
    from livekit import api

    URL, API_KEY, API_SECRET = _URL, _API_KEY, _API_SECRET

    room = f"e2ee-{int(time.time() * 1000)}-{os.urandom(2).hex()}"
    op_key = key if operator_key == "same" else operator_key

    robot_cfg = PortalConfig(room, Role.ROBOT)
    operator_cfg = PortalConfig(room, Role.OPERATOR)
    for cfg in (robot_cfg, operator_cfg):
        cfg.add_video("cam", codec=VideoCodec.PNG)
        cfg.add_state_typed([("j", DType.F32)])
        cfg.add_action_typed([("j", DType.F32)])

    if key is not None:
        robot_cfg.set_e2ee_key(key)
    if op_key is not None:
        operator_cfg.set_e2ee_key(op_key)

    def _token(identity: str) -> str:
        grants = api.VideoGrants(
            room_join=True,
            room=room,
            can_publish=True,
            can_subscribe=True,
            # Portal self-sets the `lk.portal.role` attribute on connect.
            can_update_own_metadata=True,
        )
        return (
            api.AccessToken(API_KEY, API_SECRET)
            .with_identity(identity)
            .with_grants(grants)
            .with_ttl(datetime.timedelta(hours=1))
            .to_jwt()
        )

    return robot_cfg, operator_cfg, URL, _token("robot"), _token("operator")


# ---------------------------------------------------------------------------
# Matching key: data must flow end-to-end
# ---------------------------------------------------------------------------


async def test_e2ee_matching_key_observations_arrive():
    """Robot and operator share the same key. State + frame should pair
    into at least one observation on the operator side."""
    robot_cfg, operator_cfg, url, robot_tok, op_tok = _e2ee_pair(KEY_A)

    robot = Portal(robot_cfg)
    operator = Portal(operator_cfg)
    obs: list[Observation] = []
    operator.on_observation(lambda o: obs.append(o))

    try:
        await robot.connect(url, robot_tok)
        await asyncio.sleep(0.2)
        await operator.connect(url, op_tok)
        await asyncio.sleep(0.3)

        ts = int(time.time() * 1_000_000)
        robot.send_video_frame("cam", _rgb_frame(seed=1), timestamp_us=ts)
        robot.send_state({"j": 0.5}, timestamp_us=ts)
        await asyncio.sleep(SETTLE_S)

        assert len(obs) >= 1, (
            "expected at least one observation with matching E2EE key; "
            f"got {len(obs)}"
        )
        assert "cam" in obs[-1].frames
        assert abs(obs[-1].state["j"] - 0.5) < 1e-3
    finally:
        await operator.disconnect()
        await robot.disconnect()


# ---------------------------------------------------------------------------
# Mismatched keys: data must NOT form valid observations
# ---------------------------------------------------------------------------


async def test_e2ee_mismatched_keys_no_observations():
    """Robot uses KEY_A, operator uses KEY_B. Packets arrive at the
    operator but decrypt as garbage; no valid observations should form."""
    robot_cfg, operator_cfg, url, robot_tok, op_tok = _e2ee_pair(
        KEY_A, operator_key=KEY_B
    )

    robot = Portal(robot_cfg)
    operator = Portal(operator_cfg)
    obs: list[Observation] = []
    operator.on_observation(lambda o: obs.append(o))

    try:
        await robot.connect(url, robot_tok)
        await asyncio.sleep(0.2)
        await operator.connect(url, op_tok)
        await asyncio.sleep(0.3)

        ts = int(time.time() * 1_000_000)
        for _ in range(5):
            robot.send_video_frame("cam", _rgb_frame(), timestamp_us=ts)
            robot.send_state({"j": 0.5}, timestamp_us=ts)
            ts += 33_333

        await asyncio.sleep(SETTLE_S)

        assert len(obs) == 0, (
            f"expected no observations with mismatched E2EE keys; got {len(obs)}"
        )
    finally:
        await operator.disconnect()
        await robot.disconnect()


# ---------------------------------------------------------------------------
# One side unencrypted: data must NOT form valid observations
# ---------------------------------------------------------------------------


async def test_e2ee_only_robot_encrypted_no_observations():
    """Robot sets a key, operator does not. Robot's outbound packets are
    E2EE-encrypted; the operator has no key to decrypt them. No valid
    observations should arrive."""
    robot_cfg, operator_cfg, url, robot_tok, op_tok = _e2ee_pair(
        KEY_A, operator_key=None
    )

    robot = Portal(robot_cfg)
    operator = Portal(operator_cfg)
    obs: list[Observation] = []
    operator.on_observation(lambda o: obs.append(o))

    try:
        await robot.connect(url, robot_tok)
        await asyncio.sleep(0.2)
        await operator.connect(url, op_tok)
        await asyncio.sleep(0.3)

        ts = int(time.time() * 1_000_000)
        for _ in range(5):
            robot.send_video_frame("cam", _rgb_frame(), timestamp_us=ts)
            robot.send_state({"j": 0.5}, timestamp_us=ts)
            ts += 33_333

        await asyncio.sleep(SETTLE_S)

        assert len(obs) == 0, (
            f"expected no observations when only robot is encrypted; got {len(obs)}"
        )
    finally:
        await operator.disconnect()
        await robot.disconnect()
