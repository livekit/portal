<p align="center">
  <a href="https://livekit.io/">
    <img src=".github/assets/livekit-mark.png" alt="LiveKit logo" width="100" height="100">
  </a>
</p>

<h1 align="center">livekit-portal</h1>

<p align="center">
  <a href="https://github.com/livekit/livekit-portal/actions/workflows/tests.yml"><img src="https://github.com/livekit/livekit-portal/actions/workflows/tests.yml/badge.svg?branch=main" alt="tests"></a>
  <a href="https://pypi.org/project/livekit-portal/"><img src="https://img.shields.io/pypi/v/livekit-portal" alt="PyPI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License"></a>
  <a href="https://www.python.org/downloads/"><img src="https://img.shields.io/badge/python-3.10%2B-blue" alt="Python 3.10+"></a>
</p>

<p align="center">
  <img src=".github/assets/portal-demo.gif" alt="Portal demo: synced camera and joint state between a remote robot and a local operator" width="720">
</p>

<!--BEGIN_DESCRIPTION-->
<p align="center"><b>Teleoperate, run policies, and record demonstrations against the same robot, from anywhere on the internet, with multiple operators in the room at once.</b> Portal carries cameras, joint state, and actions over LiveKit's room model. A policy and a human teleoperator can join the same session, hand off control mid-session with one call, and stream every executed action to a recorder for HITL training data. Synchronized <code>(frames, state, timestamp)</code> observations on the control side. Works with any robotics stack. Optional <a href="https://github.com/huggingface/lerobot">LeRobot</a> plugin for a one-line drop-in.</p>
<!--END_DESCRIPTION-->

<p align="center">
  <a href="#quickstart">Quickstart</a> ·
  <a href="#examples">Examples</a> ·
  <a href="docs/portal-api.md">Portal API</a> ·
  <a href="docs/concepts.md">Concepts</a> ·
  <a href="docs/synchronization.md">Deep dive</a>
</p>

---

## Features

**Multi-operator sessions.** A robot, policies, humans, recorders, and supervisors can all join the same session at once. The robot listens to whichever operator currently holds control. Other operators stream silently and are dropped at the gate. Handoff is `await op.set_active_operator("human-binh")` from any participant. Built on LiveKit participant attributes plus one RPC method.

- **Live human-in-the-loop.** Policy drives, human takes over to demonstrate corrections, policy resumes.
- **HITL data recording.** A passive operator joins with `set_action_subscription(True)` and receives every executed action labeled with `action.sender` plus the matching observation. Fits in a 50-line script.
- **Shadow evaluation.** Run a candidate policy alongside the active one. Both stream actions; only the active one is honored. The shadow records its outputs for offline comparison.
- **Supervisor arbitration.** A participant that never sends actions can still call `set_active_operator(...)` to route control. Useful for human overseers, scheduling pipelines, or A/B routing.

**Remote robot, same code.** Your robot loop keeps its shape. Portal moves the hardware to another machine. Your policy or teleop code still sees a local-looking robot object.

**Synced observations out of the box.** Cameras and joint state arrive fused into `Observation(frames, state, timestamp_us)`. That is the shape robotics policies already consume. No matching logic on your side.

**Built for VLA inference.** First-class **action chunks** ship a `(horizon, n_fields)` tensor in one packet via byte streams (no 15 KB cap). Tag every action with `in_reply_to_ts_us` and `metrics.policy.e2e_us_p50/p95` derives true observation→action latency, not just ping. See [`examples/python/inference/`](examples/python/inference) for a runnable VLA-style loop.

**Frame video for policies.** WebRTC video is lossy and resamples colorspace. For inference where pixels matter, pass a non-H264 codec to [`add_video`](docs/frame-video.md) (`RAW`, `PNG`, or `MJPEG`) and each frame ships independently over a reliable byte stream. Same `send_video_frame` / `on_video_frame` API, RGB on both ends. MJPEG q=90 sustains 30 fps at 720p.

