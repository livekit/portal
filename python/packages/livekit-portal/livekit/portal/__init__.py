"""livekit-portal. Python bindings.

Thin ergonomic wrapper over the UniFFI-generated `livekit_portal_ffi`
module. The generated module already exposes `Portal`, `PortalConfig`, the
record types (`Observation`, `Action`, `State`, `VideoFrame`, `PortalMetrics`
and nested submetrics), the `PortalError` exception, and the
`PortalCallbacks` foreign trait. This module:

  * Renames `VideoFrame` to `VideoFrameData` for backwards API parity with
    the old protobuf-based wrapper (consumers import `VideoFrameData`).
  * Adds `Portal.on_action / on_state / on_observation / on_video_frame /
    on_drop` convenience registrations, routed through an internal dispatcher
    that implements `PortalCallbacks`. Callbacks run on the asyncio event
    loop of the thread that registered them (not on the tokio worker that
    fires the event), matching the previous wrapper's semantics.
  * Adds frame-normalization on `send_video_frame` (accept bytes or
    `np.ndarray(H, W, 3)` uint8 and infer W/H from the array).

Frame format is RGB24 in both directions: `send_video_frame` accepts RGB,
and received `VideoFrameData.data` is packed RGB (`W*H*3` bytes) regardless
of transport — WebRTC frames are color-converted from I420 in Rust before
delivery, frame-video frames are codec-decoded to RGB. Use
`livekit.portal.frame_bytes_to_numpy_rgb` for a typed `(H, W, 3)` view.
"""
from __future__ import annotations

import asyncio
import logging
import numbers
import threading
import traceback
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, Iterable, List, Optional, Tuple, Union

_log = logging.getLogger(__name__)

from . import _frame
from . import livekit_portal_ffi as _ffi
from ._frame import frame_bytes_to_numpy_rgb

# Re-export generated types that don't carry dtype-sensitive payload. The
# UniFFI module is the source of truth for these.
Role = _ffi.Role
DType = _ffi.DType
VideoCodec = _ffi.VideoCodec
FieldSpec = _ffi.FieldSpec
FrameVideoSpec = _ffi.FrameVideoSpec
ChunkSpec = _ffi.ChunkSpec
VideoFrameData = _ffi.VideoFrame
PortalMetrics = _ffi.PortalMetrics
SyncMetrics = _ffi.SyncMetrics
TransportMetrics = _ffi.TransportMetrics
BufferMetrics = _ffi.BufferMetrics
RttMetrics = _ffi.RttMetrics
PolicyMetrics = _ffi.PolicyMetrics
PortalError = _ffi.PortalError
RpcInvocationData = _ffi.RpcInvocationData
RpcError = _ffi.RpcError

# Default JPEG quality for `add_video` with `VideoCodec.MJPEG` when no
# explicit value is given. Mirrors the Rust core's `DEFAULT_MJPEG_QUALITY`.
DEFAULT_MJPEG_QUALITY: int = 90

# A schema entry accepted by add_state_typed/add_action_typed. Either a
# FieldSpec (record passthrough) or a (name, dtype) tuple — the latter is
# the natural Python shape.
SchemaEntry = Union[Tuple[str, DType], FieldSpec]

# One field value after `_cast_values` reconstruction. Matches the
# declared dtype family: `BOOL` → `bool`, integer dtypes → `int`, float
# dtypes → `float`. Exposed at module scope so static type-checkers can
# reason about the shape of `action.values` / `state.values` /
# `observation.state`.
TypedScalar = Union[bool, int, float]


def _to_field_specs(schema: Iterable[SchemaEntry]) -> List[FieldSpec]:
    out: List[FieldSpec] = []
    for entry in schema:
        if isinstance(entry, FieldSpec):
            out.append(entry)
        else:
            name, dtype = entry
            out.append(FieldSpec(name=name, dtype=dtype))
    return out


_INT_DTYPES = frozenset(
    {DType.I32, DType.I16, DType.I8, DType.U32, DType.U16, DType.U8}
)
_FLOAT_DTYPES = frozenset({DType.F64, DType.F32})

# numpy is listed in pyproject.toml's runtime deps (camera frames need it).
# Its scalar types register with `numbers.Integral` / `numbers.Real` for
# int/float, but `np.bool_` does *not* register as `bool`/`Integral`/`Real`,
# so accept it explicitly. Falls back cleanly if numpy is somehow absent.
try:
    import numpy as _np  # noqa: F401
    _NUMPY_BOOL_TYPES: Tuple[type, ...] = (bool, _np.bool_)
except ImportError:  # pragma: no cover
    _NUMPY_BOOL_TYPES = (bool,)


def _validate_send_values(
    values: Dict[str, Any],
    schema: List[FieldSpec],
    stream: str,
) -> None:
    """Reject a send payload whose values' Python types disagree with the
    declared dtype. Mirrors the core Rust `PortalError::DtypeMismatch`
    check — raised before we cross the FFI boundary so the caller sees
    the bug at the earliest point.

    Rules:
      - `DType.BOOL` → `bool` or `numpy.bool_`.
      - integer dtypes → any `numbers.Integral` (int, numpy int kinds)
        except booleans (Python `bool` is-a `int`; `numpy.bool_` is
        treated the same way to match).
      - float dtypes → any `numbers.Real` (int, float, numpy numerics)
        except booleans.

    Keys absent from the schema skip validation — they're reported
    separately by the core publisher's unknown-key warn path.
    """
    # Build a quick lookup once per call. Schemas are small (typical << 32
    # fields) so the dict overhead is negligible versus a linear scan per
    # value.
    declared: Dict[str, DType] = {f.name: f.dtype for f in schema}
    for name, v in values.items():
        dtype = declared.get(name)
        if dtype is None:
            continue
        if dtype == DType.BOOL:
            ok = isinstance(v, _NUMPY_BOOL_TYPES)
        elif dtype in _INT_DTYPES:
            ok = (
                isinstance(v, numbers.Integral)
                and not isinstance(v, _NUMPY_BOOL_TYPES)
            )
        else:  # float dtype
            ok = (
                isinstance(v, numbers.Real)
                and not isinstance(v, _NUMPY_BOOL_TYPES)
            )
        if not ok:
            # `flat_error` on the FFI side means the generated
            # `PortalError.DtypeMismatch` class takes the formatted
            # message as a single positional arg; the structured fields
            # are embedded in the string, matching the error surfaced by
            # the Rust core.
            raise PortalError.DtypeMismatch(
                f"field '{name}' declared as {dtype} but sent as {type(v).__name__}"
            )


