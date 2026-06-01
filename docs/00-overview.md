# Documentation overview

This folder documents `livekit-portal`, the robotics layer that carries
cameras, joint state, and actions between a remote robot and one or more
operators over LiveKit. For the project pitch, the feature list, and the
runnable demos, start with the [repository README](../README.md).

The pages are numbered in reading order. You do not have to read them all.
Most users need the first three and then dip into the reference pages as
questions come up.

## Reading order

| # | Page | What's in it |
|---|------|--------------|
| 1 | [Quickstart](01-quickstart.md) | Install, mint tokens, run a robot and an operator end to end. |
| 2 | [Concepts](02-concepts.md) | The mental model. Roles, the observation model, multi-operator control, the frame format. |
| 3 | [Portal API](03-portal-api.md) | The primary surface. `Robot`, `Operator`, callbacks, send methods, the control plane, gotchas. |
| 4 | [Config from YAML](04-config-file.md) | Build `RobotConfig` / `OperatorConfig` from a shareable wire-contract file. |
| 5 | [Frame video](05-frame-video.md) | Per-frame RGB over byte streams (RAW / PNG / MJPEG) for policies that read pixels. |
| 6 | [Tuning](06-tuning.md) | `fps`, `slack`, `tolerance`, asymmetric rates, transport reliability. |
| 7 | [RPC](07-rpc.md) | One-shot imperative commands like `home` or `calibrate`. |
| 8 | [E2EE](08-e2ee.md) | Shared-key end-to-end encryption for media and data. |
| 9 | [Synchronization](09-synchronization.md) | Deep dive on the match algorithm, cursors, and complexity. |
| 10 | [lerobot integration](10-lerobot.md) | Optional plugins that wrap the Portal API for lerobot users. |

## How to navigate

- **New here?** Read [Quickstart](01-quickstart.md), then
  [Concepts](02-concepts.md). That is enough to run Portal and understand
  what it does.
- **Building against the API?** [Portal API](03-portal-api.md) is the
  reference. Reach for [Config from YAML](04-config-file.md),
  [Frame video](05-frame-video.md), [Tuning](06-tuning.md),
  [RPC](07-rpc.md), and [E2EE](08-e2ee.md) as specific needs come up.
- **Want to know how sync works?**
  [Synchronization](09-synchronization.md) explains the algorithm in full.
  It is background, not required reading.
- **Already on lerobot?** The Portal API is the foundation everything else
  builds on, so read [Concepts](02-concepts.md) first. The
  [lerobot plugins](10-lerobot.md) are a thin convenience wrapper over that
  API.

## Conventions

- Code samples are Python unless marked otherwise. The same model holds in
  the Rust core and the other LiveKit SDKs.
- "Robot" and "Operator" are the two roles. There is one robot per session
  and any number of operators. See [Concepts](02-concepts.md).
- Numbers quoted in these pages (buffer sizes, match windows, defaults)
  reflect the current shipped configuration.
