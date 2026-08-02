# Remote inference example

VLA-style remote policy: the robot streams camera + state, a remote
"policy" emits a horizon of future actions per inference step as a single
**action chunk**, and the robot unrolls the chunk locally between
inference rounds.

This example is the canonical use case for two Portal features:

- **Action chunks** (`add_action_chunk` / `send_action_chunk` /
  `on_action_chunk`). These ship a `(horizon, n_fields)` tensor in one packet
  via LiveKit byte streams. No 15 KB data-packet limit; reliable by
  design.

- **Observation-correlated actions** (`in_reply_to_ts_us`). The policy
  tags every chunk with the observation timestamp it was produced from.
  The robot computes true end-to-end policy latency from this; surfaces
  as `metrics.policy.e2e_us_p50/p95`.

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
[robot] t= 3s chunks=15 chunk_age=143ms e2e=46.2ms/53.1ms (p50/p95) correlated=15 rtt=8ms
```

Reading left to right:

- `chunks`: how many chunks the policy has produced so far.
- `chunk_age`: wall-clock time since the latest chunk arrived. Should
  hover near `1 / PORTAL_CHUNKS_PER_SECOND`.
- `e2e`: `metrics.policy.e2e_us_p50/p95`: observation→action latency.
  This is the number to watch. Includes inference time, serialization,
  and network. It covers everything between "robot captured this state" and
  "robot received the corresponding action."
- `correlated`: `metrics.policy.correlated_received`. Should track the
  total chunks received (every chunk is correlated in this example).
- `rtt`: `metrics.rtt.rtt_us_last`. Note this is much smaller than
  `e2e`: ping doesn't include inference time. That's exactly what
  `metrics.policy` measures and `metrics.rtt` does not.

## Knobs

`.env` controls the run shape:

| Var | Default | Purpose |
|---|---|---|
| `PORTAL_FPS` | 30 | Robot's state + frame publish rate |
| `PORTAL_HORIZON` | 20 | Timesteps per action chunk |
| `PORTAL_CHUNKS_PER_SECOND` | 5 | Policy inference rate |
| `PORTAL_INFERENCE_LATENCY_MS` | 30 | Simulated forward-pass wall time |
| `PORTAL_DURATION_SECONDS` | 20 | Total run length |

Crank `PORTAL_INFERENCE_LATENCY_MS` to see `e2e_us_p50` track it. That's
the point of `metrics.policy`: ping says one thing, the actual policy
loop measures another, and you want to alert on the latter.

## Wiring it into your stack

The pieces map directly to a real VLA loop:

| Example function | Real-system equivalent |
|---|---|
| `_fake_inference(obs, horizon, latency_ms)` | Your VLA forward pass over `obs.frames` + `obs.state` |
| `ChunkPlayer` (in `robot.py`) | Your servo controller stepping through a horizon |
| `portal.send_action_chunk("act", chunk, in_reply_to_ts_us=obs.timestamp_us)` | Same line, real chunk |
| `portal.on_action_chunk("act", on_chunk)` | Same line, real chunk consumer |

Both peers must declare the same chunk schema (name, horizon, fields).
A mismatch (different horizon, renamed field, dtype flip) changes the
fingerprint and the receive side drops with a one-shot warning.
