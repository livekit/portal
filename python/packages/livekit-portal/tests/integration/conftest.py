"""Integration tests against a real LiveKit server.

Skipped automatically when `LIVEKIT_URL` is not in the environment, so a
plain `pytest` run still works without a server. The minimal invocation
for a local dev server is:

    LIVEKIT_URL=ws://localhost:7880 pytest tests/integration

`LIVEKIT_API_KEY` / `LIVEKIT_API_SECRET` default to `devkey` / `secret`
(LiveKit's stock dev creds). Override both for a non-local server.

Each test gets a fresh room name so parallel runs don't collide and each
scenario starts from a clean slate.
"""
from __future__ import annotations

import datetime
import os
import time
from typing import Optional

import pytest

from livekit.portal import DType, Operator, OperatorConfig, Robot, RobotConfig


# `LIVEKIT_URL` is the explicit opt-in: integration tests are skipped
# entirely when it's missing so a bare `pytest` doesn't accidentally hit
# a server. The keys default to LiveKit's stock dev creds so a local
# `livekit-server --dev` run needs only `LIVEKIT_URL`.
URL = os.environ.get("LIVEKIT_URL")
API_KEY = os.environ.get("LIVEKIT_API_KEY", "devkey")
API_SECRET = os.environ.get("LIVEKIT_API_SECRET", "secret")


collect_ignore = (
    []
    if URL
    else [
        "test_chunks.py",
        "test_frame_video.py",
        "test_frame_video_stress.py",
        "test_e2ee.py",
        "test_multi_operator.py",
        "test_action_subscription.py",
        "test_webrtc_codecs.py",
        "test_ffi_role_split.py",
        "test_rpc.py",
    ]
)


async def wait_for(
    predicate,
    timeout_s: float = 15.0,
    interval_s: float = 0.05,
) -> bool:
    """Poll `predicate` until it returns truthy or `timeout_s` elapses, then
    return its final value.

    Use this instead of a fixed `asyncio.sleep` before a receive assertion.
    Large byte-stream payloads (e.g. a 25 MB raw frame) can take several
    seconds to traverse the SFU, so a fixed settle window races the transfer
    and flakes under load. Polling returns as soon as the data arrives and
    only waits out the full timeout on genuine failure.
    """
    import asyncio

    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout_s
    while loop.time() < deadline:
        if predicate():
            return True
        await asyncio.sleep(interval_s)
    return bool(predicate())


def _make_token(
    identity: str,
    room: str,
    *,
    attributes: Optional[dict] = None,
) -> str:
    # Imported lazily so the regular test suite doesn't need livekit-api
    # installed when integration tests are skipped.
    from livekit import api

    grants = api.VideoGrants(
        room_join=True,
        room=room,
        can_publish=True,
        can_subscribe=True,
        # v0.2 multi-controller relies on Portal self-setting the `role`
        # attribute on connect; the grant must permit it.
        can_update_own_metadata=True,
    )
    builder = (
        api.AccessToken(API_KEY, API_SECRET)
        .with_identity(identity)
        .with_grants(grants)
        .with_ttl(datetime.timedelta(hours=1))
    )
    if attributes:
        builder = builder.with_attributes(attributes)
    return builder.to_jwt()


class Pair:
    """Robot + Operator on the same fresh room, both connected.

    Tests that intentionally diverge schemas mutate `robot_cfg` /
    `operator_cfg` BEFORE calling `start()`. State schema is preset on
    both sides so observations can form, even when the test only cares
    about chunks.

    The operator self-claims the active-operator pointer after connect so
    its actions actually reach the robot — the action gate drops anything
    from a non-active operator.
    """

    def __init__(self) -> None:
        self.room = f"stress-{int(time.time()*1000)}-{os.urandom(2).hex()}"
        self.robot_cfg = RobotConfig(self.room)
        self.operator_cfg = OperatorConfig(self.room)
        self.robot_cfg.add_state_typed([("j", DType.F32)])
        self.operator_cfg.add_state_typed([("j", DType.F32)])
        self.robot: Optional[Robot] = None
        self.operator: Optional[Operator] = None

    async def start(self) -> None:
        import asyncio
        self.robot = Robot(self.robot_cfg)
        self.operator = Operator(self.operator_cfg)
        await self.robot.connect(URL, _make_token("robot", self.room))
        await asyncio.sleep(0.2)
        await self.operator.connect(URL, _make_token("operator", self.room))
        await self.operator.set_active_operator(self.operator.local_identity())
        await asyncio.sleep(0.1)

    async def stop(self) -> None:
        if self.operator:
            try:
                await self.operator.disconnect()
            except Exception:  # noqa: BLE001
                pass
        if self.robot:
            try:
                await self.robot.disconnect()
            except Exception:  # noqa: BLE001
                pass


@pytest.fixture
async def pair():
    """Yields a fresh Pair; the test populates configs and calls
    `await pair.start()` itself. Teardown disconnects both sides even if
    the test panics, so a flaky network failure doesn't leak rooms.
    """
    p = Pair()
    try:
        yield p
    finally:
        await p.stop()