def _cast_values(
    values: Dict[str, float], schema: List[FieldSpec]
) -> Dict[str, TypedScalar]:
    """Map each value to its declared Python type: `BOOL` → `bool`, integer
    dtypes → `int`, float dtypes → `float`. Keys missing from `schema` are
    dropped; absent schema fields are omitted from the result.

    The core pipeline widens every dtype to `f64` for carry-forward and
    buffering; because every supported integer dtype (I32/I16/I8 and
    U32/U16/U8) fits in the 53-bit mantissa of f64, the round trip through
    the pipeline is lossless and this cast is exact.
    """
    out: Dict[str, TypedScalar] = {}
    for field in schema:
        if field.name not in values:
            continue
        v = values[field.name]
        if field.dtype == DType.BOOL:
            out[field.name] = bool(v)
        elif field.dtype in _INT_DTYPES:
            out[field.name] = int(v)
        else:
            out[field.name] = float(v)
    return out


# --- Delivery records -------------------------------------------------------
#
# The FFI delivers Action / State / Observation with `values` as
# `Dict[str, float]` because the core pipeline is f64 throughout (see
# rationale on `_cast_values`). The records below wrap the FFI payload,
# re-cast `values` per the declared schema, and expose `raw_values` as an
# escape hatch for callers that want the f64 dict (e.g. writing into a
# numpy buffer without a per-field Python cast).
#
# These replace the FFI Observation/Action/State in the public API. They
# are duck-compatible on the attributes user code reads (`values`,
# `state`, `frames`, `timestamp_us`).

@dataclass(frozen=True, slots=True)
class Action:
    """An action received from the operator.

    `values` holds Python-native types per the declared action schema.
    `raw_values` is the original `Dict[str, float]` with every dtype
    widened to `f64`, for callers that want to skip the per-field cast.

    `in_reply_to_ts_us` is the observation timestamp this action was
    produced from, when the operator passed one. Use it to compute true
    end-to-end policy latency (`now_us - in_reply_to_ts_us`) instead of
    network RTT.
    """

    values: Dict[str, TypedScalar]
    raw_values: Dict[str, float]
    timestamp_us: int
    in_reply_to_ts_us: Optional[int] = None


@dataclass(frozen=True, slots=True)
class ActionChunk:
    """An action chunk received from the operator.

    `data` is `field -> numpy.ndarray` per the declared chunk schema.
    Each array has length `horizon` and dtype matching the field's
    declared `DType`. `raw_data` is the same payload as
    `Dict[str, list[float]]` — the lossless f64 view, useful when you
    want to skip the per-field numpy reconstruction.

    `in_reply_to_ts_us` matches the same field on `Action`.
    """

    name: str
    horizon: int
    data: Dict[str, Any]
    raw_data: Dict[str, List[float]]
    timestamp_us: int
    in_reply_to_ts_us: Optional[int] = None


@dataclass(frozen=True, slots=True)
class State:
    """A state sample received from the robot.

    Semantics for `values` / `raw_values` match `Action`.
    """

    values: Dict[str, TypedScalar]
    raw_values: Dict[str, float]
    timestamp_us: int


@dataclass(frozen=True, slots=True)
class Observation:
    """A synchronized observation: matched video frames + state sample.

    `state` holds Python-native types per the declared state schema;
    `raw_state` keeps the f64 dict. `frames` is unchanged from the FFI
    layer — one entry per registered video track.
    """

    state: Dict[str, TypedScalar]
    raw_state: Dict[str, float]
    frames: Dict[str, VideoFrameData]
    timestamp_us: int


def _normalize_chunk_data(
    data: Any, schema: List[FieldSpec]
) -> Dict[str, List[float]]:
    """Coerce send-side chunk data into the `Dict[str, list[float]]` the
    FFI accepts. Two input shapes:

    * `numpy.ndarray` of shape `(horizon, len(schema))` — split into
      per-field columns in declared order. Convenient for uniform-dtype
      VLA tensors.
    * `Dict[str, ndarray | list]` — pass-through, with each column cast
      to a `list[float]`. Unknown keys go through to the core, which
      warns once each.
    """
    if _np is not None and isinstance(data, _np.ndarray):
        if data.ndim != 2 or data.shape[1] != len(schema):
            raise PortalError.Deserialization(
                f"chunk ndarray must be shape (horizon, {len(schema)}); got {data.shape}"
            )
        cols: Dict[str, List[float]] = {}
        for i, field in enumerate(schema):
            cols[field.name] = data[:, i].astype(_np.float64).tolist()
        return cols
    if not isinstance(data, dict):
        raise PortalError.Deserialization(
            "chunk data must be a dict or 2D numpy array"
        )
    cols = {}
    for name, column in data.items():
        if _np is not None and isinstance(column, _np.ndarray):
            cols[name] = column.astype(_np.float64).tolist()
        else:
            cols[name] = [float(v) for v in column]
    return cols


def _wrap_action(
    action: _ffi.Action, schema: List[FieldSpec]
) -> Action:
    return Action(
        values=_cast_values(action.values, schema),
        raw_values=action.values,
        timestamp_us=action.timestamp_us,
        in_reply_to_ts_us=action.in_reply_to_ts_us,
    )


# Numpy dtype mapping for chunk reconstruction. Defined once at import so
# the per-frame hot path doesn't re-allocate the lookup table.
try:
    import numpy as _np
    _NUMPY_DTYPE_MAP = {
        DType.F64: _np.float64,
        DType.F32: _np.float32,
        DType.I32: _np.int32,
        DType.I16: _np.int16,
        DType.I8: _np.int8,
        DType.U32: _np.uint32,
        DType.U16: _np.uint16,
        DType.U8: _np.uint8,
        DType.BOOL: _np.bool_,
    }
except ImportError:  # pragma: no cover
    _np = None
    _NUMPY_DTYPE_MAP = {}


def _wrap_action_chunk(
    chunk: _ffi.ActionChunk, schema_by_name: Dict[str, List[FieldSpec]]
) -> ActionChunk:
    """Reconstruct typed numpy arrays per field. The FFI hands us
    `Dict[str, list[float]]` because the core pipeline is f64; we cast each
    column back to its declared dtype so policies that emit `float32[H]`
    don't pay an unwanted widening on receipt.
    """
    fields = schema_by_name.get(chunk.name, [])
    raw_data: Dict[str, List[float]] = chunk.data
    if _np is None:
        # numpy is a hard runtime dep on this package, so this branch only
        # exists for hypothetical embedded builds. Hand back the raw lists
        # in `data` so callers can still consume the chunk.
        data: Dict[str, Any] = dict(raw_data)
    else:
        data = {}
        for field in fields:
            column = chunk.data.get(field.name)
            if column is None:
                continue
            np_dtype = _NUMPY_DTYPE_MAP.get(field.dtype, _np.float64)
            data[field.name] = _np.asarray(column, dtype=np_dtype)
    return ActionChunk(
        name=chunk.name,
        horizon=chunk.horizon,
        data=data,
        raw_data=raw_data,
        timestamp_us=chunk.timestamp_us,
        in_reply_to_ts_us=chunk.in_reply_to_ts_us,
    )


