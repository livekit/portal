"""LiveKit Portal robot implementation.

Runs on the operator side. Opens a Portal `Operator` session and presents
the remote physical robot as a local lerobot ``Robot``. When constructed
with a local lerobot ``Teleoperator`` (e.g. a leader arm, a gamepad), it
introspects ``action_features`` to derive motor keys automatically. the
local teleop stays in the user's loop and generates the actions that
LiveKitRobot forwards over the wire.
"""
from __future__ import annotations

import asyncio
import logging
import threading
from dataclasses import dataclass
from typing import Any

from lerobot.robots.config import RobotConfig
from lerobot.robots.robot import Robot

from livekit.portal import (
    DEFAULT_MJPEG_QUALITY,
    DType,
    Operator as PortalOperator,
    OperatorConfig as PortalOperatorConfig,
    VideoCodec,
    frame_bytes_to_numpy_rgb,
)

from ._utils import split_observation_features

_log = logging.getLogger(__name__)


@RobotConfig.register_subclass("livekit")
@dataclass
class LiveKitRobotConfig(RobotConfig):
    url: str = ""
    token: str = ""
    session: str = ""
    # Operator-side identity. Used by Portal's multi-controller layer to
    # route the active-operator pointer. Defaults to ``"operator"`` to
    # preserve the v0.1 single-operator UX. Set explicitly when running
    # multiple operators (HITL with a human + a policy, ensemble policies,
    # etc.) so each gets a unique identity.
    identity: str = "operator"
    # Auto-claim control after connect via ``set_active_operator(identity)``.
    # On for the convenience single-op case so existing lerobot scripts keep
    # working unchanged. Turn off in HITL setups where another participant
    # (human teleoperator, supervisor UI) decides who has control.
    auto_claim_control: bool = True
    fps: int = 30

    # Explicit-mode fallbacks used only when no local Teleoperator is passed.
    motors: tuple[str, ...] = ()
    camera_names: tuple[str, ...] = ()
    camera_height: int = 480
    camera_width: int = 640

    # Video transport. `H264` rides the WebRTC media path (default).
    # `MJPEG` / `PNG` / `RAW` ride a reliable per-frame byte stream — pick
    # one of those for policy-grade pixels. `video_quality` is honored only
    # for `MJPEG`. Both peers must agree, so set the same values on the
    # operator's `LiveKitRobotConfig` and the robot's
    # `LiveKitTeleoperatorConfig`.
    video_codec: VideoCodec = VideoCodec.H264
    video_quality: int = DEFAULT_MJPEG_QUALITY

    # Portal tuning.
    slack: int | None = None
    tolerance: float | None = None
    state_reliable: bool = True
    action_reliable: bool = True
    reuse_stale_frames: bool = False

    # Full observation schema when the remote robot reports state beyond the
    # action schema (e.g. {"shoulder.pos": float, "slider.pos": float}).
    # Mirrors lerobot's observation_features convention: scalar keys map to a
    # Python type; camera keys map to a shape tuple. When provided this
    # replaces the default "state mirrors action" assumption entirely.
    observation_features: dict | None = None


