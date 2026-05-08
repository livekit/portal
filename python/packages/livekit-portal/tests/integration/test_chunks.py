"""Integration tests for action chunks + correlation against a live LiveKit.

Each scenario uses the `pair` fixture from `conftest.py` to spin up a
robot + operator on a fresh room. Assertions cover the edge cases most
likely to misbehave in production: large payloads, schema mismatch,
backpressure, NaN/inf preservation, integer saturation, length mismatch,
disconnect cleanup, reconnect.

Skipped automatically when `LIVEKIT_URL` isn't set (see conftest).
"""
from __future__ import annotations

import asyncio

import numpy as np
import pytest

from livekit.portal import (
    ActionChunk,
    DType,
    PortalError,
)

# Tests are async — pytest-asyncio 0.23+ supports per-test marker.
pytestmark = pytest.mark.asyncio


# Short settle window so async deliveries reach the Robot before assertions.
SETTLE_S = 0.6


# ---------------------------------------------------------------------------
# Wire / serialization edge cases
# ---------------------------------------------------------------------------


async def test_big_chunk_above_data_packet_cap(pair):
    """100 rows × 30 F64 fields = 24,020-byte payload. Above the 15 KB
    data-packet limit, so this exercises the byte-stream fragmentation
    path that motivates using byte streams in the first place."""
    horizon = 100
    fields = [(f"j{i}", DType.F64) for i in range(30)]
    pair.robot_cfg.add_action_chunk("act", horizon=horizon, fields=fields)
    pair.operator_cfg.add_action_chunk("act", horizon=horizon, fields=fields)

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("act", lambda c: received.append(c))

    payload = np.random.RandomState(42).randn(horizon, 30).astype(np.float64)
    pair.operator.send_action_chunk("act", payload)
    await asyncio.sleep(SETTLE_S + 0.4)

    assert len(received) == 1, f"expected 1 chunk, got {len(received)}"
    chunk = received[0]
    # F64 has no narrowing — column should equal exactly.
    assert chunk.raw_data["j0"] == payload[:, 0].tolist()
    assert chunk.horizon == horizon


async def test_schema_mismatch_horizon_drops(pair):
    """Operator declares horizon=10, robot declares horizon=20 →
    fingerprints diverge → robot must drop without firing the callback."""
    pair.robot_cfg.add_action_chunk(
        "act", horizon=20, fields=[("j", DType.F32)]
    )
    pair.operator_cfg.add_action_chunk(
        "act", horizon=10, fields=[("j", DType.F32)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("act", lambda c: received.append(c))

    pair.operator.send_action_chunk("act", {"j": [0.0] * 10})
    await asyncio.sleep(SETTLE_S)

    assert received == []
    assert pair.robot.metrics().transport.action_chunks_received == 0


async def test_schema_mismatch_dtype_drops(pair):
    """Same name, same horizon, different dtype → fingerprints diverge."""
    pair.robot_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F32)]
    )
    pair.operator_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F64)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("act", lambda c: received.append(c))

    pair.operator.send_action_chunk("act", {"j": [0.0, 0.1, 0.2, 0.3]})
    await asyncio.sleep(SETTLE_S)

    assert received == []


async def test_burst_300_chunks(pair):
    """300 chunks back-to-back. Mpsc queue cap is 1024, so this should
    not drop. Stresses both the publisher's drainer and the receiver's
    byte-stream open rate."""
    fields = [("j", DType.F32)]
    pair.robot_cfg.add_action_chunk("act", horizon=4, fields=fields)
    pair.operator_cfg.add_action_chunk("act", horizon=4, fields=fields)

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("act", lambda c: received.append(c))

    for i in range(300):
        pair.operator.send_action_chunk(
            "act",
            {"j": [float(i), float(i + 1), float(i + 2), float(i + 3)]},
        )

    await asyncio.sleep(2.0)

    assert len(received) >= 250, f"received only {len(received)}/300"
    # All received chunks must be well-formed.
    assert all(len(c.raw_data["j"]) == 4 for c in received)