def _wrap_state(
    state: _ffi.State, schema: List[FieldSpec]
) -> State:
    return State(
        values=_cast_values(state.values, schema),
        raw_values=state.values,
        timestamp_us=state.timestamp_us,
    )


def _wrap_observation(
    obs: _ffi.Observation, state_schema: List[FieldSpec]
) -> Observation:
    return Observation(
        state=_cast_values(obs.state, state_schema),
        raw_state=obs.state,
        frames=obs.frames,
        timestamp_us=obs.timestamp_us,
    )


# --- Dispatcher -------------------------------------------------------------

class _Dispatcher(_ffi.PortalCallbacks):
    """Sits behind a Portal as its `PortalCallbacks` implementation.

    The foreign-trait methods run on the tokio worker thread that fired the
    event — we must *not* execute user code there (long work would block the
    video/state receive path, and reentering the FFI from there would
    deadlock). Everything is hopped onto the registered asyncio loop via
    `call_soon_threadsafe`.
    """

    def __init__(
        self,
        action_schema: List[FieldSpec],
        state_schema: List[FieldSpec],
        chunk_schemas: Dict[str, List[FieldSpec]],
    ) -> None:
        self._lock = threading.Lock()
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._action_cb: Optional[Callable[[Action], Any]] = None
        self._state_cb: Optional[Callable[[State], Any]] = None
        self._observation_cb: Optional[Callable[[Observation], Any]] = None
        self._drop_cb: Optional[Callable[[List[Dict[str, Any]]], Any]] = None
        # Per-track video callback: track_name → callable(track_name, frame).
        self._video_cbs: Dict[str, Callable[[str, VideoFrameData], Any]] = {}
        # Per-chunk callback: chunk_name → callable(chunk).
        self._chunk_cbs: Dict[str, Callable[[ActionChunk], Any]] = {}
        # Multi-controller callbacks (v0.2). All three are optional; nothing
        # fires when unset.
        self._operator_joined_cb: Optional[Callable[[str], Any]] = None
        self._operator_left_cb: Optional[Callable[[str], Any]] = None
        self._active_operator_changed_cb: Optional[Callable[[Optional[str]], Any]] = None
        # Schemas are frozen at Portal construction and read by the wrap
        # helpers below on every delivery.
        self._action_schema = action_schema
        self._state_schema = state_schema
        self._chunk_schemas = chunk_schemas

    def bind_loop(self, loop: asyncio.AbstractEventLoop) -> None:
        with self._lock:
            self._loop = loop

    def _schedule(self, cb: Callable[..., Any], *args: Any) -> None:
        loop = self._loop
        if loop is None:
            _safely_call(cb, *args)
            return
        loop.call_soon_threadsafe(_safely_call, cb, *args)

    # --- PortalCallbacks trait impls (called from Rust/tokio thread) --------

    def on_action(self, action: _ffi.Action) -> None:
        cb = self._action_cb
        if cb is not None:
            self._schedule(cb, _wrap_action(action, self._action_schema))

    def on_state(self, state: _ffi.State) -> None:
        cb = self._state_cb
        if cb is not None:
            self._schedule(cb, _wrap_state(state, self._state_schema))

    def on_observation(self, observation: _ffi.Observation) -> None:
        cb = self._observation_cb
        if cb is not None:
            self._schedule(
                cb, _wrap_observation(observation, self._state_schema)
            )

    def on_video_frame(self, track_name: str, frame: VideoFrameData) -> None:
        cb = self._video_cbs.get(track_name)
        if cb is not None:
            self._schedule(cb, track_name, frame)

    def on_drop(self, dropped: List[Dict[str, float]]) -> None:
        cb = self._drop_cb
        if cb is not None:
            # Drops are the state values that couldn't be matched to a
            # frame. Cast each to typed values so the callback sees the
            # same shape it gets from `on_state` / `on_observation`.
            typed = [_cast_values(d, self._state_schema) for d in dropped]
            self._schedule(cb, typed)

    def on_action_chunk(self, chunk: _ffi.ActionChunk) -> None:
        cb = self._chunk_cbs.get(chunk.name)
        if cb is not None:
            self._schedule(cb, _wrap_action_chunk(chunk, self._chunk_schemas))

    def on_operator_joined(self, identity: str) -> None:
        cb = self._operator_joined_cb
        if cb is not None:
            self._schedule(cb, identity)

    def on_operator_left(self, identity: str) -> None:
        cb = self._operator_left_cb
        if cb is not None:
            self._schedule(cb, identity)

    def on_active_operator_changed(self, identity: Optional[str]) -> None:
        cb = self._active_operator_changed_cb
        if cb is not None:
            self._schedule(cb, identity)

    # --- Registration (from Python user thread) -----------------------------

    def set_action(self, cb: Callable[[Action], Any]) -> None:
        self._action_cb = cb

    def set_state(self, cb: Callable[[State], Any]) -> None:
        self._state_cb = cb

    def set_observation(self, cb: Callable[[Observation], Any]) -> None:
        self._observation_cb = cb

    def set_drop(self, cb: Callable[[List[Dict[str, Any]]], Any]) -> None:
        self._drop_cb = cb

    def set_video(self, track_name: str, cb: Callable[[str, VideoFrameData], Any]) -> None:
        self._video_cbs[track_name] = cb

    def set_action_chunk(
        self, chunk_name: str, cb: Callable[[ActionChunk], Any]
    ) -> None:
        self._chunk_cbs[chunk_name] = cb

    def set_operator_joined(self, cb: Callable[[str], Any]) -> None:
        self._operator_joined_cb = cb

    def set_operator_left(self, cb: Callable[[str], Any]) -> None:
        self._operator_left_cb = cb

    def set_active_operator_changed(self, cb: Callable[[Optional[str]], Any]) -> None:
        self._active_operator_changed_cb = cb


_uniffi_bound_loop: Optional[asyncio.AbstractEventLoop] = None
_uniffi_bound_loop_lock = threading.Lock()


def _set_uniffi_event_loop(loop: asyncio.AbstractEventLoop) -> None:
    """Point UniFFI's global async-trait dispatch at `loop`.

    The underlying `uniffi_set_event_loop` is a process-global — multiple
    Portals on different asyncio loops in the same process will collide.
    Warn on mismatch so the misuse is at least visible rather than a silent
    cross-loop dispatch. The normal single-loop case is a no-op on the
    second call.
    """
    global _uniffi_bound_loop
    with _uniffi_bound_loop_lock:
        if _uniffi_bound_loop is loop:
            return
        if _uniffi_bound_loop is not None and _uniffi_bound_loop.is_running():
            _log.warning(
                "livekit-portal: multiple Portals bound to different asyncio "
                "loops in the same process; RPC handler dispatch will run on "
                "the most-recently-connected loop. This is a UniFFI "
                "limitation (uniffi_set_event_loop is process-global)."
            )
        _uniffi_bound_loop = loop
        _ffi.uniffi_set_event_loop(loop)


