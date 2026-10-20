"""Portal benchmark harness.

Robot and operator run in separate processes so a blocking send can't
delay the receiver's callbacks. Latencies use time.time_ns() on both sides
(same host, same clock).

  python portal_bench.py suite --label main --out results/main.json [--only video|control|mixed] [--size-hint]
  python portal_bench.py worker ...   (spawned by suite)
"""
from __future__ import annotations

import argparse
import asyncio
import datetime
import inspect
import json
import os
import subprocess
import tempfile
import sys
import time

import numpy as np

URL = os.environ.get("LIVEKIT_URL", "ws://localhost:7880")
KEY = os.environ.get("LIVEKIT_API_KEY", "devkey")
SECRET = os.environ.get("LIVEKIT_API_SECRET", "secret")

RES = {"480p": (640, 480), "720p": (1280, 720), "1080p": (1920, 1080)}
CODECS = ["H264", "VP8", "VP9", "AV1", "MJPEG", "PNG", "RAW"]
N_JOINTS = 14


def now_us() -> int:
    return time.time_ns() // 1000


def token(identity: str, room: str) -> str:
    from livekit import api

    grants = api.VideoGrants(
        room_join=True, room=room, can_publish=True, can_subscribe=True,
        can_update_own_metadata=True,
    )
    builder = api.AccessToken(KEY, SECRET).with_identity(identity).with_grants(grants)
    # BENCH_ROOM_PLAYOUT_DELAY=1 creates the room the way the quickstart does,
    # with tight playout delay bounds.
    if os.environ.get("BENCH_ROOM_PLAYOUT_DELAY"):
        builder = builder.with_room_config(
            api.RoomConfiguration(name=room, min_playout_delay=0, max_playout_delay=1)
        )
    return builder.with_ttl(datetime.timedelta(hours=1)).to_jwt()


def frames(w: int, h: int, n: int = 30) -> list[np.ndarray]:
    # Moving gradient plus mild noise: encoders do real work, every frame differs.
    rng = np.random.default_rng(0)
    x = np.arange(w, dtype=np.int32)
    y = np.arange(h, dtype=np.int32)[:, None]
    out = []
    for i in range(n):
        s = i * 8
        r = np.broadcast_to((x + s) % 256, (h, w))
        g = np.broadcast_to((y + s) % 256, (h, w))
        b = (x + y + s) % 256
        img = np.stack([r, g, b], axis=-1).astype(np.uint8)
        img ^= rng.integers(0, 8, size=img.shape, dtype=np.uint8)
        out.append(np.ascontiguousarray(img))
    return out


def pct(xs: list[float], p: float):
    if not xs:
        return None
    return float(np.percentile(np.asarray(xs), p))


def summarize(xs: list[float]) -> dict:
    return {"n": len(xs), "p50": pct(xs, 50), "p95": pct(xs, 95), "p99": pct(xs, 99),
            "mean": float(np.mean(xs)) if xs else None}


def make_cfg(role: str, a) -> object:
    from livekit.portal import DType, OperatorConfig, RobotConfig, VideoCodec

    cfg = (RobotConfig if role == "robot" else OperatorConfig)(a.room)
    cfg.set_fps(a.fps)
    joints = [(f"j{i}", DType.F32) for i in range(N_JOINTS)]
    cfg.add_state_typed(joints)
    cfg.add_action_typed(joints)
    if a.codec:
        kw = {}
        if a.size_hint and "width" in inspect.signature(cfg.add_video).parameters:
            kw["width"], kw["height"] = RES[a.res]
        cfg.add_video("cam", codec=getattr(VideoCodec, a.codec), **kw)
    return cfg


