# LiveKit Portal documentation

> Cameras, joint state, and actions between a remote robot and one or more
> operators, over LiveKit.

Portal gives your control code a local-looking robot that happens to be
somewhere else on the internet. It carries video and state one way, actions
the other way, and hands your policy a single fused `Observation` per tick.

If you are new here, read [Quickstart](01-quickstart.md) and then
[Concepts](02-concepts.md). That is enough to run Portal and understand what
it does. Everything after that is reference material you can reach for when a
specific question comes up.

## Start here

| Page | What you get |
|---|---|
| [1. Quickstart](01-quickstart.md) | Install, mint a token, run a robot and an operator end to end. |
| [2. Concepts](02-concepts.md) | The mental model. Roles, observations, control handoff, frame format. |
| [3. Portal API](03-portal-api.md) | The main surface. `Robot`, `Operator`, callbacks, send methods, control plane. |

## Then as needed

| Page | Reach for it when |
|---|---|
| [4. Tuning](04-tuning.md) | You want to change `fps`, `slack`, or `tolerance`, or you are seeing drops. |
| [5. Frame video](05-frame-video.md) | A policy reads the pixels and lossy H.264 is not acceptable. |
| [6. RPC](06-rpc.md) | You need one-shot commands like `home` or `calibrate`. |
| [7. Metrics](07-metrics.md) | You want to know what `portal.metrics()` contains and which number to watch. |
| [8. Troubleshooting](08-troubleshooting.md) | Something is not working, or a tagged warning showed up in your logs. |

## Reference

Deeper material. None of it is required reading.

| Page | What's in it |
|---|---|
| [Config from YAML](reference/config-file.md) | Build `RobotConfig` / `OperatorConfig` from a shareable wire-contract file. |
| [E2EE](reference/e2ee.md) | Shared-key end-to-end encryption for media and data. |
| [Synchronization](reference/synchronization.md) | The full match algorithm, cursors, fair-share, complexity. |
| [Wire protocol](reference/wire-protocol.md) | Topics, attributes, RPC, and binary layouts for building a Portal peer in any LiveKit SDK. |
| [lerobot plugins](reference/lerobot.md) | Optional convenience wrappers for stacks already on lerobot. |

## Find your path

**I want to run something right now.** [Quickstart](01-quickstart.md), then
[`examples/python/basic/`](../examples/python/basic). No hardware needed.

**I am writing a policy against Portal.** [Concepts](02-concepts.md), then
[Portal API](03-portal-api.md). If your policy reads pixels, add
[Frame video](05-frame-video.md).

**I am seeing dropped observations.** [Troubleshooting](08-troubleshooting.md)
for the warning tag, then [Tuning](04-tuning.md) for the knob to turn.

**I want a human and a policy sharing one robot.**
[Concepts: control handoff](02-concepts.md#control-handoff), then
[Portal API: the active operator](03-portal-api.md#the-active-operator).

**I am on lerobot already.** Read [Concepts](02-concepts.md) first, because
the plugins are a thin wrapper over the same model. Then
[lerobot plugins](reference/lerobot.md).

**I am writing a Portal peer in another SDK.**
[Wire protocol](reference/wire-protocol.md) is the whole contract.

## Conventions in these pages

Code samples are Python. The same model holds in the Rust core and in the
other LiveKit SDKs.

"Robot" and "Operator" are the two roles. One robot per session, any number of
operators. See [Concepts](02-concepts.md).

Samples that show a full file are runnable as written once you fill in your
LiveKit credentials. Samples that show a fragment say so.

Defaults quoted in these pages match the shipped code. If a number here
disagrees with what you observe, the code wins and the docs have a bug.
