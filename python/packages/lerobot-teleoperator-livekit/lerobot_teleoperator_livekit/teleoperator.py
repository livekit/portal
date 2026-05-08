"""LiveKit Portal teleoperator implementation.

Runs on the physical robot. Opens a Portal `Robot` session, publishing
camera frames + state via ``send_feedback(...)`` and surfacing received
actions via ``get_action()``. When constructed with a local lerobot
``Robot`` instance, it introspects ``observation_features`` /
``action_features`` to derive motor and camera shapes automatically — the
physical robot stays in the user's loop; the plugin just brokers the wire.
"""
from __future__ import annotations

import asyncio
import logging
import threading
from dataclasses import dataclass
from typing import Any

import numpy as np
from lerobot.teleoperators.config import TeleoperatorConfig
from lerobot.teleoperators.teleoperator import Teleoperator

from livekit.portal import (
    DEFAULT_MJPEG_QUALITY,
    DType,
    Robot as PortalRobot,
    RobotConfig as PortalRobotConfig,
    VideoCodec,
)

from ._utils import split_observation_features

logger = logging.getLogger(__name__)


@TeleoperatorConfig.register_subclass("livekit")
@dataclass
class LiveKitTeleoperatorConfig(TeleoperatorConfig):
    url: str = ""
    token: str = ""
    session: str = ""
    fps: int = 30

    # Explicit-mode fallbacks when no local Robot is passed to the constructor
    # (e.g. the ``--teleop.type=livekit`` CLI path, which can't pass instances).
    # Ignored when a robot is provided.
    motors: tuple[str, ...] = ()
    camera_names: tuple[str, ...] = ()

    # Video transport. `H264` rides the WebRTC media path (default).
    # `MJPEG` / `PNG` / `RAW` ride a reliable per-frame byte stream — pick
    # one of those for policy-grade pixels. `video_quality` is honored only
    # for `MJPEG`. Both peers must agree, so set the same values on the
    # robot's `LiveKitTeleoperatorConfig` and the operator's
    # `LiveKitRobotConfig`.
    video_codec: VideoCodec = VideoCodec.H264
    video_quality: int = DEFAULT_MJPEG_QUALITY

    # Portal tuning.
    slack: int | None = None
    tolerance: float | None = None
    state_reliable: bool = True
    action_reliable: bool = True
    reuse_stale_frames: bool = False


class LiveKitTeleoperator(Teleoperator):
    """lerobot Teleoperator that forwards actions over a Portal session.

    Construct with an optional local ``Robot`` instance; its features
    determine the motor keys and camera list used by the network layer.
    Without a robot, falls back to ``config.motors`` + ``config.camera_names``.

    Typical use on the robot side::

        robot = MyPhysicalRobot(...)
        teleop = LiveKitTeleoperator(cfg, robot=robot)
        teleop.connect()
        while running:
            obs = robot.get_observation()
            teleop.send_feedback(obs)
            action = teleop.get_action()
            if action:
                robot.send_action(action)
    """

    config_class = LiveKitTeleoperatorConfig
    name = "livekit"

    def __init__(
        self,
        config: LiveKitTeleoperatorConfig,
        robot: Any | None = None,
    ) -> None:
        super().__init__(config)
        self.config = config

        self._state_keys, self._action_keys, self._cameras = self._resolve_schema(
            config, robot
        )
        # Portal's state/action fields are the raw motor names (without the
        # ".pos" suffix); lerobot observation/action dicts use the suffix.
        self._state_motors = [_strip_pos(k) for k in self._state_keys]
        self._action_motors = [_strip_pos(k) for k in self._action_keys]
        self._camera_names = list(self._cameras.keys())

        self._act_features: dict = {k: float for k in self._action_keys}
        self._feedback_features: dict = {k: float for k in self._state_keys}
        for name, shape in self._cameras.items():
            self._feedback_features[name] = shape

        self._portal: PortalRobot | None = None
        self._portal_cfg: PortalRobotConfig | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._loop_thread: threading.Thread | None = None
        self._connected = False

    # -- lerobot interface ----------------------------------------------------

    @property
    def action_features(self) -> dict:
        return self._act_features

    @property
    def feedback_features(self) -> dict:
        return self._feedback_features

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
                "LiveKitTeleoperatorConfig.url and .token are required; mint"
                " a robot-side token with can_update_own_metadata=True before"
                " calling connect()."
            )

        self._start_loop()

        self._portal_cfg = PortalRobotConfig(self.config.session or "lerobot")
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

        self._portal = PortalRobot(self._portal_cfg)
        self._run(self._portal.connect(self.config.url, self.config.token))
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

    def get_action(self) -> dict[str, Any]:
        """Latest action received from the operator, keyed like the local
        robot's ``action_features``. Empty dict if nothing has arrived."""
        if self._portal is None:
            return {}
        action = self._portal.get_action()
        if action is None:
            return {}
        return {
            k: float(action.values.get(m, 0.0))
            for k, m in zip(self._action_keys, self._action_motors)
        }

    def send_feedback(self, feedback: dict[str, Any]) -> None:
        """Publish the local robot's observation to the operator.

        ``feedback`` is the same dict shape returned by
        ``robot.get_observation()``: motor keys (e.g. ``"shoulder.pos"``) map
        to floats, camera keys (matching those in ``observation_features``)
        map to ``np.ndarray(H, W, 3)`` uint8 RGB. Unknown keys are ignored.
        """
        if self._portal is None or not self._connected:
            return

        if self._state_motors:
            state: dict[str, float] = {}
            for key, motor in zip(self._state_keys, self._state_motors):
                if key in feedback:
                    state[motor] = float(feedback[key])
            if state:
                self._portal.send_state(state)

        for cam in self._camera_names:
            frame = feedback.get(cam)
            if frame is None:
                continue
            if not isinstance(frame, np.ndarray):
                raise TypeError(
                    f"camera feedback '{cam}' must be np.ndarray (H, W, 3)"
                    f" uint8 RGB; got {type(frame).__name__}"
                )
            self._portal.send_video_frame(cam, frame)

        if logger.isEnabledFor(logging.DEBUG):
            known = set(self._state_keys) | set(self._camera_names)
            skipped = [k for k in feedback if k not in known]
            if skipped:
                logger.debug("send_feedback: skipped %d unknown key(s): %s", len(skipped), skipped)

    # -- schema resolution ---------------------------------------------------

    @staticmethod
    def _resolve_schema(
        config: LiveKitTeleoperatorConfig,
        robot: Any | None,
    ) -> tuple[list[str], list[str], dict[str, tuple[int, ...]]]:
        if robot is not None:
            obs_features = dict(getattr(robot, "observation_features", {}))
            act_features = dict(getattr(robot, "action_features", {}))
            if not obs_features and not act_features:
                raise ValueError(
                    "local robot has empty observation_features /"
                    " action_features; cannot infer schema"
                )
            state_keys, cameras = split_observation_features(obs_features)
            action_keys = sorted(act_features.keys())
            return state_keys, action_keys, cameras

        if config.motors or config.camera_names:
            state_keys = sorted(f"{m}.pos" for m in config.motors)
            action_keys = list(state_keys)
            # Shape is metadata only; Portal accepts any resolution at runtime.
            cameras = {name: () for name in config.camera_names}
            return state_keys, action_keys, cameras

        raise ValueError(
            "LiveKitTeleoperator needs either a local Robot instance or"
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