def _safely_call(cb: Callable[..., Any], *args: Any) -> None:
    try:
        result = cb(*args)
        # If the user registered `async def`, schedule the coroutine on the
        # current event loop. `call_soon_threadsafe` runs the outer callable
        # inside the loop thread, so `get_event_loop` here is safe.
        if asyncio.iscoroutine(result):
            asyncio.ensure_future(result)
    except BaseException:  # noqa: BLE001
        traceback.print_exc()


# --- RPC handler adapter ----------------------------------------------------


class _RpcHandlerAdapter(_ffi.RpcHandler):
    """Wrap a user callable so it satisfies the UniFFI async trait.

    Accepts either `async def handle(data) -> str` or sync `def handle(data) -> str`.
    Sync callables run inline on the asyncio loop, so handlers doing blocking
    work should use `async def` and `await asyncio.to_thread(...)` themselves.
    Raising `RpcError.Error` propagates to the caller; any other exception
    becomes a generic application error (code 1500).
    """

    def __init__(self, callback: Callable[[RpcInvocationData], Any]) -> None:
        super().__init__()
        self._callback = callback

    async def handle(self, data: RpcInvocationData) -> str:
        try:
            result = self._callback(data)
            if asyncio.iscoroutine(result):
                return await result
            return result  # type: ignore[return-value]
        except (_ffi.RpcError, asyncio.CancelledError):
            # RpcError: user-signalled application error, propagate verbatim.
            # CancelledError: the Rust side dropped the future (timeout or
            # caller cancellation); let asyncio unwind the task cleanly
            # instead of writing a bogus result on a torn-down handle.
            raise
        except Exception as e:  # noqa: BLE001
            traceback.print_exc()
            raise _ffi.RpcError.Error(
                code=1500,
                message=f"handler raised {type(e).__name__}: {e}",
                data=None,
            ) from e


# --- PortalConfig -----------------------------------------------------------

class PortalConfig:
    """Builder for a Portal session.

    State (`video_tracks`, `state_schema`, `action_schema`) is mirrored in
    Python for fast lookup — the Rust side owns the authoritative copy.
    Use `add_state_typed` / `add_action_typed` with `(name, DType)` pairs to
    declare fields.
    """

    __slots__ = (
        "_inner",
        "_session",
        "_role",
        "_video_tracks",
        "_frame_video_tracks",
        "_state_schema",
        "_action_schema",
        "_action_chunks",
    )

    def __init__(self, session: str, role: Role) -> None:
        self._inner = _ffi.PortalConfig(session, role)
        self._session = session
        self._role = role
        self._video_tracks: List[str] = []
        self._frame_video_tracks: List[FrameVideoSpec] = []
        self._state_schema: List[FieldSpec] = []
        self._action_schema: List[FieldSpec] = []
        self._action_chunks: List[ChunkSpec] = []

    @property
    def session(self) -> str:
        return self._session

    @property
    def role(self) -> Role:
        return self._role

    @property
    def video_tracks(self) -> List[str]:
        return list(self._video_tracks)

    @property
    def frame_video_tracks(self) -> List[FrameVideoSpec]:
        return list(self._frame_video_tracks)

    @property
    def state_fields(self) -> List[str]:
        return [f.name for f in self._state_schema]

    @property
    def action_fields(self) -> List[str]:
        return [f.name for f in self._action_schema]

    @property
    def state_schema(self) -> List[FieldSpec]:
        return list(self._state_schema)

    @property
    def action_schema(self) -> List[FieldSpec]:
        return list(self._action_schema)

    @property
    def action_chunks(self) -> List[ChunkSpec]:
        """All declared action chunks, in declaration order."""
        return list(self._action_chunks)

    def add_video(
        self,
        name: str,
        codec: VideoCodec = VideoCodec.H264,
        quality: int = DEFAULT_MJPEG_QUALITY,
    ) -> None:
        """Declare a video track.

        `codec` picks both the encoding and the wire transport. The
        user-facing send/receive API is identical regardless of codec —
        `send_video_frame` accepts RGB and `on_video_frame` /
        `get_video_frame` deliver RGB.

          * `VideoCodec.H264` (default) — WebRTC media path. Real-time
            RTP/SRTP, lossy, best-effort. Lowest end-to-end latency at
            scale. `quality` is ignored.
          * `VideoCodec.RAW` — byte-stream, uncompressed RGB24. Largest
            payload, zero encode cost. `quality` is ignored.
          * `VideoCodec.PNG` — byte-stream, lossless. ~2-3x compression on
            natural images. `quality` is ignored.
          * `VideoCodec.MJPEG` — byte-stream, lossy per-frame JPEG. ~10-20x
            compression at quality 90, sub-millisecond decode. Use for
            inference where frame independence matters but bit-exactness
            doesn't.

        `quality` is in 1..=100 and is honored for `VideoCodec.MJPEG`. It is
        ignored for every other codec. Track names must be unique across
        all `add_video` calls; a duplicate raises.

        **Byte-stream latency** (non-H264 codecs): each frame's payload is
        fragmented at the LiveKit chunk size (15 KB) and shipped over a
        single SCTP data channel. Per-frame latency is roughly `1 ms + 2 ms
        × ⌈encoded_size / 15 KB⌉` on localhost, set by the data-channel
        drain rate (not Portal's encode cost). Pick a codec whose encoded
        output fits in one chunk for low-latency closed-loop work — at
        typical inference resolutions (224×224 to 480p) MJPEG q=80–95
        usually does.
        """
        self._inner.add_video(name, codec, quality)
        if codec == VideoCodec.H264:
            self._video_tracks.append(name)
        else:
            self._frame_video_tracks.append(
                FrameVideoSpec(name=name, codec=codec, quality=quality)
            )

    def add_state_typed(self, schema: Iterable[SchemaEntry]) -> None:
        """Declare state fields with per-field dtype.

        Accepts an iterable of `(name, DType)` tuples or `FieldSpec` records.
        Order is significant and must match on both peers.
        """
        specs = _to_field_specs(schema)
        self._inner.add_state_typed(specs)
        self._state_schema.extend(specs)

    def add_action_typed(self, schema: Iterable[SchemaEntry]) -> None:
        """Declare action fields with per-field dtype.

        Accepts an iterable of `(name, DType)` tuples or `FieldSpec` records.
        Order is significant and must match on both peers.
        """
        specs = _to_field_specs(schema)
        self._inner.add_action_typed(specs)
        self._action_schema.extend(specs)

    def add_action_chunk(
        self,
        name: str,
        horizon: int,
        fields: Iterable[SchemaEntry],
    ) -> None:
        """Declare a named action chunk: a fixed-horizon batch of typed
        per-field values published as one byte stream.

        Use this for VLA policies that emit a horizon of future actions
        per inference step. Multiple chunks can be declared. Names must
        be unique. The chunk's payload uses LiveKit byte streams (not
        data packets), so it isn't bounded by the 15 KB packet limit.

        Both peers must declare the same chunks (same name, horizon, and
        ordered fields) — a fingerprint mismatch drops the packet.
        """
        specs = _to_field_specs(fields)
        self._inner.add_action_chunk(name, horizon, specs)
        self._action_chunks.append(
            ChunkSpec(name=name, horizon=horizon, fields=specs)
        )

    def set_fps(self, fps: int) -> None:
        self._inner.set_fps(fps)

    def set_slack(self, ticks: int) -> None:
        self._inner.set_slack(ticks)

    def set_tolerance(self, ticks: float) -> None:
        self._inner.set_tolerance(ticks)

    def set_state_reliable(self, reliable: bool) -> None:
        self._inner.set_state_reliable(reliable)

    def set_action_reliable(self, reliable: bool) -> None:
        self._inner.set_action_reliable(reliable)

    def set_ping_ms(self, ms: int) -> None:
        self._inner.set_ping_ms(ms)

    def set_e2ee_key(self, key: bytes) -> None:
        """Set a shared E2EE key. Both peers must call this with the same key
        before connecting. The key is used as a GCM-AES shared secret for all
        media tracks and data channels.
        """
        self._inner.set_e2ee_key(bytes(key))

    def set_reuse_stale_frames(self, enable: bool) -> None:
        """Reuse the most recent already-emitted frame on a track when the
        current state has no in-range match. Video "freezes" on the last good
        frame during loss, but state keeps flowing — every state becomes an
        observation once every track has emitted at least once.

        Off by default (strict drop-on-horizon). Turn on for data collection
        or logging where losing state is worse than a transient video freeze.
        Leave off for real-time control where a stale frame would misalign
        the perception/action loop.
        """
        self._inner.set_reuse_stale_frames(enable)

    def set_multi_controller(self, enable: bool) -> None:
        """Opt into the v0.2 multi-controller layer.

        Off by default so v0.1 callers using `Portal` directly keep working
        unchanged. When on, Portal self-sets the `lk.portal.role` attribute
        on connect, tracks operators via attribute events, gates inbound
        actions on the Robot side, and registers the
        `portal.set_active_operator` RPC. The `Robot` / `Operator` classes
        call this automatically.
        """
        self._inner.set_multi_controller(enable)

    def close(self) -> None:
        """No-op: UniFFI releases the Rust-side handle when Python GC drops
        the last reference. Kept for backwards compatibility with callers
        that explicitly `close()`.
        """
        # Drop our reference so the underlying Arc can be released.
        self._inner = None  # type: ignore[assignment]