class LiveKitRobot(Robot):
    """lerobot Robot that receives synced observations from a remote physical
    robot over a Portal session and publishes actions back to it.

    Construct with an optional local ``Teleoperator`` instance; its
    ``action_features`` determine the motor keys used over the wire. State
    is assumed to mirror the action schema (standard lerobot convention),
    and camera names must be provided separately — the local teleop doesn't
    know what cameras the remote robot has.

    Typical operator-side use::

        leader = MyLeaderArmTeleop(...)
        robot = LiveKitRobot(cfg, teleop=leader)
        robot.connect()
        while running:
            obs = robot.get_observation()
            action = leader.get_action()
            robot.send_action(action)
    """

    config_class = LiveKitRobotConfig
    name = "livekit"

    def __init__(
        self,
        config: LiveKitRobotConfig,
        teleop: Any | None = None,
    ) -> None:
        super().__init__(config)
        self.config = config

        self._state_keys, self._action_keys, self._cameras = self._resolve_schema(
            config, teleop
        )
        self._state_motors = [_strip_pos(k) for k in self._state_keys]
        self._action_motors = [_strip_pos(k) for k in self._action_keys]
        self._camera_names = list(self._cameras.keys())

        if config.observation_features:
            self._obs_features = dict(config.observation_features)
            for name, shape in self._cameras.items():
                self._obs_features.setdefault(name, shape)
        else:
            self._obs_features = {k: float for k in self._state_keys}
            for name, shape in self._cameras.items():
                self._obs_features[name] = shape
        self._act_features: dict = {k: float for k in self._action_keys}

        self._portal: PortalOperator | None = None
        self._portal_cfg: PortalOperatorConfig | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._loop_thread: threading.Thread | None = None
        self._connected = False
        self._last_observation_timestamp_us: int | None = None
        self._schema_mismatch_warned = False

    # -- lerobot interface ----------------------------------------------------

    @property
    def observation_features(self) -> dict:
        return self._obs_features

    @property
    def action_features(self) -> dict:
        return self._act_features

    @property
    def is_connected(self) -> bool:
        return self._connected

    @property
    def is_calibrated(self) -> bool:
        return True

    def calibrate(self) -> None:
        pass

    def configure(self) -> None:
        pass

    def connect(self, calibrate: bool = True) -> None:
        if self._connected:
            return
        if not self.config.url or not self.config.token:
            raise RuntimeError(
                "LiveKitRobotConfig.url and .token are required; mint an"
                " operator-side token with can_update_own_metadata=True"
                " before calling connect()."
            )

        self._start_loop()

        self._portal_cfg = PortalOperatorConfig(
            self.config.session or "lerobot",
            identity=self.config.identity,
        )
        for cam in self._camera_names:
            self._portal_cfg.add_video(
                cam,
                codec=self.config.video_codec,
                quality=self.config.video_quality,
            )
        if self._state_motors:
            self._portal_cfg.add_state_typed(
                [(name, DType.F64) for name in self._state_motors]
            )
        if self._action_motors:
            self._portal_cfg.add_action_typed(
                [(name, DType.F64) for name in self._action_motors]
            )
        self._portal_cfg.set_fps(self.config.fps)
        if self.config.slack is not None:
            self._portal_cfg.set_slack(self.config.slack)
        if self.config.tolerance is not None:
            self._portal_cfg.set_tolerance(self.config.tolerance)
        self._portal_cfg.set_state_reliable(self.config.state_reliable)
        self._portal_cfg.set_action_reliable(self.config.action_reliable)
        self._portal_cfg.set_reuse_stale_frames(self.config.reuse_stale_frames)

        self._portal = PortalOperator(self._portal_cfg)
        self._run(self._portal.connect(self.config.url, self.config.token))
        if self.config.auto_claim_control:
            # Claim ourselves as the active operator so the robot accepts our
            # actions. Without this the Portal robot drops every action because
            # `active_operator` defaults to None. HITL or multi-operator setups
            # disable this and let a separate participant arbitrate control.
            local_id = self._portal.local_identity()
            if local_id is not None:
                self._run(self._portal.set_active_operator(local_id))
        self._connected = True

    def disconnect(self) -> None:
        if not self._connected:
            return
        try:
            if self._portal is not None:
                self._run(self._portal.disconnect())
        finally:
            if self._portal is not None:
                self._portal.close()
                self._portal = None
            self._portal_cfg = None
            self._stop_loop()
            self._connected = False

    def get_observation(self) -> dict[str, Any]:
        """Latest synced observation from the remote robot, shaped for
        lerobot (``{motor}.pos -> float``, ``{camera} -> np.ndarray(H,W,3)``
        uint8 RGB). Empty dict until the first observation syncs.

        The sender-side timestamp of this observation is available via
        :attr:`last_observation_timestamp_us` after the call returns.
        """
        if self._portal is None:
            return {}
        obs = self._portal.get_observation()
        if obs is None:
            return {}
        self._last_observation_timestamp_us = obs.timestamp_us
        out: dict[str, Any] = {}
        for key, motor in zip(self._state_keys, self._state_motors):
            if motor in obs.state:
                out[key] = float(obs.state[motor])
        if not self._schema_mismatch_warned and self._state_motors and obs.state:
            received = set(obs.state.keys())
            expected = set(self._state_motors)
            if received != expected:
                missing = sorted(expected - received)
                unexpected = sorted(received - expected)
                _log.warning(
                    "State schema mismatch: operator expects %s but robot"
                    " sent %s (missing=%s, unexpected=%s). Check that both"
                    " sides declare matching state keys.",
                    sorted(expected),
                    sorted(received),
                    missing,
                    unexpected,
                )
                self._schema_mismatch_warned = True
        for cam in self._camera_names:
            frame = obs.frames.get(cam)
            if frame is not None:
                out[cam] = frame_bytes_to_numpy_rgb(
                    frame.data, frame.width, frame.height
                )
        return out

    @property
    def last_observation_timestamp_us(self) -> int | None:
        """Sender's system time in µs (epoch) for the most recent observation
        returned by :meth:`get_observation`, or ``None`` if none yet."""
        return self._last_observation_timestamp_us

    def metrics(self):
        """Snapshot of the underlying Portal's metrics (RTT, sync delta, jitter,
        buffer fill, drops). Returns ``None`` when disconnected."""
        if self._portal is None:
            return None
        return self._portal.metrics()

    def send_action(self, action: dict[str, Any]) -> dict[str, Any]:
        """Publish an action to the remote robot. Returns ``action`` unchanged
        so callers can record it."""
        if self._portal is None or not self._connected:
            return action
        values: dict[str, float] = {}
        for key, motor in zip(self._action_keys, self._action_motors):
            if key in action:
                values[motor] = float(action[key])
        if values:
            self._portal.send_action(values)
        return action

    # -- schema resolution ---------------------------------------------------

    @staticmethod
    def _resolve_schema(
        config: LiveKitRobotConfig,
        teleop: Any | None,
    ) -> tuple[list[str], list[str], dict[str, tuple[int, ...]]]:
        camera_shape = (config.camera_height, config.camera_width, 3)
        cameras = {name: camera_shape for name in config.camera_names}

        # When observation_features is provided it is the authoritative state
        # schema — same pattern as LiveKitTeleoperator using robot.observation_features.
        if config.observation_features:
            obs_state_keys, obs_cameras = split_observation_features(
                config.observation_features
            )
            cameras = {**cameras, **obs_cameras}
            if teleop is not None:
                act_features = dict(getattr(teleop, "action_features", {}))
                if not act_features:
                    raise ValueError(
                        "local teleop has empty action_features; cannot infer"
                        " schema"
                    )
                return obs_state_keys, sorted(act_features.keys()), cameras
            if config.motors:
                return obs_state_keys, sorted(f"{m}.pos" for m in config.motors), cameras
            return obs_state_keys, obs_state_keys, cameras

        if teleop is not None:
            act_features = dict(getattr(teleop, "action_features", {}))
            if not act_features:
                raise ValueError(
                    "local teleop has empty action_features; cannot infer"
                    " schema"
                )
            action_keys = sorted(act_features.keys())
            # lerobot convention: observation mirrors action for telemetry.
            return list(action_keys), action_keys, cameras

        if config.motors or config.camera_names:
            state_keys = sorted(f"{m}.pos" for m in config.motors)
            return state_keys, list(state_keys), cameras

        raise ValueError(
            "LiveKitRobot needs either a local Teleoperator instance or"
            " config.motors / config.camera_names to derive its schema"
        )

    # -- background loop plumbing --------------------------------------------

    def _start_loop(self) -> None:
        self._loop = asyncio.new_event_loop()
        started = threading.Event()

        def _runner() -> None:
            asyncio.set_event_loop(self._loop)
            started.set()
            self._loop.run_forever()

        self._loop_thread = threading.Thread(
            target=_runner, name="livekit-portal-loop", daemon=True
        )
        self._loop_thread.start()
        started.wait()

    def _stop_loop(self) -> None:
        if self._loop is None:
            return
        self._loop.call_soon_threadsafe(self._loop.stop)
        if self._loop_thread is not None:
            self._loop_thread.join(timeout=5.0)
        self._loop.close()
        self._loop = None
        self._loop_thread = None

    def _run(self, coro):
        assert self._loop is not None, "background loop not started"
        return asyncio.run_coroutine_threadsafe(coro, self._loop).result()


def _strip_pos(key: str) -> str:
    return key[: -len(".pos")] if key.endswith(".pos") else key
