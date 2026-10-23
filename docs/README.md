# LiveKit Portal documentation

LiveKit Portal connects robots to remote operators over a LiveKit room. Use it
to teleoperate a robot, run a policy on another machine, or record demonstrations.
The robot streams camera frames and state; operators send actions back.

Start with the [quickstart](01-quickstart.md) to run a complete control loop
without physical hardware. Then use the guides below for your application.

## Build your application

| Task | Guide |
|---|---|
| Connect a robot and an operator | [Quickstart](01-quickstart.md) |
| Understand how data and control move between participants | [Concepts](02-concepts.md) |
| Record demonstrations for review or training | [Data recording](03-data-recording.md) |
| Let a human take over from a policy | [Control handoff and arbitration](04-arbitration.md) |
| Configure roles, send data, and register callbacks | [Portal API](05-portal-api.md) |
| Share camera and field definitions between processes | [Config from YAML](reference/config-file.md) |
| Connect an existing LeRobot robot or teleoperator | [LeRobot plugins](reference/lerobot.md) |

Runnable projects are in [`examples/python/`](../examples/python): start with
`basic`, then try `so101` for hardware or `benchmark` to measure latency.

## Tune and debug

| Task | Guide |
|---|---|
| Adjust observation matching or handle a stalled camera | [Tuning](06-tuning.md) |
| Choose image quality and bandwidth for a policy | [Frame video](07-frame-video.md) |
| Send a request such as homing or calibration | [RPC](08-rpc.md) |
| Measure latency, frame delivery, and recording drops | [Metrics](09-metrics.md) |
| Diagnose missing data or a warning in the logs | [Troubleshooting](10-troubleshooting.md) |
| Upgrade an existing v0.2 application | [Migrating to v0.3](11-migrating-to-v0.3.md) |

## Reference

- [Synchronization](reference/synchronization.md): how Portal matches camera
  frames and state, including buffering and drop behavior.
- [Wire protocol](reference/wire-protocol.md): implement a compatible peer with
  another LiveKit SDK.
- [End-to-end encryption](reference/e2ee.md): configure a shared encryption key.

Examples use Python unless labeled otherwise. Full scripts include their setup;
code fragments assume the configuration or connection described on the page.
