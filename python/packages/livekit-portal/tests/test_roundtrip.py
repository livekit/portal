"""Random values survive the send-receive round trip for every dtype.

What this covers, and what it can't:

* **Send side** (`test_send_reaches_ffi_for_every_dtype`) actually crosses
  the FFI with a payload that uses every declared dtype. If the uniffi
  generator forgets a primitive converter (the `_UniffiFfiConverterFloat64`
  regression we hit at pin 86a8c0041f), `send_action` raises `NameError`
  here instead of the `PortalError.WrongRole` it should get from the
  role check further down the stack.

* **Receive side** (`test_random_values_roundtrip_through_wrapper`) runs
  per-dtype with random in-range inputs and simulates what the core
  delivers: narrow to the dtype, widen back to f64 via
  `TypedValue::as_f64`, hand that to the Python wrapper, and assert the
  wrapper reconstructs the original value. We can't reach a real peer
  in-process, so this stops at the FFI receive boundary; anything below
  it is covered by the core crate's Rust tests.
"""
from __future__ import annotations

import random
import struct
from typing import Any, List, Tuple

import pytest

from livekit.portal import (
    DType,
    FieldSpec,
    Portal,
    PortalConfig,
    PortalError,
    Role,
    _wrap_action,
)
from livekit.portal import livekit_portal_ffi as _ffi


# Every dtype the wire format supports. Kept as a flat list so a single
# parametrize sweeps them all.
_ALL_DTYPES: List[DType] = [
    DType.F64,
    DType.F32,
    DType.I32,
    DType.I16,
    DType.I8,
    DType.U32,
    DType.U16,
    DType.U8,
    DType.BOOL,
]

# (min, max) inclusive for each integer dtype, used for random sampling.
_INT_RANGES = {
    DType.I32: (-(2**31), 2**31 - 1),
    DType.I16: (-(2**15), 2**15 - 1),
    DType.I8: (-(2**7), 2**7 - 1),
    DType.U32: (0, 2**32 - 1),
    DType.U16: (0, 2**16 - 1),
    DType.U8: (0, 2**8 - 1),
}


def _f32_round(x: float) -> float:
    """Narrow an f64 to f32 and widen back — same as the core's
    `value as f32 as f64`. IEEE 754 pack/unpack is the canonical way."""
    return struct.unpack("<f", struct.pack("<f", x))[0]


def _random_value(dtype: DType, rng: random.Random) -> Any:
    if dtype == DType.BOOL:
        return rng.choice([True, False])
    if dtype == DType.F64:
        return rng.uniform(-1e9, 1e9)
    if dtype == DType.F32:
        # Sampled in f64 but will be narrowed downstream — we compare
        # against the f32-rounded expectation, so any value works.
        return rng.uniform(-1e6, 1e6)
    lo, hi = _INT_RANGES[dtype]
    return rng.randint(lo, hi)


def _simulate_delivered_f64(dtype: DType, value: Any) -> float:
    """What the core puts in the delivered `Dict[str, float]` after the
    dtype round trip (`TypedValue::as_f64`)."""
    if dtype == DType.BOOL:
        return 1.0 if value else 0.0
    if dtype == DType.F32:
        return _f32_round(value)
    return float(value)


def _expected_typed(dtype: DType, value: Any) -> Any:
    """What the wrapper's `_cast_values` should return for that delivery."""
    if dtype == DType.BOOL:
        return bool(value)
    if dtype == DType.F32:
        return _f32_round(value)
    if dtype == DType.F64:
        return float(value)
    return int(value)


# --- send: every dtype crosses the FFI without a NameError ----------------


def _all_dtypes_portal(role: Role) -> Tuple[Portal, List[FieldSpec]]:
    schema = [
        FieldSpec(name=f"v_{d.name.lower()}", dtype=d) for d in _ALL_DTYPES
    ]
    cfg = PortalConfig("roundtrip", role)
    cfg.add_state_typed(list(schema))
    cfg.add_action_typed(list(schema))
    return Portal(cfg), schema


def _sample_payload(schema: List[FieldSpec], seed: int):
    rng = random.Random(seed)
    return {f.name: _random_value(f.dtype, rng) for f in schema}


def test_send_reaches_ffi_for_every_dtype():
    portal, schema = _all_dtypes_portal(Role.OPERATOR)
    payload = _sample_payload(schema, seed=0xA11D7)
    try:
        portal.send_action(payload)
    except PortalError.DtypeMismatch as e:
        pytest.fail(f"random in-range payload rejected by validator: {e}")
    except PortalError:
        # WrongRole (portal isn't connected to a peer). Confirms the FFI
        # boundary was crossed cleanly — every map/primitive converter
        # the payload touches exists.
        pass


# --- receive: wrapper reconstructs random values per dtype -----------------


@pytest.mark.parametrize("dtype", _ALL_DTYPES, ids=lambda d: d.name)
def test_random_values_roundtrip_through_wrapper(dtype):
    # Fresh rng per dtype keeps one failure's diagnostics independent of
    # the others while still being fully deterministic.
    rng = random.Random(0xC0FFEE ^ hash(dtype.name))
    schema = [FieldSpec(name="v", dtype=dtype)]

    for _ in range(128):
        value = _random_value(dtype, rng)
        delivered = _simulate_delivered_f64(dtype, value)
        expected = _expected_typed(dtype, value)

        ffi_action = _ffi.Action(
            values={"v": delivered},
            timestamp_us=0,
            in_reply_to_ts_us=None,
            sender="",
        )
        got = _wrap_action(ffi_action, schema).values["v"]

        assert got == expected, (
            f"{dtype.name}: input={value!r} delivered={delivered!r} "
            f"wrapped={got!r} expected={expected!r}"
        )
        # bool / int / float are all truthy-equal to each other; the type
        # check is what keeps a BOOL from smuggling through an int field.
        assert type(got) is type(expected), (
            f"{dtype.name}: wrapped type {type(got).__name__} "
            f"!= expected {type(expected).__name__}"
        )