# ---------------------------------------------------------------------------
# Numerical edge cases
# ---------------------------------------------------------------------------


async def test_nan_inf_round_trip(pair):
    """NaN, +inf, -inf in F32 column. Wire is bit-pattern preserving
    for IEEE 754, so all three survive the f64 → f32 narrow."""
    pair.robot_cfg.add_action_chunk(
        "ext", horizon=4, fields=[("j", DType.F32)]
    )
    pair.operator_cfg.add_action_chunk(
        "ext", horizon=4, fields=[("j", DType.F32)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("ext", lambda c: received.append(c))

    pair.operator.send_action_chunk(
        "ext", {"j": [float("nan"), float("inf"), float("-inf"), 0.5]}
    )
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    out = received[0].data["j"]
    assert np.isnan(out[0])
    assert out[1] == np.float32("inf")
    assert out[2] == np.float32("-inf")
    assert float(out[3]) == 0.5


async def test_i8_saturation_clamps(pair):
    """I8 column with out-of-range values. Encoder saturates to
    [-128, 127] and emits a one-shot warn; decode still succeeds."""
    pair.robot_cfg.add_action_chunk(
        "ctrl", horizon=4, fields=[("mode", DType.I8)]
    )
    pair.operator_cfg.add_action_chunk(
        "ctrl", horizon=4, fields=[("mode", DType.I8)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("ctrl", lambda c: received.append(c))

    pair.operator.send_action_chunk(
        "ctrl", {"mode": [500.0, -500.0, 42.0, 0.0]}
    )
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    assert list(received[0].data["mode"]) == [127, -128, 42, 0]


async def test_bool_round_trip(pair):
    """BOOL column: any non-zero non-NaN is truthy."""
    pair.robot_cfg.add_action_chunk(
        "grip", horizon=4, fields=[("g", DType.BOOL)]
    )
    pair.operator_cfg.add_action_chunk(
        "grip", horizon=4, fields=[("g", DType.BOOL)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("grip", lambda c: received.append(c))

    pair.operator.send_action_chunk("grip", {"g": [1.0, 0.0, -1.0, 0.5]})
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    assert list(received[0].data["g"]) == [True, False, True, True]


# ---------------------------------------------------------------------------
# Send-side shape handling
# ---------------------------------------------------------------------------


async def test_short_column_zero_padded(pair):
    """Column shorter than horizon: tail zero-padded by `serialize_chunk`."""
    pair.robot_cfg.add_action_chunk(
        "pad", horizon=4, fields=[("j", DType.F64)]
    )
    pair.operator_cfg.add_action_chunk(
        "pad", horizon=4, fields=[("j", DType.F64)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("pad", lambda c: received.append(c))

    pair.operator.send_action_chunk("pad", {"j": [1.0, 2.0]})
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    assert received[0].raw_data["j"] == [1.0, 2.0, 0.0, 0.0]


async def test_long_column_truncated(pair):
    """Column longer than horizon: tail truncated."""
    pair.robot_cfg.add_action_chunk(
        "trim", horizon=4, fields=[("j", DType.F64)]
    )
    pair.operator_cfg.add_action_chunk(
        "trim", horizon=4, fields=[("j", DType.F64)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("trim", lambda c: received.append(c))

    pair.operator.send_action_chunk(
        "trim", {"j": [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]}
    )
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    assert received[0].raw_data["j"] == [1.0, 2.0, 3.0, 4.0]


async def test_empty_data_dict_zero_fills(pair):
    """`send_action_chunk` with `{}` zero-fills every declared column."""
    pair.robot_cfg.add_action_chunk(
        "zero", horizon=3, fields=[("a", DType.F32), ("b", DType.I16)]
    )
    pair.operator_cfg.add_action_chunk(
        "zero", horizon=3, fields=[("a", DType.F32), ("b", DType.I16)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("zero", lambda c: received.append(c))

    pair.operator.send_action_chunk("zero", {})
    await asyncio.sleep(SETTLE_S)

    assert len(received) == 1
    assert list(received[0].data["a"]) == [0.0, 0.0, 0.0]
    assert list(received[0].data["b"]) == [0, 0, 0]


# ---------------------------------------------------------------------------
# API contract
# ---------------------------------------------------------------------------


async def test_undeclared_chunk_send_raises(pair):
    pair.operator_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F32)]
    )
    await pair.start()

    with pytest.raises(PortalError.UnknownChunk):
        pair.operator.send_action_chunk("never_declared", {"j": [0.0] * 4})


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------


async def test_send_then_immediate_disconnect(pair):
    """Send a chunk, immediately disconnect both sides. No panic, no hang."""
    pair.robot_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F32)]
    )
    pair.operator_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F32)]
    )
    await pair.start()
    pair.operator.send_action_chunk("act", {"j": [0.0, 0.1, 0.2, 0.3]})
    # `pair.stop()` runs in the fixture teardown — that's the actual test:
    # it must not hang or raise.


async def test_disconnect_then_reconnect_fresh_room():
    """Reconnect path: end the first session, start a fresh Pair, verify
    chunks flow on the new connection. Tests that publishers/slots reset
    correctly across disconnect + new construction.
    """
    from livekit.portal import DType  # local import to avoid module-level
    from .conftest import Pair

    fields = [("j", DType.F32)]

    pair1 = Pair()
    pair1.robot_cfg.add_action_chunk("act", horizon=4, fields=fields)
    pair1.operator_cfg.add_action_chunk("act", horizon=4, fields=fields)
    received1: list[ActionChunk] = []
    await pair1.start()
    pair1.robot.on_action_chunk("act", lambda c: received1.append(c))
    pair1.operator.send_action_chunk("act", {"j": [1.0, 2.0, 3.0, 4.0]})
    await asyncio.sleep(SETTLE_S)
    await pair1.stop()

    pair2 = Pair()
    pair2.robot_cfg.add_action_chunk("act", horizon=4, fields=fields)
    pair2.operator_cfg.add_action_chunk("act", horizon=4, fields=fields)
    received2: list[ActionChunk] = []
    try:
        await pair2.start()
        pair2.robot.on_action_chunk("act", lambda c: received2.append(c))
        pair2.operator.send_action_chunk("act", {"j": [5.0, 6.0, 7.0, 8.0]})
        await asyncio.sleep(SETTLE_S)
    finally:
        await pair2.stop()

    assert len(received1) == 1
    assert len(received2) == 1


# ---------------------------------------------------------------------------
# Correlation + policy metrics
# ---------------------------------------------------------------------------


async def test_correlation_populates_e2e_metrics(pair):
    """Send N chunks each with `in_reply_to_ts_us`, verify the robot's
    `metrics.policy.e2e_us_p50/p95` populate and `correlated_received`
    counts every received chunk."""
    pair.robot_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F32)]
    )
    pair.operator_cfg.add_action_chunk(
        "act", horizon=4, fields=[("j", DType.F32)]
    )

    received: list[ActionChunk] = []
    await pair.start()
    pair.robot.on_action_chunk("act", lambda c: received.append(c))

    import time
    for _ in range(10):
        ts = int(time.time() * 1_000_000) - 5_000  # pretend obs was 5 ms ago
        pair.operator.send_action_chunk(
            "act", {"j": [0.0, 0.1, 0.2, 0.3]}, in_reply_to_ts_us=ts
        )
        await asyncio.sleep(0.02)

    await asyncio.sleep(SETTLE_S)

    assert len(received) == 10
    m = pair.robot.metrics()
    assert m.policy.correlated_received == 10
    assert m.policy.e2e_us_p50 is not None
    assert m.policy.e2e_us_p95 is not None
    # All chunks correlated.
    assert all(c.in_reply_to_ts_us is not None for c in received)
