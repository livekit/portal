"""Policy side. Runs on Modal.

A normal inference operator: connect, claim control, and on every observation
call `policy.get_action(obs)` and publish the result. The only twist is that
our policy reads a QR code instead of running a model, so the tutorial is about
running a policy on Modal, not about weights.

The `Policy` class matches a LeRobot policy's call shape:

    policy = ACTPolicy.from_pretrained("your-org/your-checkpoint")
    action = policy.select_action(observation)

Swap `Policy` for a real one and the loop below does not change.

    uv run modal run policy.py     # on Modal
    uv run python policy.py        # locally, for a quick check
"""
from __future__ import annotations

import asyncio
import datetime
import os
import pathlib
import time
from typing import Optional

import modal
from livekit import api
from livekit.portal import (
    Observation, Operator, OperatorConfig, State, frame_bytes_to_numpy_rgb,
)

TRACK = "cam1"
ROOM = "portal-modal-mock"
DURATION_S = 30


def config_path() -> pathlib.Path:
    """portal.yaml, next to this file locally or mounted at /root on Modal."""
    here = pathlib.Path(__file__).parent / "portal.yaml"
    return here if here.exists() else pathlib.Path("/root/portal.yaml")


def connect(identity: str):
    """Mint a room token from the LiveKit creds in the environment.

    On Modal the secret sets LIVEKIT_URL / KEY / SECRET as env vars. For a
    local run, export them first (for example: `set -a; . .env; set +a`).
    """
    grants = api.VideoGrants(
        room_join=True, room=ROOM, can_publish=True,
        can_subscribe=True, can_update_own_metadata=True,
    )
    token = (
        api.AccessToken(os.environ["LIVEKIT_API_KEY"], os.environ["LIVEKIT_API_SECRET"])
        .with_identity(identity).with_grants(grants)
        .with_ttl(datetime.timedelta(hours=1)).to_jwt()
    )
    return os.environ["LIVEKIT_URL"], token


class Policy:
    """Stand-in for a LeRobot policy. It reads the QR clock out of the frame."""

    def __init__(self) -> None:
        import cv2

        self._cv2 = cv2
        self._detector = cv2.QRCodeDetector()

    def get_action(self, obs: Observation) -> Optional[dict]:
        """The decoded token, or None if the QR was unreadable this frame."""
        frame = obs.frames.get(TRACK)
        if frame is None:
            return None
        rgb = frame_bytes_to_numpy_rgb(bytes(frame.data), frame.width, frame.height)
        gray = self._cv2.cvtColor(rgb, self._cv2.COLOR_RGB2GRAY)
        payload, _points, _straight = self._detector.detectAndDecode(gray)
        if not payload.startswith("p1:"):
            return None
        seq, capture_us = payload[3:].split(",")
        return {"seq": float(seq), "t_capture_us": float(capture_us)}


async def main() -> None:
    url, token = connect("policy")

    cfg = OperatorConfig.from_yaml_file(config_path(), ROOM)
    op = Operator(cfg)
    policy = Policy()
    hits = misses = 0

    # When each raw state packet arrived, by seq, on our own clock. Portal pairs
    # a frame and a state by the capture time the robot stamped, so the video's
    # extra delay never shows up as a timestamp. Instead the fused observation
    # fires later than the state, once the matching frame lands. obs-fire minus
    # state-arrival is that extra video delay, one way.
    state_arrival: dict[int, float] = {}

    def on_state(state: State) -> None:
        state_arrival[int(state.values["seq"])] = time.monotonic()

    def on_observation(obs: Observation) -> None:
        nonlocal hits, misses
        action = policy.get_action(obs)   # QR decode is fast, so run it inline
        if action is None:
            misses += 1
            return
        arrived = state_arrival.pop(int(action["seq"]), None)
        lag_us = int((time.monotonic() - arrived) * 1_000_000) if arrived is not None else 0
        action["codec_lag_us"] = float(lag_us)
        # in_reply_to_ts_us feeds Portal's built-in e2e metric the QR capture
        # time, which turns that metric into a glass-to-glass number.
        op.send_action(action, in_reply_to_ts_us=int(action["t_capture_us"]))
        hits += 1

    op.on_state(on_state)
    op.on_observation(on_observation)

    print(f"[policy] connecting to {url} in room '{ROOM}' ...")
    await op.connect(url, token)

    # Claim control, or the robot's gate drops our actions.
    await op.set_active_operator(op.local_identity())
    print(f"[policy] connected and controlling as '{op.local_identity()}'")

    stop_at = time.monotonic() + DURATION_S + 5.0   # outlast the robot a little
    try:
        while time.monotonic() < stop_at:
            await asyncio.sleep(1.0)
            total = hits + misses
            rate = (hits / total * 100.0) if total else 0.0
            print(f"[policy] decoded={hits} missed={misses} decode_rate={rate:.0f}%")
    finally:
        print(f"[policy] decoded {hits} frames, disconnecting...")
        await op.disconnect()
        op.close()


# --- Deploy to Modal ----------------------------------------------------------
# A Portal operator only dials out to LiveKit, so it drops straight onto Modal.
# livekit-portal ships on PyPI, so the image is a plain pip install. The two apt
# packages are OpenCV's shared libs. This mock is CPU-only; for a real model add
# gpu="A10G" here and pip-install your stack.
app = modal.App("portal-modal-mock")
image = (
    modal.Image.debian_slim(python_version="3.12")
    .apt_install("libgl1", "libglib2.0-0")
    .pip_install(
        "livekit-portal", "livekit-api>=0.7",
        "numpy>=1.24", "opencv-python-headless>=4.9",
    )
    .add_local_python_source("policy")
    .add_local_file(str(pathlib.Path(__file__).parent / "portal.yaml"), "/root/portal.yaml")
)


@app.function(image=image, secrets=[modal.Secret.from_name("livekit-credentials")], timeout=3600)
def run_on_modal():
    asyncio.run(main())


@app.local_entrypoint()
def modal_entry():
    run_on_modal.remote()


if __name__ == "__main__":
    asyncio.run(main())