async def worker(a) -> dict:
    from livekit.portal import Operator, Robot

    cfg = make_cfg(a.role, a)
    portal = (Robot if a.role == "robot" else Operator)(cfg)
    loop = asyncio.get_running_loop()
    lat_video, lat_state, lat_action, send_block, recv_t = [], [], [], [], []
    t_measure = [None]
    sizes: dict[str, int] = {}

    def measuring() -> bool:
        return t_measure[0] is not None and time.monotonic() >= t_measure[0]

    def on_frame(_name, f):
        if measuring():
            lat_video.append((now_us() - f.timestamp_us) / 1000)
            recv_t.append(time.monotonic())
            sizes[f"{f.width}x{f.height}"] = sizes.get(f"{f.width}x{f.height}", 0) + 1

    def on_state(s):
        if measuring():
            lat_state.append((now_us() - s.timestamp_us) / 1000)

    def on_action(act):
        if measuring():
            lat_action.append((now_us() - act.timestamp_us) / 1000)

    if a.role == "operator":
        if a.codec:
            portal.on_video_frame("cam", on_frame)
        portal.on_state(on_state)
    else:
        portal.on_action(on_action)

    await portal.connect(URL, token(a.role, a.room))
    if a.role == "operator":
        await portal.set_active_operator(portal.local_identity())

    # Both peers start at the same wall-clock instant, picked by the suite.
    await asyncio.sleep(max(0.0, a.start_at - time.time()))
    t0 = time.monotonic()
    t_measure[0] = t0 + a.warmup
    t_end = t0 + a.warmup + a.duration

    imgs = frames(*RES[a.res]) if (a.role == "robot" and a.codec) else []
    vals = {f"j{i}": 0.0 for i in range(N_JOINTS)}
    period = 1.0 / a.rate
    tick, sent_frames = 0, 0
    frame_every = max(1, round(a.rate / a.fps)) if a.codec else 0
    while time.monotonic() < t_end:
        if a.role == "robot":
            if a.codec and tick % frame_every == 0:
                img = imgs[sent_frames % len(imgs)]
                ts = now_us()
                t = time.perf_counter()
                portal.send_video_frame("cam", img, timestamp_us=ts)
                dt = (time.perf_counter() - t) * 1000
                if measuring():
                    send_block.append(dt)
                sent_frames += 1
            if a.control:
                portal.send_state(vals, timestamp_us=now_us())
        elif a.control:
            portal.send_action(vals, timestamp_us=now_us())
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
        res["recv_sizes"] = sizes
        res["recv_fps"] = len(recv_t) / a.duration
        gaps = np.diff(recv_t) * 1000 if len(recv_t) > 1 else []
        res["max_gap_ms"] = float(np.max(gaps)) if len(gaps) else None
    try:
        m = portal.metrics()
        res["rtt_us_mean"] = m.rtt.rtt_us_mean
        res["frames_sent"] = dict(m.transport.frames_sent)
        res["frames_received_total"] = dict(m.transport.frames_received)
    except Exception:  # noqa: BLE001
        pass
    try:
        await asyncio.wait_for(portal.disconnect(), 5)
    except Exception:  # noqa: BLE001
        pass
    return res


def run_scenario(name: str, base: list[str], timeout: float) -> dict:
    room = f"bench-{name}-{int(time.time()*1000)}-{os.urandom(2).hex()}"
    start_at = time.time() + 4.0
    procs = {}
    for role in ("robot", "operator"):
        out = os.path.join(tempfile.gettempdir(), f"portal-bench-{room}-{role}.json")
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
    import livekit.portal as lp

    results = {"label": a.label, "portal_path": lp.__file__, "started": time.time(),
               "duration": a.duration, "warmup": a.warmup, "scenarios": []}
    common = ["--duration", str(a.duration), "--warmup", str(a.warmup),
              "--drain", "2", "--fps", "30"]
    if a.size_hint:
        common.append("--size-hint")
    plan = []
    if a.only in (None, "control"):
        plan.append(("control-100hz", ["--control", "--rate", "100", "--res", "480p"]))
    if a.only in (None, "video"):
        codecs = a.codecs.split(",") if a.codecs else CODECS
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


def brief(r: dict) -> dict:
    rb, op = r.get("robot", {}), r.get("operator", {})
    g = lambda d, k: (d.get(k) or {}).get("p50")  # noqa: E731
    return {"s": r["scenario"], "send_p50": g(rb, "send_block_ms"),
            "video_p50": g(op, "video_latency_ms"), "fps": op.get("recv_fps"),
            "sizes": op.get("recv_sizes"), "state_p50": g(op, "state_latency_ms"), "action_p50": g(rb, "action_latency_ms"),
            "err": {k: v[:120] for k, v in r.items() if k.endswith("_error")}}


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
    s.add_argument("--size-hint", action="store_true")
    w = sub.add_parser("worker")
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
    w.add_argument("--size-hint", action="store_true")
    a = ap.parse_args()
    if a.cmd == "suite":
        suite(a)
    else:
        res = asyncio.run(worker(a))
        with open(a.result, "w") as f:
            json.dump(res, f)


if __name__ == "__main__":
    main()
