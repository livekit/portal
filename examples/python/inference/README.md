# Remote inference example

VLA-style remote policy: the robot streams camera + state, a remote
"policy" plans a horizon of future actions per inference step, then steps
through that horizon locally and sends one action per control tick at the
robot's fps. Each new inference round replaces what is left of the previous
horizon.

The example centers on **observation-correlated actions**
(`in_reply_to_ts_us`). The policy tags every action with the timestamp of
the observation its horizon was planned from. The robot computes true
end-to-end policy latency from this and surfaces it as
`metrics.policy.e2e_us_p50/p95`.

## Run

```bash
cp .env.example .env       # fill in LIVEKIT_URL / KEY / SECRET / ROOM
uv sync

uv run robot.py            # terminal 1
uv run policy.py           # terminal 2
```

Defaults assume a local server (`ws://localhost:7880`, devkey/secret).
For LiveKit Cloud, point `LIVEKIT_URL` at your project and use real keys.

## What you should see

`robot.py` logs once a second, like:

```
[robot] t= 3s actions=82 action_age=18ms active=policy e2e=187.05ms/286.74ms (p50/p95) correlated=82 rtt=1.32ms
```

Reading left to right:

- `actions`: how many actions the robot has received so far. Should grow
  at about `PORTAL_FPS` per second once the first plan lands.
- `action_age`: wall-clock time since the latest action arrived. Should
  hover near `1 / PORTAL_FPS`.
- `e2e`: `metrics.policy.e2e_us_p50/p95`: observation→action latency.
  This is the number to watch. It covers everything between "robot captured
  this state" and "robot received an action planned from it": inference,
  serialization, network, and how far into the horizon the action sits.
  The first action of each horizon shows the inference-plus-network floor;
  later steps add one control tick each.
- `correlated`: `metrics.policy.correlated_received`. Should track the
  total actions received (every action is correlated in this example).
- `rtt`: `metrics.rtt.rtt_us_last`. Note this is much smaller than
  `e2e`: ping doesn't include inference time. That's exactly what
  `metrics.policy` measures and `metrics.rtt` does not.

## Knobs

`.env` controls the run shape:

| Var | Default | Purpose |
|---|---|---|
| `PORTAL_FPS` | 30 | Robot's state + frame publish rate |
| `PORTAL_HORIZON` | 20 | Actions planned per inference round |
| `PORTAL_INFERENCE_HZ` | 5 | Policy inference rate |
| `PORTAL_INFERENCE_LATENCY_MS` | 30 | Simulated forward-pass wall time |
| `PORTAL_DURATION_SECONDS` | 20 | Total run length |

Crank `PORTAL_INFERENCE_LATENCY_MS` to see `e2e_us_p50` track it.
Keep `PORTAL_HORIZON / PORTAL_FPS` longer than one inference round
(`1 / PORTAL_INFERENCE_HZ` plus the inference latency), or the policy runs
out of planned actions and the robot holds its last command until the next
plan lands. That's
the point of `metrics.policy`: ping says one thing, the actual policy
loop measures another, and you want to alert on the latter.

## Wiring it into your stack

The pieces map directly to a real VLA loop:

| Example function | Real-system equivalent |
|---|---|
| `_fake_inference(obs, horizon, latency_ms)` | Your VLA forward pass over `obs.frames` + `obs.state` |
| `Plan` (in `policy.py`) | Your action buffer stepping through the model's horizon |
| `op.send_action(cmd, in_reply_to_ts_us=plan.obs_ts_us)` | Same line, real per-tick command |
| `robot.on_action(tracker.push)` | Same line, hand `action.values` to your servo loop |

Both peers must declare the same action schema (names, order, dtypes). A
mismatch (renamed field, dtype flip) changes the fingerprint and the receive
side drops with a one-shot warning.
