# Migrating to v0.3

v0.3 adds observers, recording, keypoints, and clock synchronization. It also
changes action subscriptions and removes action chunks. Upgrade all participants
together, then update the affected calls below.

## Upgrade every peer together

v0.2 and v0.3 peers cannot share a room. Every peer now publishes
`lk.portal.version = "3"`, and a peer whose version is missing or different is
logged as `[version-mismatch]` and left out of every roster. The robot drops its
actions and refuses its `set_active_operator` calls.

**Do this:** upgrade the robot and every operator at the same time. If something
stops arriving after an upgrade, look for `[version-mismatch]` in the logs.

## Action chunks are removed

`send_action_chunk`, `on_action_chunk`, `get_action_chunk`, `add_action_chunk`,
the `action_chunks` config property, the `ChunkSpec` and `ActionChunk` exports,
`PortalError.UnknownChunk`, the `portal_action_chunk` topic, the yaml
`action_chunks` block and the chunk metrics are gone. A yaml file that still has `action_chunks` fails with
`ConfigFileError.ActionChunksRemoved`.

**Do this:** plan the horizon on the policy side and step through it, sending
one action per control tick with the observation it answers:

```python
plan = policy(obs)                     # shape (horizon, n_fields)
for row in plan:
    op.send_action(dict(zip(fields, row)), in_reply_to_ts_us=obs.timestamp_us)
    await asyncio.sleep(1 / fps)
```

## `action_subscription` names a mode

The bool is gone. `set_action_subscription` and the yaml key take `none`,
`active` or `all`, and a bool raises (`TypeError` in Python, a config error in
yaml) with the replacement spelled out.

| v0.2 | v0.3 |
|---|---|
| `set_action_subscription(False)` | `set_action_subscription("none")`, the default |
| `set_action_subscription(True)` | `set_action_subscription("active")` |
| — | `set_action_subscription("all")`: also the actions the gate dropped |

Every `Action` now carries `active`, stamped at gate time next to `sender`.
`active=False` marks a shadow action the robot ignored.

**Do this:** replace `True` with `"active"` and `False` with `"none"`. If you
record, consider an [observer](05-portal-api.md#surface-summary) instead of an
operator that promises not to drive.

## Timestamps default to the robot's clock

`send_state`, `send_video_frame`, `send_action` and the new `send_keypoint` now
default `timestamp_us` to `now_us()`. On the robot that is its own clock, which
never goes backwards. Everywhere else it is the time sync estimate of the
robot's clock, so an operator's actions land on the robot's timeline instead of
its own. Explicit timestamps pass through unchanged, and `in_reply_to_ts_us` is
never touched.

**Do this:** nothing, unless you relied on an operator stamping with its own
wall clock. Labs whose hosts are already synced by PTP or GPS can keep the host
clock with `set_time_sync_source(TimeSyncSource.SYSTEM)` (yaml
`time_sync_source: system`); see [Time sync](05-portal-api.md#time-sync).

## `ping_ms` is removed

`set_ping_ms`, `ping_ms` and the yaml `ping_ms` key are gone, along with the
`portal_rtt` topic. RTT now comes from the time sync exchange on
`portal_clock`, which runs at fixed rates (4 Hz until synced, then 1 Hz).
`metrics().rtt` keeps its fields, but the robot only answers pings now, so its
own `rtt` stays empty.

A yaml file that still has `ping_ms` no longer loads: unknown keys are a
`ConfigFileError.Parse` error.

**Do this:** delete `set_ping_ms` calls and `ping_ms` yaml keys.

## Only operators and observers may steer

The robot's `portal.set_active_operator` RPC now refuses callers that are
neither an operator nor an observer, with RPC error `2003`. The robot also
drops actions from any sender that is not a recognised operator, even if the
active-operator pointer names it.

**Do this:** if a dashboard or script moved the pointer by calling the RPC
directly, make it an [observer](05-portal-api.md#surface-summary) and call
`set_active_operator`.

## Smaller API changes

- **`WrongRole` names the caller's own role.** It used to report the opposite
  role, hard-coded.
- **Rust:** `PortalConfig::add_video` takes `(name, codec, VideoOptions { quality,
  max_bitrate_kbps, simulcast, screencast, resolution })` instead of the
  positional options. `resolution` is an `Option` of the new `Resolution` type.
  `set_action_subscription` and its getter use `ActionSubscription` instead of a
  `bool`. `set_ping_ms` and `ping_ms` are gone.
- **Rust:** `Portal::get_observation` returns
  `PortalResult<Option<Observation>>` and `Portal::on_observation` returns
  `PortalResult<()>`. Both fail with `ObservationSyncDisabled` on a peer that
  turned observation sync off. `Role` has a third variant, `Observer`, so
  exhaustive matches need an arm.
- **Raw FFI users:** the `PortalCallbacks` trait gained `on_time_synced` and
  `on_keypoint`, and `add_video` gained positional `width` and `height`
  arguments. The `livekit.portal` classes handle both for you.

## What's new

None of these break anything, but they are why v0.3 exists:

- [**Observers**](02-concepts.md#roles) see everything in the room but never
  drive. Use them to record, or to hand control between operators.
- [**Keypoints**](05-portal-api.md#keypoints) are free-form `{type, payload}`
  annotations from any role, on the robot's clock.
- [**Time sync**](05-portal-api.md#time-sync) puts every peer on the robot's
  clock: `now_us()`, `on_time_synced`, `metrics().time_sync`.
- [**Observation sync is optional**](05-portal-api.md#receiving-data) for peers
  that don't consume bundles live, like a teleoperator or a recorder.
- [**Recording**](03-data-recording.md) from an observer to a sink, with
  `RrdSink` writing Rerun archives. See
  [`examples/python/basic`](../examples/python/basic).