# --- Portal -----------------------------------------------------------------

class Portal:
    """Main session object.

    Construct with a `PortalConfig`, then `await connect(url, token)`.
    Register push callbacks with `on_action / on_state / on_observation /
    on_video_frame / on_drop`.
    """

    __slots__ = (
        "_inner",
        "_dispatcher",
        "_state_fields",
        "_action_fields",
        "_state_schema",
        "_action_schema",
        "_video_tracks",
        "_action_chunks",
        "_chunk_schemas",
    )

    def __init__(self, config: PortalConfig) -> None:
        # Schema snapshots let delivery records reconstruct Python types
        # per declared dtype — the FFI boundary delivers everything as
        # `Dict[str, float]` (the core pipeline is f64 throughout).
        self._state_schema: List[FieldSpec] = list(config.state_schema)
        self._action_schema: List[FieldSpec] = list(config.action_schema)
        self._action_chunks: List[ChunkSpec] = list(config.action_chunks)
        self._chunk_schemas: Dict[str, List[FieldSpec]] = {
            spec.name: list(spec.fields) for spec in self._action_chunks
        }
        self._dispatcher = _Dispatcher(
            self._action_schema, self._state_schema, self._chunk_schemas
        )
        self._inner = _ffi.Portal(config._inner, self._dispatcher)
        # Snapshot what the Rust side confirmed it was built with.
        self._state_fields: List[str] = list(self._inner.state_fields())
        self._action_fields: List[str] = list(self._inner.action_fields())
        self._video_tracks: List[str] = list(self._inner.video_tracks())

    # -- async lifecycle -----------------------------------------------------

    async def connect(self, url: str, token: str) -> None:
        # Callbacks fire on tokio workers; hop them onto this loop. The
        # UniFFI-generated RPC handler dispatch also needs to know which
        # loop to run async foreign-trait methods on, since it's invoked
        # from a tokio worker with no asyncio loop of its own.
        loop = asyncio.get_running_loop()
        self._dispatcher.bind_loop(loop)
        _set_uniffi_event_loop(loop)
        await self._inner.connect(url, token)

    async def disconnect(self) -> None:
        await self._inner.disconnect()

    # -- send (sync, fire-and-forget) ----------------------------------------

    def send_video_frame(
        self,
        track_name: str,
        frame: Any,
        width: Optional[int] = None,
        height: Optional[int] = None,
        timestamp_us: Optional[int] = None,
    ) -> None:
        rgb, w, h = _frame.normalize_rgb(frame, width, height)
        self._inner.send_video_frame(track_name, rgb, w, h, timestamp_us)

    def send_state(
        self,
        values: Dict[str, Any],
        timestamp_us: Optional[int] = None,
    ) -> None:
        """Publish a state sample (robot role only). Each value's Python
        type must match the field's declared dtype: `True` / `False` for
        `DType.BOOL`, `int` for integer dtypes, `int` or `float` for
        float dtypes. A mismatch raises `PortalError.DtypeMismatch`
        before any packet is sent.
        """
        _validate_send_values(values, self._state_schema, "state")
        self._inner.send_state(values, timestamp_us)

    def send_action(
        self,
        values: Dict[str, Any],
        timestamp_us: Optional[int] = None,
        in_reply_to_ts_us: Optional[int] = None,
    ) -> None:
        """Publish an action (operator role only). Same validation rules
        as `send_state`.

        Pass `in_reply_to_ts_us=obs.timestamp_us` to give the receiver
        the data needed to compute true end-to-end policy latency
        (`metrics.policy.e2e_us_*`). Leave it `None` for unsolicited
        publishes (teleop, idle commands).
        """
        _validate_send_values(values, self._action_schema, "action")
        self._inner.send_action(values, timestamp_us, in_reply_to_ts_us)

    def send_action_chunk(
        self,
        chunk_name: str,
        data: Any,
        timestamp_us: Optional[int] = None,
        in_reply_to_ts_us: Optional[int] = None,
    ) -> None:
        """Publish an action chunk on the named declaration.

        `data` may be any of:
          - `Dict[str, ndarray | list[float]]` — one column per field of
            length `horizon`.
          - `numpy.ndarray` of shape `(horizon, len(fields))` and a single
            dtype — split into per-field columns by declared field order.
            Convenient for VLA policies that emit a uniform tensor.

        Wrong-length columns are zero-padded by the core. Unknown fields
        are warned-and-ignored once each.
        """
        schema = self._chunk_schemas.get(chunk_name)
        if schema is None:
            raise PortalError.UnknownChunk(f"unknown action chunk: {chunk_name}")
        ffi_data = _normalize_chunk_data(data, schema)
        self._inner.send_action_chunk(
            chunk_name, ffi_data, timestamp_us, in_reply_to_ts_us
        )

    # -- pull (sync, latest-wins) --------------------------------------------

    def get_observation(self) -> Optional[Observation]:
        raw = self._inner.get_observation()
        return None if raw is None else _wrap_observation(raw, self._state_schema)

    def get_action(self) -> Optional[Action]:
        raw = self._inner.get_action()
        return None if raw is None else _wrap_action(raw, self._action_schema)

    def get_state(self) -> Optional[State]:
        raw = self._inner.get_state()
        return None if raw is None else _wrap_state(raw, self._state_schema)

    def get_video_frame(self, track_name: str) -> Optional[VideoFrameData]:
        return self._inner.get_video_frame(track_name)

    def get_action_chunk(self, chunk_name: str) -> Optional[ActionChunk]:
        """Latest chunk received for `chunk_name`, or `None` if none has
        arrived yet (or the chunk wasn't declared).
        """
        raw = self._inner.get_action_chunk(chunk_name)
        if raw is None:
            return None
        return _wrap_action_chunk(raw, self._chunk_schemas)

    # -- push callbacks ------------------------------------------------------

    def on_action(self, callback: Callable[[Action], Any]) -> None:
        self._dispatcher.set_action(callback)

    def on_state(self, callback: Callable[[State], Any]) -> None:
        self._dispatcher.set_state(callback)

    def on_observation(self, callback: Callable[[Observation], Any]) -> None:
        self._dispatcher.set_observation(callback)

    def on_video_frame(
        self,
        track_name: str,
        callback: Callable[[str, VideoFrameData], Any],
    ) -> None:
        self._dispatcher.set_video(track_name, callback)

    def on_action_chunk(
        self,
        chunk_name: str,
        callback: Callable[[ActionChunk], Any],
    ) -> None:
        """Register a callback for the named chunk declaration. Fires on
        every chunk received for that name. Per-field columns in
        `chunk.data` are reconstructed as numpy arrays of the declared
        dtype; use `chunk.raw_data` for the f64 list view.
        """
        if chunk_name not in self._chunk_schemas:
            raise PortalError.UnknownChunk(
                f"unknown action chunk: {chunk_name}"
            )
        self._dispatcher.set_action_chunk(chunk_name, callback)

    def on_drop(
        self,
        callback: Callable[[List[Dict[str, Any]]], Any],
    ) -> None:
        """`callback(dropped)` receives a list of typed state dicts that
        couldn't be matched to a video frame. Each dict mirrors
        `observation.state` — Python-native types per the declared schema.
        """
        self._dispatcher.set_drop(callback)

    # -- rpc -----------------------------------------------------------------

    def peer_identity(self) -> Optional[str]:
        """Identity of the peer once Portal has seen any traffic from them.

        `None` before the peer has published any Portal-topic data packet
        or a subscribed video track (whichever happens first).
        """
        return self._inner.peer_identity()

    # -- multi-controller (v0.2) --------------------------------------------

    def local_identity(self) -> Optional[str]:
        """Own LiveKit identity once connected. `None` before `connect()`."""
        return self._inner.local_identity()

    def active_operator(self) -> Optional[str]:
        """Identity of the operator the robot is currently listening to,
        or `None` if no operator is selected.

        On Robot side this is the local pointer (also broadcast as the
        `lk.portal.active_operator` attribute). On Operator side this is a
        mirror of the robot's attribute, kept in sync via attribute change
        events.
        """
        return self._inner.active_operator()

    async def set_active_operator(self, identity: Optional[str]) -> None:
        """Set the active operator.

        On Robot side this updates the local pointer and broadcasts via
        the robot's own attributes. On Operator side this dispatches a
        `portal.set_active_operator` RPC to the robot. Pass `None` to
        clear and drop all incoming actions.
        """
        await self._inner.set_active_operator(identity)

    def operators(self) -> List[str]:
        """Identities of currently-connected operators (excluding self)."""
        return list(self._inner.operators())

    def robot_identity(self) -> Optional[str]:
        """Identity of the robot in the room, or `None`. Operator-side
        helper, derived from the robot's `lk.portal.role` attribute.
        """
        return self._inner.robot_identity()

    def on_operator_joined(self, callback: Callable[[str], Any]) -> None:
        """Fire when an operator joins the room."""
        self._dispatcher.set_operator_joined(callback)

    def on_operator_left(self, callback: Callable[[str], Any]) -> None:
        """Fire when an operator leaves the room. Note: the robot's
        `active_operator` pointer is **not** auto-cleared on disconnect
        — it stays pinned so a same-identity reconnect resumes control.
        """
        self._dispatcher.set_operator_left(callback)

    def on_active_operator_changed(
        self,
        callback: Callable[[Optional[str]], Any],
    ) -> None:
        """Fire when the robot's `active_operator` attribute changes (or,
        on Robot side, when the local pointer is updated). Argument is
        the new identity, or `None` if cleared.
        """
        self._dispatcher.set_active_operator_changed(callback)

    def register_rpc_method(
        self,
        method: str,
        handler: Callable[[RpcInvocationData], Any],
    ) -> None:
        """Register a handler for `method`. The handler is invoked on this
        Portal's asyncio loop whenever a peer calls `perform_rpc(method, ...)`.

        `handler` may be a regular `def` returning `str`, or an `async def`
        returning `str`. To signal an application error, `raise
        RpcError.Error(code=..., message=..., data=...)` — that will be
        serialized back to the caller.
        """
        wrapper = _RpcHandlerAdapter(handler)
        self._inner.register_rpc_method(method, wrapper)

    def unregister_rpc_method(self, method: str) -> None:
        self._inner.unregister_rpc_method(method)

    async def perform_rpc(
        self,
        method: str,
        payload: str = "",
        destination: Optional[str] = None,
        response_timeout_ms: Optional[int] = None,
    ) -> str:
        """Invoke `method` on the peer. When `destination` is omitted,
        Portal routes to the identified peer (see `peer_identity`),
        falling back to the single remote participant if none is
        identified yet. Returns the handler's string payload.
        """
        return await self._inner.perform_rpc(
            destination,
            method,
            payload,
            response_timeout_ms,
        )

    # -- metrics -------------------------------------------------------------

    def metrics(self) -> PortalMetrics:
        return self._inner.metrics()

    def reset_metrics(self) -> None:
        self._inner.reset_metrics()

    # -- cleanup -------------------------------------------------------------

    def close(self) -> None:
        """No-op: UniFFI releases the Rust-side handle when Python GC drops
        the last reference. Kept for backwards compatibility."""
        self._inner = None  # type: ignore[assignment]


