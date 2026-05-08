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

from livekit.portal import DType, Portal, PortalConfig, Role


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
    ]
)


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
    """

    def __init__(self) -> None:
        self.room = f"stress-{int(time.time()*1000)}-{os.urandom(2).hex()}"
        self.robot_cfg = PortalConfig(self.room, Role.ROBOT)
        self.operator_cfg = PortalConfig(self.room, Role.OPERATOR)
        self.robot_cfg.add_state_typed([("j", DType.F32)])
        self.operator_cfg.add_state_typed([("j", DType.F32)])
        self.robot: Optional[Portal] = None
        self.operator: Optional[Portal] = None

    async def start(self) -> None:
        import asyncio
        self.robot = Portal(self.robot_cfg)
        self.operator = Portal(self.operator_cfg)
        await self.robot.connect(URL, _make_token("robot", self.room))
        await asyncio.sleep(0.2)
        await self.operator.connect(URL, _make_token("operator", self.room))
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
