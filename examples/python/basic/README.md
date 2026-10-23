# Basic example

> Portal's API end to end, with no hardware.

Three roles share one room:

- **`robot.py`** streams a synthetic camera and arm state, and executes actions
  from the operator in control.
- **`teleoperator.py`** takes control, reads observations, and sends actions back.
- **`observer.py`** records the session to a Rerun archive and marks each take
  with keypoints. It sees everything and never drives.

All three load the same wire contract from `portal.yaml`. Everything that isn't
Portal (settings, tokens, the fake camera, pacing) lives in `_common.py`.

## What you need

- Python 3.12 and [`uv`](https://docs.astral.sh/uv/)
- A LiveKit server. [LiveKit Cloud](https://cloud.livekit.io) free tier works, so
  does a local `livekit-server --dev`.

## Run it

```bash
cp .env.example .env      # fill in LIVEKIT_API_KEY and LIVEKIT_API_SECRET
uv sync
```

Then one terminal per role:

```bash
uv run robot.py           # terminal 1
uv run teleoperator.py    # terminal 2
uv run observer.py        # terminal 3, optional
```

Each runs for `PORTAL_DURATION_SECONDS` and disconnects. The observer prints the
archive's path at the end. Open it with `rerun <path>` and scrub the
`robot_time` timeline to see each frame, the state and the action taken in
response on one row, with the takes marked under `keypoints/`.

## What you should see

```
[robot] in control: teleoperator
[robot] teleoperator says: hello
[robot] action from teleoperator: {'j1': 0.82, 'j2': 0.57, 'j3': 0.10, 'gripper': True, 'mode': 0}
[robot] e2e_p95=9.1ms
[teleoperator] 320x240 frame, state {'j1': 0.88, 'j2': 0.48, 'j3': 0.11, 'gripper': False, 'mode': 1}
[teleoperator] rtt_p95=3.7ms sync_delta_p95=0.0ms
[observer] recording {'task_description': 'take 0'}
[observer] idle {}
```

Nothing arriving? See
[Troubleshooting](../../../docs/10-troubleshooting.md#nothing-is-arriving). The
usual cause is a token missing `can_update_own_metadata`.

## What each role shows

**Robot.** It stamps each frame and state with one `now_us()` value, so the
operator receives them fused into one observation. `get_action()` returns the
latest action from the operator in control. The robot ignores everyone else.
It also registers a `say` RPC.

**Teleoperator.** It claims control with `set_active_operator`, since the robot
starts with nobody in control. It reads the latest observation with
`get_observation()`. `obs.state` comes back typed per the schema: floats, a
`bool` gripper, an `int` mode. It replies with `send_action`, passing
`in_reply_to_ts_us` so the robot can measure end-to-end latency
(`metrics().policy.e2e_us_p95`).

**Observer.** It waits for `on_time_synced` so the archive is on the robot's
clock from its first row, then records with `record_to(RrdSink(...))`. It
brackets each take with `recording` and `idle` keypoints using `send_keypoint`.
See [Data recording](../../../docs/03-data-recording.md).

`print_metrics` in `_common.py` prints the headline numbers from `metrics()`.
See [Metrics](../../../docs/09-metrics.md).

## Configuration

Everything is driven by `.env`.

| Variable | Default | Purpose |
|---|---|---|
| `LIVEKIT_URL` | none | Required. Server URL, e.g. `ws://localhost:7880`. |
| `LIVEKIT_API_KEY` | none | Required. |
| `LIVEKIT_API_SECRET` | none | Required. |
| `LIVEKIT_ROOM` | none | Required. Room shared by every role; `.env.example` uses `portal-demo`. |
| `PORTAL_DURATION_SECONDS` | `30` | How long each role runs. |
| `PORTAL_TAKE_SECONDS` | `5` | Length of each take the observer marks. |
| `PORTAL_RECORDING_DIR` | `data/sessions` | Where the observer writes `.rrd` archives. |

The fps, tracks and schemas are in `portal.yaml`. See
[Config from YAML](../../../docs/reference/config-file.md).

## Next steps

- [Quickstart](../../../docs/01-quickstart.md). The same thing, built up step by
  step.
- [Concepts](../../../docs/02-concepts.md). The model behind what you just ran.
- [`../so101/`](../so101). The same ideas on real hardware, with the LeRobot plugins.
- [`../benchmark/`](../benchmark). Latency and jitter for each stream on your network.