# ---------------------------------------------------------------------------
# v0.2 role-specific config + classes.
#
# These are thin wrappers around the unified `Portal` / `PortalConfig` that
# hide the wrong-role methods and split the config types. The Rust core stays
# unified; both `Robot` and `Operator` instantiate the same `Portal` class
# under the hood. `Portal` / `PortalConfig` / `Role` remain exported for v0.1
# compatibility.
# ---------------------------------------------------------------------------


class _RoleConfigBase:
    """Shared declaration surface for `RobotConfig` and `OperatorConfig`.

    Holds an internal `PortalConfig` keyed to the appropriate `Role`. All
    declarative methods (schemas, video tracks, action chunks, tuning knobs)
    forward straight through. Subclasses don't extend this surface; they
    only differ in which role they construct.
    """

    __slots__ = ("_inner",)

    def __init__(self, session: str, role: Role) -> None:
        self._inner = PortalConfig(session, role)
        # `Robot` / `Operator` opt into the v0.2 multi-controller layer
        # unconditionally. Users who want the v0.1 single-peer behavior keep
        # using the unified `Portal` / `PortalConfig` classes.
        self._inner.set_multi_controller(True)

    @property
    def session(self) -> str:
        return self._inner.session

    @property
    def role(self) -> Role:
        return self._inner.role

    @property
    def video_tracks(self) -> List[str]:
        return self._inner.video_tracks

    @property
    def frame_video_tracks(self) -> List[FrameVideoSpec]:
        return self._inner.frame_video_tracks

    @property
    def state_schema(self) -> List[FieldSpec]:
        return self._inner.state_schema

    @property
    def action_schema(self) -> List[FieldSpec]:
        return self._inner.action_schema

    @property
    def action_chunks(self) -> List[ChunkSpec]:
        return self._inner.action_chunks

    def add_video(
        self,
        name: str,
        codec: VideoCodec = VideoCodec.H264,
        quality: int = DEFAULT_MJPEG_QUALITY,
    ) -> None:
        self._inner.add_video(name, codec, quality)

    def add_state_typed(self, schema: Iterable[SchemaEntry]) -> None:
        self._inner.add_state_typed(schema)

    def add_action_typed(self, schema: Iterable[SchemaEntry]) -> None:
        self._inner.add_action_typed(schema)

    def add_action_chunk(
        self,
        name: str,
        horizon: int,
        fields: Iterable[SchemaEntry],
    ) -> None:
        self._inner.add_action_chunk(name, horizon, fields)

    def set_fps(self, fps: int) -> None:
        self._inner.set_fps(fps)

    def set_slack(self, ticks: int) -> None:
        self._inner.set_slack(ticks)

    def set_tolerance(self, ticks: float) -> None:
        self._inner.set_tolerance(ticks)

    def set_state_reliable(self, reliable: bool) -> None:
        self._inner.set_state_reliable(reliable)

    def set_action_reliable(self, reliable: bool) -> None:
        self._inner.set_action_reliable(reliable)

    def set_ping_ms(self, ms: int) -> None:
        self._inner.set_ping_ms(ms)

    def set_e2ee_key(self, key: bytes) -> None:
        self._inner.set_e2ee_key(key)

    def set_reuse_stale_frames(self, enable: bool) -> None:
        self._inner.set_reuse_stale_frames(enable)


