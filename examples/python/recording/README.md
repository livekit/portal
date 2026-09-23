# Recording example

> Observers recording a session to Rerun and handing control between operators.

This example adds two **observers** to the [basic example](../basic). Observers
see everything in the room but never send actions:

- `recorder.py` writes state, video, the actions the robot executes and every
  keypoint to a Rerun `.rrd` archive with `RrdSink`.
- `orchestrator.py` hands control to each operator in turn and marks every take
  with `recording` / `idle` keypoints.

## Run it

```bash
cp .env.example .env      # fill in LIVEKIT_API_KEY and LIVEKIT_API_SECRET
uv sync
(cd ../basic && cp .env.example .env && uv sync)   # same credentials and room
```

Then four terminals:

```bash
cd ../basic && uv run robot.py           # terminal 1
cd ../basic && uv run teleoperator.py    # terminal 2
uv run recorder.py                       # terminal 3
uv run orchestrator.py                   # terminal 4
```

When the recorder finishes it prints the archive's path. Open it with
`rerun data/sessions/<room>-<time>.rrd`. Scrub the `robot_time` timeline to
see each frame, the state and the action taken in response on one row, with the
takes marked under `keypoints/`.

## What's in the archive

Everything is on the robot's clock, even though four processes produced it,
because every peer syncs to the robot's clock in the background. See
[Recording](../../../docs/03-portal-api.md#recording) for the full entity
layout. Turning archives into a dataset is up to your own tooling.

## The files

| File | What it does |
|---|---|
| `recorder.py` | Observer recording to `RrdSink`, printing frames written and dropped. |
| `orchestrator.py` | Observer rotating control between operators and marking takes with keypoints. |
| `portal.yaml` | The basic example's wire contract. |
| `_common.py` | Token minting and env helpers, shared with the basic example. |