**Works with any stack.** Role-specific `Robot` and `Operator` classes in Python and a unified `Portal` core in Rust. An optional [lerobot](https://github.com/huggingface/lerobot) plugin for a one-line wrap around your existing `Robot` or `Teleoperator`.

**Low-latency transport.** WebRTC video (SIMD RGB→I420). SCTP data channels with reliable or unreliable delivery per stream. Byte streams for arbitrary-size payloads. RPC for one-shots like `home` or `calibrate`. Rust core, Python bindings via UniFFI.

---

## Quickstart

### Install

```bash
pip install livekit-portal
```

Or with uv:

```bash
uv add livekit-portal
```

Prebuilt wheels are available for Linux (x86\_64, aarch64), macOS (Intel and Apple Silicon), and Windows (x86\_64). Python 3.10+ is required.

**lerobot plugin.** If your stack uses [lerobot](https://github.com/huggingface/lerobot), install the matching plugin instead:

```bash
pip install lerobot-robot-livekit          # robot side
pip install lerobot-teleoperator-livekit   # operator side
```

<details>
<summary>Build from source</summary>

You need a [Rust toolchain](https://rustup.rs/) (stable `cargo`) and [`uv`](https://docs.astral.sh/uv/).

```bash
git clone https://github.com/livekit/livekit-portal.git
cd livekit-portal

bash scripts/build_ffi_python.sh release
cd python && uv sync
```

`build_ffi_python.sh` compiles the `livekit-portal-ffi` cdylib and generates the UniFFI Python bindings. On a cold machine this takes a few minutes. Rerun it whenever the Rust code changes.

</details>

### Code

A complete remote-robot session in two files. The robot host publishes
frames and state, executes actions, and exposes a `home` RPC. The control
host receives synced observations, runs a policy, and calls `home` before
the control loop starts.

**`robot.py`** runs on the machine the robot is plugged into.

```python
import asyncio, time
from livekit.portal import DType, Robot, RobotConfig

async def main():
    cfg = RobotConfig("session-1")
    cfg.add_video("front")                       # add more tracks for multi-camera
    cfg.add_state_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.add_action_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.set_fps(30)

    robot_portal = Robot(cfg)

    # One-shot commands. Either side can register. Either side can invoke.
    def on_home(_):
        robot.home()
        return "ok"
    robot_portal.register_rpc_method("home", on_home)

    # Actions arrive here from whichever operator currently holds control.
    # Other operators in the room are silently dropped at the gate.
    robot_portal.on_action(lambda a: robot.send_action(a.values))

    await robot_portal.connect(url, token)

    while running:
        obs = robot.get_observation()
        ts = int(time.time() * 1_000_000)
        robot_portal.send_video_frame("front", obs.image, 640, 480, timestamp_us=ts)
        robot_portal.send_state(obs.state, timestamp_us=ts)
        await asyncio.sleep(1 / 30)

asyncio.run(main())
```

**`operator.py`** runs wherever your policy or teleop UI lives.

```python
import asyncio
from livekit.portal import DType, Operator, OperatorConfig

async def main():
    cfg = OperatorConfig("session-1", identity="policy-v1")
    cfg.add_video("front")
    cfg.add_state_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.add_action_typed([("j1", DType.F32), ("j2", DType.F32), ("j3", DType.F32)])
    cfg.set_fps(30)

    op = Operator(cfg)

    # Cameras, state, and a sender timestamp arrive fused as one tuple.
    def on_observation(obs):
        # obs.frames["front"], obs.state, obs.timestamp_us
        # Pass in_reply_to_ts_us so metrics.policy.e2e_us_* measures
        # true observation→action latency on the robot side.
        op.send_action(policy(obs), in_reply_to_ts_us=obs.timestamp_us)

    op.on_observation(on_observation)
    await op.connect(url, token)

    # Robot starts with `active_operator=None` and drops every action.
    # Claim control to be the one whose actions are accepted.
    await op.set_active_operator(op.local_identity())

    await op.perform_rpc("home")                 # imperative commands, not a loop
    print(op.metrics())                          # RTT, sync delta, jitter, drops

    while running:
        await asyncio.sleep(1)

asyncio.run(main())
```

That is the whole surface at work in one page. Synced observations, an
action callback, a control-plane claim, an RPC for one-shots, and a live
metrics snapshot. The code above is a sketch. For a runnable version with
token minting already wired up, see [`examples/python/basic/`](examples/python/basic)
or the step-by-step [Quickstart doc](docs/quickstart.md).

## Behind the project

Teleoperation over WAN is a networking problem before it is a robotics
problem. Low-latency video and control data have to traverse NAT,
asymmetric bandwidth, jitter, and packet loss. WebRTC was built for
exactly this, and [LiveKit](https://livekit.io/) wraps it in a
production-grade SFU with a clean SDK. Portal builds the robotics layer
on top.

That layer exists because robotics policies want one bundled
`Observation` per tick: cameras, joint state, and a timestamp arriving
together. LiveKit's transport primitives do not deliver data that way.
Video tracks and data streams each have their own pacing, codec path,
and retransmission. On the receiver they surface as independent event
streams arriving out of phase.

Portal closes that gap. Every outgoing frame and state packet carries the
sender's monotonic clock (packet-trailer metadata for video, a `u64`
prefix for data). On the control side, a per-session `SyncBuffer` matches
them by sender timestamp:

```text
for each head state S:
    for each registered video track k:
        F = nearest pending frame in track k to S
        if |S - F| < search_range:                   track k matches
        elif track k's newest frame is past S + R:   drop the state
        else:                                        wait for a newer frame

if every track matched:
    emit Observation { frames, state, timestamp_us: S }
```

The real implementation is amortized `O(N + M)` through two-pointer
cursors and blocker-gated short-circuiting, with `O(1)` unmatchability
detection. Full walkthrough in
[docs/synchronization.md](docs/synchronization.md). The
[Concepts](docs/concepts.md) page covers roles and the observation model.
[Tuning](docs/tuning.md) covers `fps`, `slack`, and `tolerance`.

## Multi-operator and HITL

A Portal session is a room. Robot, policies, humans, recorders, and
supervisors all join the same one. The robot listens to one operator at
a time, named by an attribute it publishes
(`lk.portal.active_operator`). Other operators' actions are dropped at
the gate. Handoff is one method call from any participant.

```python
# Policy is driving. Human takes over to demonstrate a correction.
await human.set_active_operator(human.local_identity())
# ... human teleops for a bit ...
# Hand back to policy.
await human.set_active_operator("policy-v1")
```

Four common patterns:

| Pattern | Who's in the room | What changes |
|---|---|---|
| **Single operator** | robot + 1 operator | Operator self-claims at startup. |
| **HITL teleop** | robot + policy + human | Either side calls `set_active_operator(...)` to switch. The robot's stream of executed actions is continuous across the cutover. |
| **HITL data recording** | robot + policy + human + recorder | Recorder joins as a passive observer with `set_action_subscription(True)`. Receives every executed action labeled with `action.sender`, paired with the synchronized observation. |
| **Shadow eval** | robot + active policy + candidate policy + recorder | Candidate streams its actions; the gate drops them. Recorder captures both streams for offline divergence scoring. |
| **Supervisor** | robot + N operators + supervisor UI | Supervisor never claims control. Calls `set_active_operator(...)` to route control to whichever operator should be active. |

Backing primitives:

1. **Participant attributes** for the active-operator pointer. Server-managed, broadcast on change, included in JoinResponse for late joiners.
2. **One RPC method** (`portal.set_active_operator`) for cross-participant writes to the robot's attribute.
3. **The SFU's data fanout.** Every operator already receives every other operator's action packets; Portal adds a one-line gate keyed on `active_operator`.

Recipes:
[recorder example](python/packages/livekit-portal/tests/integration/test_action_subscription.py),
[handoff tests](python/packages/livekit-portal/tests/integration/test_multi_operator.py).

## Examples

Running examples is the fastest way to a known-good setup. Both live under
[`examples/python/`](examples/python).

**[`examples/python/basic/`](examples/python/basic)**

No hardware required. Uses the Portal API directly. Synthetic video and
state on one terminal, subscriber on another. Proves your LiveKit
credentials and native build are healthy.

```bash
cd examples/python/basic
cp .env.example .env            # fill in LIVEKIT_URL / API_KEY / API_SECRET
uv sync
uv run robot.py                 # terminal 1
uv run teleoperator.py          # terminal 2
```

**[`examples/python/inference/`](examples/python/inference)**

VLA-style remote inference. The robot streams obs to a remote "policy"
which emits a `(horizon, n_fields)` **action chunk** per inference step.
The robot unrolls the chunk locally between rounds. Demonstrates the two
inference-shaped features: `add_action_chunk` and `in_reply_to_ts_us`.
Reports live `metrics.policy.e2e_us_p50/p95`, the actual
observation→action latency, not network ping.

```bash
cd examples/python/inference
cp .env.example .env
uv sync
uv run robot.py                 # terminal 1
uv run policy.py                # terminal 2
```

**[`examples/python/so101/`](examples/python/so101)**

Real hardware. Uses the lerobot plugin. A physical **SO-101 follower** is
driven by a remote **SO-101 leader**. Camera and joint state render in
[rerun](https://rerun.io). Full calibration and wiring walkthrough in its
[README](examples/python/so101/README.md).

## Using with lerobot

If your stack is already on [lerobot](https://github.com/huggingface/lerobot),
two optional plugin packages wrap the Portal code above. You pass in your
existing `Robot` or `Teleoperator` and the remote arm shows up as a local
lerobot device to any workflow (teleop, dataset recording, policy eval). See
[lerobot integration](docs/lerobot.md) for the full reference.

## Why LiveKit

Portal sits on LiveKit rather than raw WebRTC or a custom transport.
The choice keeps the codebase focused on robotics instead of plumbing.

| What LiveKit gives you | Why it matters for Portal |
|---|---|
| **Rooms with N participants** | A robot, two operators, a recorder, and a supervisor are the same session as 1:1. No new signaling, no mesh, no per-pair connection setup. |
| **Participant attributes** | Server-managed key-value state per participant, broadcast on change, included in JoinResponse for late joiners. The active-operator pointer is one attribute on the robot. |
| **Cross-participant RPC** | `portal.set_active_operator` is one method registered on the robot. Any operator calls it with one line. |
| **Production SFU** | A late joiner gets the full state without warm-up. Bandwidth is fanned out by the server, not by the robot. |
| **Tokens with attributes** | Initial values like `active_operator` can be seeded at token-mint time so the robot starts focused on a specific operator before anyone connects. JWT-based permissions per participant. |
| **Transport primitives** | RTP media with pacing and bandwidth adaptation. SCTP data channels, reliable or unreliable. Typed byte streams with chunking. Portal maps observations straight onto these. |
| **Cross-language SDKs** | Rust, Python, Swift, Kotlin, JavaScript, Unity. A browser teleop UI speaks the same protocol as the robot host. |
| **Deploy anywhere** | [LiveKit Cloud](https://livekit.io/cloud) for zero ops, or self-host the open-source server. TURN relays handle NAT traversal. |
| **Recording and egress** | Server-side session recording is one webhook away. |

Running on a single machine or a LAN-only robot? You do not need any of
this. A direct socket is enough.

## Documentation

| Page | What's in it |
|---|---|
| [Quickstart](docs/quickstart.md) | Install, tokens, first run with `Robot` and `Operator` |
| [Portal API](docs/portal-api.md) | The primary surface. `Robot`, `Operator`, callbacks, send methods, multi-controller |
| [Concepts](docs/concepts.md) | Roles, the observation model, multi-controller, frame format |
| [Tuning](docs/tuning.md) | `fps`, `slack`, `tolerance`, asymmetric rates, reliability |
| [RPC](docs/rpc.md) | Imperative commands (`home`, `calibrate`, ...) on top of LiveKit RPC |
| [Synchronization deep dive](docs/synchronization.md) | The full match algorithm, cursor bookkeeping, complexity |
| [lerobot integration](docs/lerobot.md) | The optional convenience plugins |

## License

Apache-2.0. See [LICENSE](LICENSE) for details.