class RobotConfig(_RoleConfigBase):
    """Robot-side session config. Same declarative surface as
    `OperatorConfig`; the Role is pinned to `Role.ROBOT` internally.
    """

    def __init__(self, session: str) -> None:
        super().__init__(session, Role.ROBOT)


class OperatorConfig(_RoleConfigBase):
    """Operator-side session config. Same declarative surface as
    `RobotConfig`; the Role is pinned to `Role.OPERATOR`.

    `identity` is informational. The actual LiveKit participant identity
    comes from the access token. Setting it here lets your own code know
    which identity it is supposed to claim, but does not validate against
    the token. Defaults to `None`; callers that want a stable identity
    should generate one and use it for both `OperatorConfig.identity` and
    the token mint.
    """

    __slots__ = ("identity",)

    def __init__(self, session: str, identity: Optional[str] = None) -> None:
        super().__init__(session, Role.OPERATOR)
        self.identity = identity


class Robot:
    """Robot-side Portal facade.

    Wraps a `Portal` instance constructed with `Role.ROBOT` and exposes only
    the methods that make sense on the robot side (publish state and video,
    receive actions and chunks, control plane). Callers who want the
    unified surface can keep using `Portal` directly.
    """

    __slots__ = ("_portal",)

    def __init__(self, config: RobotConfig) -> None:
        self._portal = Portal(config._inner)

    # -- lifecycle -----------------------------------------------------------

    async def connect(self, url: str, token: str) -> None:
        await self._portal.connect(url, token)

    async def disconnect(self) -> None:
        await self._portal.disconnect()

    def close(self) -> None:
        self._portal.close()

    # -- publish (robot-side) ------------------------------------------------

    def send_video_frame(
        self,
        track_name: str,
        frame: Any,
        width: Optional[int] = None,
        height: Optional[int] = None,
        timestamp_us: Optional[int] = None,
    ) -> None:
        self._portal.send_video_frame(track_name, frame, width, height, timestamp_us)

    def send_state(
        self,
        values: Dict[str, Any],
        timestamp_us: Optional[int] = None,
    ) -> None:
        self._portal.send_state(values, timestamp_us)

    # -- receive (robot-side) ------------------------------------------------

    def on_action(self, callback: Callable[[Action], Any]) -> None:
        self._portal.on_action(callback)

    def on_action_chunk(
        self,
        chunk_name: str,
        callback: Callable[[ActionChunk], Any],
    ) -> None:
        self._portal.on_action_chunk(chunk_name, callback)

    def get_action(self) -> Optional[Action]:
        return self._portal.get_action()

    def get_action_chunk(self, chunk_name: str) -> Optional[ActionChunk]:
        return self._portal.get_action_chunk(chunk_name)

    # -- multi-controller ----------------------------------------------------

    def local_identity(self) -> Optional[str]:
        return self._portal.local_identity()

    def active_operator(self) -> Optional[str]:
        return self._portal.active_operator()

    async def set_active_operator(self, identity: Optional[str]) -> None:
        await self._portal.set_active_operator(identity)

    def operators(self) -> List[str]:
        return self._portal.operators()

    def on_operator_joined(self, callback: Callable[[str], Any]) -> None:
        self._portal.on_operator_joined(callback)

    def on_operator_left(self, callback: Callable[[str], Any]) -> None:
        self._portal.on_operator_left(callback)

    def on_active_operator_changed(
        self,
        callback: Callable[[Optional[str]], Any],
    ) -> None:
        self._portal.on_active_operator_changed(callback)

    # -- rpc -----------------------------------------------------------------

    def register_rpc_method(
        self,
        method: str,
        handler: Callable[[RpcInvocationData], Any],
    ) -> None:
        self._portal.register_rpc_method(method, handler)

    def unregister_rpc_method(self, method: str) -> None:
        self._portal.unregister_rpc_method(method)

    async def perform_rpc(
        self,
        method: str,
        payload: str = "",
        destination: Optional[str] = None,
        response_timeout_ms: Optional[int] = None,
    ) -> str:
        return await self._portal.perform_rpc(
            method, payload, destination, response_timeout_ms
        )

    # -- metrics -------------------------------------------------------------

    def metrics(self) -> PortalMetrics:
        return self._portal.metrics()

    def reset_metrics(self) -> None:
        self._portal.reset_metrics()


