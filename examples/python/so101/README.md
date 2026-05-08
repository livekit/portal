# SO-101 teleoperation over LiveKit Portal

End-to-end example: drive a physical **SO-101 follower arm** from a remote
**SO-101 leader arm** over a LiveKit Portal session, with synced joint state
+ one camera streamed back for visualization in [rerun](https://rerun.io).

Two scripts, one per machine:

| Script            | Runs on                          | Hardware attached                 |
| ----------------- | -------------------------------- | --------------------------------- |
| `robot.py`        | The machine next to the arm      | SO-101 follower + USB camera      |
| `teleoperator.py` | The operator's laptop            | SO-101 leader arm                 |

They connect to the **same LiveKit room** and sync via Portal.

---

## 1. Prerequisites

- Python 3.12+ and [`uv`](https://docs.astral.sh/uv/)
- A LiveKit server — either [LiveKit Cloud](https://cloud.livekit.io) or a
  local one (`livekit-server --dev`)
- SO-101 leader + follower arms (Feetech STS3215 motors), each with their
  own USB-serial controller
- One OpenCV-compatible camera on the robot side
- The Portal native library built once at the repo root:
  `bash scripts/build_ffi_python.sh`

---

## 2. Install

From this directory:

```bash
uv sync
```

Installs `lerobot[feetech]`, the Portal SDK, the `lerobot-robot-livekit`
and `lerobot-teleoperator-livekit` plugins, and `rerun-sdk` into a local
venv. The three Portal packages are picked up as editable installs from
`../../packages/`.

---

## 3. Find the serial ports

Each arm needs its own serial port. Use lerobot's helper — unplug the arm
it asks about, press enter, then plug it back in:

```bash
# Run once for the follower, once for the leader.
uv run lerobot-find-port
```

Record the two paths (e.g. `/dev/tty.usbmodem...` on macOS, `/dev/ttyACM...`
on Linux). You'll put them in `.env` below.

---

## 4. Calibrate the arms

**This step is required on first use** and any time you swap servos or
significantly disassemble/reassemble an arm. Calibration maps raw encoder
ticks to joint angles and captures each motor's range of motion. Files are
cached under `~/.cache/huggingface/lerobot/calibration/` keyed by the `id`
you pass.

### 4a. Leader

```bash
uv run lerobot-calibrate \
  --teleop.type=so101_leader \
  --teleop.port=/dev/tty.usbmodem-LEADER \
  --teleop.id=so101_leader
```

Follow the on-screen prompts:
1. Move the arm to the **middle of its range**, press enter → sets homing offsets.
2. Move **every joint through its full range** (except `wrist_roll`, which
   is treated as full-turn), press enter → records min/max.

### 4b. Follower

Same flow, different flags:

```bash
uv run lerobot-calibrate \
  --robot.type=so101_follower \
  --robot.port=/dev/tty.usbmodem-FOLLOWER \
  --robot.id=so101_follower
```

> Leaders are torque-off during calibration (you move them by hand).
> Followers will self-drive during range capture — keep clear of the arm.

---

## 5. Configure `.env`

```bash
cp .env.example .env
```

Fill in:

```ini
LIVEKIT_URL=wss://your-project.livekit.cloud      # or ws://localhost:7880
LIVEKIT_API_KEY=...
LIVEKIT_API_SECRET=...

LIVEKIT_ROOM=so101-portal                         # any string, both sides must match
PORTAL_FPS=30

# Robot side
SO101_FOLLOWER_PORT=/dev/tty.usbmodem-FOLLOWER
SO101_FOLLOWER_ID=so101_follower                  # must match --robot.id from step 4b
SO101_CAMERA_NAME=front
SO101_CAMERA_INDEX=0
SO101_CAMERA_WIDTH=640
SO101_CAMERA_HEIGHT=480

# Operator side
SO101_LEADER_PORT=/dev/tty.usbmodem-LEADER
SO101_LEADER_ID=so101_leader                      # must match --teleop.id from step 4a
```

Same `.env` file works on both sides — each script reads only the fields
it needs. If the two machines have different ports, either keep two copies
or use `.env.local` for per-machine overrides (loaded after `.env`).

---

## 6. Run

**Terminal 1 — on the machine wired to the follower:**

```bash
uv run robot.py
```

**Terminal 2 — on the operator's laptop:**

```bash
uv run teleoperator.py
```

The rerun viewer auto-spawns on the operator side. You'll see:
- `observation.<camera_name>` — live video from the follower (JPEG-compressed,
  scrubbable on the `robot_time` timeline aligned to the follower's clock).
- `observation.<motor>.pos` — follower joint positions as scalar plots.
- `action.<motor>.pos` — commanded positions from the leader.

Stop either side with Ctrl-C.

---

## 7. Troubleshooting

**"No calibration file found"** — you skipped step 4. Run `lerobot-calibrate`
for the side that complained. IDs in `.env` must match the IDs you used during
calibration (we default to `so101_leader` / `so101_follower` above).

**Follower doesn't move** — check the LiveKit room name matches on both sides,
and that your API key has `can_publish` / `can_subscribe` (the mint helper
sets this automatically). Watch either script's stdout for connection errors.

**Laggy or frozen video** — confirm `PORTAL_FPS` matches on both sides. If
you're on a local dev server, keep the two scripts on the same LAN for
minimum RTT.

**Camera not detected** — try another `SO101_CAMERA_INDEX` (0, 1, 2…). On
macOS, the first time you run it the OS may prompt for camera permission —
accept and rerun.

**`ImportError: cannot find liblivekit_portal_ffi`** — build the native
library: `cd ../../.. && bash scripts/build_ffi_python.sh`.

---

## How the pieces fit

```
┌─────────────────────┐                       ┌──────────────────────┐
│ robot.py            │                       │ teleoperator.py      │
│                     │                       │                      │
│ SO101Follower  ◀────┤                       ├─▶ SO101Leader        │
│      │              │                       │       │              │
│      ▼              │                       │       ▼              │
│ LiveKitTeleoperator │◀─── LiveKit Portal ──▶│  LiveKitRobot        │
│   (Portal Robot)    │    (room = LIVEKIT_   │   (Portal Operator)  │
│                     │     ROOM)             │       │              │
└─────────────────────┘                       │       ▼              │
                                              │   rerun viewer       │
                                              └──────────────────────┘
```

The Portal plugins (`lerobot-robot-livekit`, `lerobot-teleoperator-livekit`)
wrap Portal inside lerobot's `Robot` / `Teleoperator` interfaces, so each
side sees the *other* arm as a local lerobot device.
