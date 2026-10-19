"""Same scenarios as portal_bench.py, on the plain LiveKit Python SDK (livekit.rtc).

Publisher marks each frame with FrameMetadata.user_timestamp; the subscriber
converts to RGB24 (what Portal delivers) and measures now - user_timestamp.
Control sends a timestamped JSON payload with publish_data at the given rate.

  python sdk_bench.py suite --label sdk --out results/sdk.json
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import struct
import subprocess
import tempfile
import sys
import time

import numpy as np
from livekit import rtc

from portal_bench import RES, frames, now_us, summarize, token

WEBRTC = ["H264", "VP8", "VP9", "AV1"]


async def worker(a) -> dict:
    room = rtc.Room()
    lat_video, lat_state, lat_action, send_block, recv_t = [], [], [], [], []
    sizes: dict[str, int] = {}
    t_measure = [None]

    def measuring() -> bool:
        return t_measure[0] is not None and time.monotonic() >= t_measure[0]

    async def consume(track: rtc.Track) -> None:
        stream = rtc.VideoStream(track, format=rtc.VideoBufferType.RGB24)
        async for ev in stream:
            if not measuring() or ev.metadata is None:
                continue
            lat_video.append((now_us() - ev.metadata.user_timestamp) / 1000)
            recv_t.append(time.monotonic())
            k = f"{ev.frame.width}x{ev.frame.height}"
            sizes[k] = sizes.get(k, 0) + 1

    tasks = []

    @room.on("track_subscribed")
    def _on_track(track, _pub, _participant):
        if track.kind == rtc.TrackKind.KIND_VIDEO:
            tasks.append(asyncio.ensure_future(consume(track)))

    @room.on("data_received")
    def _on_data(packet: rtc.DataPacket):
        if not measuring():
            return
        (ts,) = struct.unpack_from("<Q", packet.data)
        lat = (now_us() - ts) / 1000
        (lat_state if packet.topic == "state" else lat_action).append(lat)

    await room.connect(a.url, token(a.role, a.room))

    source = None
    if a.role == "robot" and a.codec:
        w, h = RES[a.res]
        source = rtc.VideoSource(w, h)
        track = rtc.LocalVideoTrack.create_video_track("cam", source)
        opts = rtc.TrackPublishOptions(
            source=rtc.TrackSource.SOURCE_CAMERA,
            video_codec=getattr(rtc.VideoCodec, a.codec),
            simulcast=False,
            video_encoding=rtc.VideoEncoding(max_framerate=a.fps * 2, max_bitrate=10_000_000),
            frame_metadata_features=[rtc.FrameMetadataFeature.FMF_USER_TIMESTAMP],
        )
        await room.local_participant.publish_track(track, opts)

    await asyncio.sleep(max(0.0, a.start_at - time.time()))
    t0 = time.monotonic()
    t_measure[0] = t0 + a.warmup
    t_end = t0 + a.warmup + a.duration

    imgs = frames(*RES[a.res]) if source else []
    vframes = [rtc.VideoFrame(i.shape[1], i.shape[0], rtc.VideoBufferType.RGB24, i.tobytes())
               for i in imgs]
    # Same 14 x f32 body as Portal's state/action, plus the timestamp.
    body = bytes(14 * 4)
    period = 1.0 / a.rate
    tick, sent = 0, 0
    frame_every = max(1, round(a.rate / a.fps)) if source else 0
    topic = "state" if a.role == "robot" else "action"
    while time.monotonic() < t_end:
        if source and tick % frame_every == 0:
            md = rtc.FrameMetadata(user_timestamp=now_us())
            t = time.perf_counter()
            source.capture_frame(vframes[sent % len(vframes)], metadata=md)
            dt = (time.perf_counter() - t) * 1000
            if measuring():
                send_block.append(dt)
            sent += 1
        if a.control:
            payload = struct.pack("<Q", now_us()) + body
            await room.local_participant.publish_data(payload, reliable=True, topic=topic)
        tick += 1
        await asyncio.sleep(max(0.0, t0 + tick * period - time.monotonic()))
    await asyncio.sleep(a.drain)

    res = {"role": a.role}
    if a.role == "robot":
        res["send_block_ms"] = summarize(send_block)
        res["action_latency_ms"] = summarize(lat_action)
    else:
        res["video_latency_ms"] = summarize(lat_video)
        res["state_latency_ms"] = summarize(lat_state)
        res["frames_received"] = len(recv_t)
        res["recv_fps"] = len(recv_t) / a.duration
        res["recv_sizes"] = sizes
        gaps = np.diff(recv_t) * 1000 if len(recv_t) > 1 else []
        res["max_gap_ms"] = float(np.max(gaps)) if len(gaps) else None
    for t in tasks:
        t.cancel()
    try:
        await asyncio.wait_for(room.disconnect(), 5)
    except Exception:  # noqa: BLE001
        pass
    return res


def run_scenario(name: str, base: list[str], timeout: float) -> dict:
    room = f"sdk-{name}-{int(time.time()*1000)}-{os.urandom(2).hex()}"
    start_at = time.time() + 4.0
    procs = {}
    for role in ("robot", "operator"):
        out = os.path.join(tempfile.gettempdir(), f"portal-sdk-bench-{room}-{role}.json")
        cmd = [sys.executable, __file__, "worker", "--role", role, "--room", room,
               "--start-at", str(start_at), "--result", out, *base]
        # A file, not a pipe: a full pipe blocks the worker's logging threads.
        errf = open(out + ".stderr", "w+")
        procs[role] = (subprocess.Popen(cmd, stderr=errf, text=True), out, errf)
    res = {"scenario": name}
    for role, (p, out, errf) in procs.items():
        try:
            p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
            res[f"{role}_error"] = "timeout"
        errf.seek(0)
        err = errf.read()
        errf.close()
        os.remove(out + ".stderr")
        if p.returncode != 0:
            res[f"{role}_error"] = (err or "")[-800:]
        try:
            with open(out) as f:
                res[role] = json.load(f)
            os.remove(out)
        except FileNotFoundError:
            pass
    return res


def suite(a) -> None:
    from portal_bench import brief

    results = {"label": a.label, "sdk": rtc.__version__, "started": time.time(),
               "duration": a.duration, "warmup": a.warmup, "scenarios": []}
    common = ["--duration", str(a.duration), "--warmup", str(a.warmup), "--drain", "2",
              "--fps", "30"]
    plan = []
    if a.only in (None, "control"):
        plan.append(("control-100hz", ["--control", "--rate", "100", "--res", "480p"]))
    if a.only in (None, "video"):
        codecs = a.codecs.split(",") if a.codecs else WEBRTC
        ress = a.res.split(",") if a.res else list(RES)
        for c in codecs:
            for r in ress:
                plan.append((f"{c}-{r}", ["--codec", c, "--res", r, "--rate", "30"]))
    if a.only in (None, "mixed"):
        plan.append(("mixed-H264-720p+control",
                     ["--codec", "H264", "--res", "720p", "--rate", "90", "--control"]))
    for name, extra in plan:
        print(f"[{a.label}] {name} ...", file=sys.stderr, flush=True)
        r = run_scenario(name, common + extra, timeout=a.duration + a.warmup + 40)
        results["scenarios"].append(r)
        print(json.dumps(brief(r)), file=sys.stderr, flush=True)
        with open(a.out, "w") as f:
            json.dump(results, f, indent=1)


def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("suite")
    s.add_argument("--label", required=True)
    s.add_argument("--out", required=True)
    s.add_argument("--only", choices=["video", "control", "mixed"])
    s.add_argument("--codecs")
    s.add_argument("--res")
    s.add_argument("--duration", type=float, default=10)
    s.add_argument("--warmup", type=float, default=8)
    w = sub.add_parser("worker")
    w.add_argument("--url", default=os.environ.get("LIVEKIT_URL", "ws://localhost:7880"))
    w.add_argument("--role", required=True)
    w.add_argument("--room", required=True)
    w.add_argument("--start-at", type=float, required=True)
    w.add_argument("--result", required=True)
    w.add_argument("--codec")
    w.add_argument("--res", default="480p")
    w.add_argument("--rate", type=float, default=30)
    w.add_argument("--fps", type=int, default=30)
    w.add_argument("--control", action="store_true")
    w.add_argument("--duration", type=float, default=10)
    w.add_argument("--warmup", type=float, default=8)
    w.add_argument("--drain", type=float, default=2)
    a = ap.parse_args()
    if a.cmd == "suite":
        suite(a)
    else:
        res = asyncio.run(worker(a))
        with open(a.result, "w") as f:
            json.dump(res, f)


if __name__ == "__main__":
    main()