class Operator:
    """Operator-side Portal facade.

    Wraps a `Portal` instance constructed with `Role.OPERATOR` and exposes
    only the methods that make sense on the operator side (publish actions
    and chunks, receive observations and video, control plane). Callers who
    want the unified surface can keep using `Portal` directly.
    """

    __slots__ = ("_portal", "_identity_hint")

    def __init__(self, config: OperatorConfig) -> None:
        self._portal = Portal(config._inner)
        self._identity_hint = config.identity

    # -- lifecycle -----------------------------------------------------------

    async def connect(self, url: str, token: str) -> None:
        await self._portal.connect(url, token)

    async def disconnect(self) -> None:
        await self._portal.disconnect()

    def close(self) -> None:
        self._portal.close()

    # -- publish (operator-side) ---------------------------------------------

    def send_action(
        self,
        values: Dict[str, Any],
        timestamp_us: Optional[int] = None,
        in_reply_to_ts_us: Optional[int] = None,
    ) -> None:
        self._portal.send_action(values, timestamp_us, in_reply_to_ts_us)

    def send_action_chunk(
        self,
        chunk_name: str,
        data: Any,
        timestamp_us: Optional[int] = None,
        in_reply_to_ts_us: Optional[int] = None,
    ) -> None:
        self._portal.send_action_chunk(
            chunk_name, data, timestamp_us, in_reply_to_ts_us
        )

    # -- receive (operator-side) ---------------------------------------------

    def on_state(self, callback: Callable[[State], Any]) -> None:
        self._portal.on_state(callback)

    def on_observation(self, callback: Callable[[Observation], Any]) -> None:
        self._portal.on_observation(callback)

    def on_video_frame(
        self,
        track_name: str,
        callback: Callable[[str, VideoFrameData], Any],
    ) -> None:
        self._portal.on_video_frame(track_name, callback)

    def on_drop(
        self,
        callback: Callable[[List[Dict[str, Any]]], Any],
    ) -> None:
        self._portal.on_drop(callback)

    def get_state(self) -> Optional[State]:
        return self._portal.get_state()

    def get_observation(self) -> Optional[Observation]:
        return self._portal.get_observation()

    def get_video_frame(self, track_name: str) -> Optional[VideoFrameData]:
        return self._portal.get_video_frame(track_name)

    # -- multi-controller ----------------------------------------------------

    def local_identity(self) -> Optional[str]:
        return self._portal.local_identity()

    @property
    def identity_hint(self) -> Optional[str]:
        """Identity supplied to `OperatorConfig`, if any. Informational —
        the LiveKit participant identity comes from the token, not this
        field. Use `local_identity()` for the actual connected identity.
        """
        return self._identity_hint

    def active_operator(self) -> Optional[str]:
        return self._portal.active_operator()

    async def set_active_operator(self, identity: Optional[str]) -> None:
        await self._portal.set_active_operator(identity)

    def operators(self) -> List[str]:
        return self._portal.operators()

    def robot_identity(self) -> Optional[str]:
        return self._portal.robot_identity()

    def on_operator_joined(self, callback: Callable[[str], Any]) -> None:
        self._portal.on_operator_joined(callback)

    def on_operator_left(self, callback: Callable[[str], Any]) -> None:
        self._portal.on_operator_left(callback)

    def on_active_operator_changed(
        self,
        callback: Callable[[Optional[str]], Any],
    ) -> None:
        self._portal.on_active_operator_changed(callback)

    # -- rpc -----------------------------------------------------------------

    def register_rpc_method(
        self,
        method: str,
        handler: Callable[[RpcInvocationData], Any],
    ) -> None:
        self._portal.register_rpc_method(method, handler)

    def unregister_rpc_method(self, method: str) -> None:
        self._portal.unregister_rpc_method(method)

    async def perform_rpc(
        self,
        method: str,
        payload: str = "",
        destination: Optional[str] = None,
        response_timeout_ms: Optional[int] = None,
    ) -> str:
        return await self._portal.perform_rpc(
            method, payload, destination, response_timeout_ms
        )

    # -- metrics -------------------------------------------------------------

    def metrics(self) -> PortalMetrics:
        return self._portal.metrics()

    def reset_metrics(self) -> None:
        self._portal.reset_metrics()


__all__ = [
    "Role",
    "DType",
    "VideoCodec",
    "FieldSpec",
    "FrameVideoSpec",
    "ChunkSpec",
    "TypedScalar",
    "DEFAULT_MJPEG_QUALITY",
    "PortalConfig",
    "Portal",
    "RobotConfig",
    "OperatorConfig",
    "Robot",
    "Operator",
    "Observation",
    "Action",
    "ActionChunk",
    "State",
    "VideoFrameData",
    "PortalMetrics",
    "SyncMetrics",
    "TransportMetrics",
    "BufferMetrics",
    "RttMetrics",
    "PolicyMetrics",
    "PortalError",
    "RpcInvocationData",
    "RpcError",
    "frame_bytes_to_numpy_rgb",
]
