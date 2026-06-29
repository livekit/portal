"""RPC integration tests.

Covers handler registration both before and after `connect()`, plus error
propagation. The post-connect case is the regression guard: registering an
RPC handler on a live participant triggers SDK publisher negotiation, which
spawns onto the tokio runtime. When `register_rpc_method` is called from a
binding's asyncio thread (no runtime context), that spawn used to panic with
"there is no reactor running". The core now enters the runtime captured at
connect time around the registration.

Skipped automatically without `LIVEKIT_URL` (see conftest).
"""
from __future__ import annotations

import asyncio
import os

import pytest

from integration.conftest import URL, _make_token
from livekit.portal import (
    DType,
    Operator,
    OperatorConfig,
    PortalError,
    Robot,
    RobotConfig,
    RpcError,
)

pytestmark = pytest.mark.asyncio


async def test_rpc_register_after_connect(pair):
    """Regression: registering a handler AFTER connect must not panic and the
    method must be invocable from the peer."""
    await pair.start()

    async def handler(data):
        return f"pong:{data.payload}"

    pair.robot.register_rpc_method("ping", handler)
    await asyncio.sleep(0.1)

    result = await pair.operator.perform_rpc(
        "ping", "hi", destination=pair.robot.local_identity()
    )
    assert result == "pong:hi"


async def test_rpc_register_before_connect():
    """The pre-connect path keeps working: handlers registered before connect
    are applied on connect and invocable afterwards. Built manually rather
    than via the `pair` fixture so the robot exists before `connect()`.
    """
    room = f"rpc-pre-{os.urandom(4).hex()}"
    robot_cfg = RobotConfig(room)
    robot_cfg.add_state_typed([("j", DType.F32)])
    operator_cfg = OperatorConfig(room)
    operator_cfg.add_state_typed([("j", DType.F32)])
    robot = Robot(robot_cfg)
    operator = Operator(operator_cfg)

    async def handler(data):
        return f"echo:{data.payload}"

    robot.register_rpc_method("echo", handler)  # before connect

    try:
        await robot.connect(URL, _make_token("robot", room))
        await asyncio.sleep(0.2)
        await operator.connect(URL, _make_token("operator", room))
        await asyncio.sleep(0.2)
        result = await operator.perform_rpc(
            "echo", "x", destination=robot.local_identity()
        )
        assert result == "echo:x"
    finally:
        for side in (operator, robot):
            try:
                await side.disconnect()
            except Exception:  # noqa: BLE001
                pass


async def test_rpc_handler_error_propagates(pair):
    """An application error raised by the handler surfaces on the caller."""
    await pair.start()

    async def handler(data):
        raise RpcError.Error(code=1234, message="boom", data=None)

    pair.robot.register_rpc_method("fail", handler)
    await asyncio.sleep(0.1)

    with pytest.raises(PortalError):
        await pair.operator.perform_rpc(
            "fail", "", destination=pair.robot.local_identity()
        )
