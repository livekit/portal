# Basic example

> The whole Portal API end to end, with no hardware.

The robot script publishes a synthetic video track and a mixed-dtype state
stream. The operator script receives them fused into observations and sends
actions back. Run it first, before wiring Portal into your own stack, to confirm
your LiveKit credentials and your native build both work.

Uses the Portal API directly. No lerobot involved.

## What you need

- Python 3.12 and [`uv`](https://docs.astral.sh/uv/)
- A LiveKit server. [LiveKit Cloud](https://cloud.livekit.io) free tier works, so
  does a local `livekit-server --dev`.

## Run it

```bash
cp .env.example .env      # fill in LIVEKIT_API_KEY and LIVEKIT_API_SECRET
uv sync
```

Then two terminals:

```bash
uv run robot.py           # terminal 1
```

```bash
uv run teleoperator.py    # terminal 2
```

Both run for `PORTAL_DURATION_SECONDS` (30 by default), print a live metrics line
every two seconds, dump a full metrics snapshot, and disconnect cleanly.

## What you should see

The operator prints observations as they are matched, and the robot prints the
actions coming back:

```
[robot] connected; streaming at 30 fps for 30s
[robot] operator joined: teleoperator
[robot] active operator now: teleoperator
[operator] obs #30 ts=... state={'j1': 0.84, ..., 'gripper': True, 'mode': 1}
[robot] action #30: ts=... values={...} from=teleoperator
[operator] rtt=12.4ms/13.1ms/19.8ms sync_delta=1.2ms/3.4ms ... obs=60
```

Nothing arriving? See
[Troubleshooting](../../../docs/08-troubleshooting.md#nothing-is-arriving). The
usual cause is a token missing `can_update_own_metadata`.

## The files

| File | What it does |
|---|---|
| `robot.py` | Publishes video and state. Handles actions. Registers a `say` RPC. |
| `teleoperator.py` | Receives observations, sends actions, claims control, calls `say`. |
| `robot_yaml.py` | Same as `robot.py`, but loads the schema from `portal.yaml`. |
| `teleoperator_yaml.py` | Same as `teleoperator.py`, loading from `portal.yaml`. |
| `portal.yaml` | The shared wire contract: schemas, video tracks, sync knobs. |
| `_common.py` | Token minting, `.env` loading, and the metrics printer. |

### The YAML variant

`robot.py` and `teleoperator.py` declare their schema in code, which means the two
declarations can drift. `robot_yaml.py` and `teleoperator_yaml.py` do the same
thing while loading one shared file:

```bash
uv run robot_yaml.py          # terminal 1
uv run teleoperator_yaml.py   # terminal 2
```

The behavior is identical. The difference is that schema mismatch stops being
possible. See [Config from YAML](../../../docs/reference/config-file.md).

## What the example demonstrates

**Synced observations.** The robot stamps each frame and state packet with one
clock. `on_observation` fires once per matched pair. No matching code on your side.

**Mixed dtypes.** The schema is three `F32` joints, a `BOOL` gripper, and an `I8`
mode. `obs.state` hands them back as real Python `float`, `bool`, and `int`.

**Control handoff.** The robot starts with no active operator and drops every
action. `teleoperator.py` calls `set_active_operator(op.local_identity())` after
connecting, which is what makes the actions land.

**Drop reporting.** `on_drop` receives a list of state dicts that never found a
matching frame. A few at startup are normal.

**RPC.** The robot registers `say`. The operator calls it once after connecting.

**Metrics.** `periodic_metrics` in `_common.py` prints RTT, sync delta, jitter,
buffer fill, and drop counts. Lift it for your own scripts. See
[Metrics](../../../docs/07-metrics.md).

## Configuration

Everything is driven by `.env`.

| Variable | Default | Purpose |
|---|---|---|
| `LIVEKIT_URL` | `ws://localhost:7880` | Server URL. |
| `LIVEKIT_API_KEY` | none | Required. |
| `LIVEKIT_API_SECRET` | none | Required. |
| `LIVEKIT_ROOM` | `portal-demo` | Room name, shared by both scripts. |
| `PORTAL_FPS` | `30` | Capture rate. Both sides must agree. |
| `PORTAL_FRAME_WIDTH` | `320` | Frame width. Must be even. |
| `PORTAL_FRAME_HEIGHT` | `240` | Frame height. Must be even. |
| `PORTAL_DURATION_SECONDS` | `30` | How long the robot streams before disconnecting. |

Raise the resolution to see the RGB-to-I420 conversion cost and the encoder work
harder. Lower `PORTAL_FPS` on a constrained link.

## Next steps

- [Quickstart](../../../docs/01-quickstart.md). The same thing, built up step by
  step.
- [Concepts](../../../docs/02-concepts.md). The model behind what you just ran.
- [`../inference/`](../inference). Action chunks and end-to-end latency metrics.
- [`../so101/`](../so101). The same ideas on real hardware.
