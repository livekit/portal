# Config from YAML

> One shareable file describing the wire contract, loaded by both peers.

Schema mismatch is the most common way a Portal session fails, and it happens
because two processes declare their fields in two places. A shared YAML file
removes that whole class of bug.

```python
from livekit.portal import RobotConfig

cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
```

The file describes the **wire contract only**: schemas, video tracks, and sync
knobs. Identity and secrets are supplied at the call site, so the same file is
reusable across the robot and the operator, and it never holds anything sensitive.

The Rust core does the parsing and validation. There is exactly one parser, so a
file loaded from Python, from the FFI, or from the Rust crate produces an
equivalent config every time.

A runnable version lives in
[`examples/python/basic/`](../../examples/python/basic), as `portal.yaml` plus
`robot_yaml.py` and `teleoperator_yaml.py`.

## Loading

Every config class exposes the same two entry points.

```text
RobotConfig.from_yaml_str(yaml: str, session: str) -> RobotConfig
RobotConfig.from_yaml_file(path: str | os.PathLike, session: str) -> RobotConfig

OperatorConfig.from_yaml_str(yaml: str, session: str) -> OperatorConfig
OperatorConfig.from_yaml_file(path: str | os.PathLike, session: str) -> OperatorConfig

PortalConfig.from_yaml_str(yaml: str, session: str, role: Role) -> PortalConfig
PortalConfig.from_yaml_file(path: str | os.PathLike, session: str, role: Role) -> PortalConfig
```

`RobotConfig` and `OperatorConfig` pin the role for you. Use them unless you have
a reason to drive `PortalConfig` directly.

Everything the file declared is readable back off the loaded config, so a value
the file owns doesn't have to be duplicated in your process:

```python
cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
period = 1.0 / cfg.fps        # the file's rate, not a second copy of it
```

See [the full property list](../03-portal-api.md#full-config-surface).

## A minimal file

```yaml
version: 1
fps: 30

videos:
  - { name: cam1, codec: h264 }

state:
  - { name: j1, dtype: f32 }
  - { name: j2, dtype: f32 }
  - { name: gripper, dtype: bool }

action:
  - { name: j1, dtype: f32 }
  - { name: j2, dtype: f32 }
  - { name: gripper, dtype: bool }
```

Both sides load it, and the schemas cannot drift:

```python
robot_cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
op_cfg = OperatorConfig.from_yaml_file("portal.yaml", "session-1")
```

## Every field at once

Nothing here is required except `version`. Treat this as the reference for what
the loader understands.

```yaml
version: 1

# Sync and pacing
fps: 30                      # observation rate
slack: 5                     # pipeline headroom, in ticks
tolerance: 1.5               # match window, in ticks

# Transport reliability
state_reliable: true
action_reliable: true

# Sync behavior
reuse_stale_frames: false    # freeze video on loss instead of dropping state

# Heartbeat
ping_ms: 1000                # RTT probe cadence. 0 disables probing here.

# Operator-only opt-in
action_subscription: false   # receive executed actions, for HITL recording

# Video tracks
videos:
  - { name: front, codec: h264, max_bitrate_kbps: 8000 }
  - { name: wrist, codec: mjpeg, quality: 90 }
  - { name: depth, codec: png }
  - { name: raw_test, codec: raw }

# Wire schemas. State flows robot to operator, action flows the other way.
state:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper, dtype: bool }
  - { name: mode, dtype: i8 }

action:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper, dtype: bool }
  - { name: mode, dtype: i8 }

# VLA-style fixed-horizon batched actions
action_chunks:
  - name: vla
    horizon: 16
    fields:
      - { name: joint_pos, dtype: f32 }
      - { name: gripper, dtype: bool }
```

## Top level

| Field | Type | Default | Description |
|---|---|---|---|
| `version` | int | **required** | Major version of the file format. Currently `1`. Unknown majors are rejected. |
| `fps` | int | `30` | Observation rate. Drives `search_range = tolerance / fps`. |
| `slack` | int | `5` | Pipeline headroom, in ticks. |
| `tolerance` | float | `1.5` | Match window, in ticks. See [Tuning](../04-tuning.md). |
| `state_reliable` | bool | `true` | Reliable transport for state packets. |
| `action_reliable` | bool | `true` | Reliable transport for action packets. |
| `reuse_stale_frames` | bool | `false` | Reuse a track's most recent frame when the current state has no in-range match. |
| `ping_ms` | int | `1000` | RTT probe cadence in ms. `0` disables probing on this side. The echo path stays active. |
| `action_subscription` | bool | `false` | Operator-side opt-in for receiving executed actions. No-op on the robot. |
| `videos` | list | `[]` | Declared video tracks. |
| `state` | list | `[]` | Declared state schema. |
| `action` | list | `[]` | Declared action schema. |
| `action_chunks` | list | `[]` | Declared action chunks. |

Anything else at the top level is a hard error. The loader uses
`deny_unknown_fields` at every level, so a misspelled `tolarance: 1.5` raises
instead of silently doing nothing.

## `videos`

```yaml
videos:
  - { name: front,   codec: h264, max_bitrate_kbps: 8000 }
  - { name: wide,    codec: vp9 }
  - { name: wrist,   codec: mjpeg, quality: 90 }
  - { name: depth,   codec: png }
  - { name: raw_cam, codec: raw }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | **Required.** Track name. Unique across all entries. |
| `codec` | string | **Required.** One of `h264`, `vp8`, `vp9`, `av1`, `h265`, `mjpeg`, `png`, `raw`. Case-insensitive, and `hevc` is an alias for `h265`. |
| `quality` | int | Optional. `1` to `100`, for `mjpeg` only. Defaults to `90`. Ignored for every other codec. |
| `max_bitrate_kbps` | int | Optional. Encoder bitrate ceiling for the WebRTC codecs. Defaults to `10000`. Rejected on the byte-stream codecs. |

The codec picks both the encoding and the transport.

**`h264`, `vp8`, `vp9`, `av1`, `h265`** use the WebRTC media path. Real-time
RTP, lossy, best-effort delivery. libwebrtc picks the operating bitrate up to
`max_bitrate_kbps`. Lowest end-to-end latency at scale.

**`mjpeg`** is a per-frame byte stream, lossy. Roughly 10 to 20x compression at
q=90, with sub-millisecond decode. Each frame is independent.

**`png`** is a per-frame byte stream, lossless. Roughly 2 to 3x compression on
natural images.

**`raw`** is a per-frame byte stream, uncompressed RGB24. Largest payload, zero
encode cost.

Codec selection guidance and the latency math are in
[Frame video](../05-frame-video.md).

## `state` and `action`

```yaml
state:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper,   dtype: bool }
  - { name: mode,      dtype: i8 }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Field name. |
| `dtype` | string | One of `f64`, `f32`, `i32`, `i16`, `i8`, `u32`, `u16`, `u8`, `bool`. Case-insensitive. |

**Order is significant.** Both peers must declare the same fields in the same
order with the same dtypes. The schema fingerprint is computed from this list, so
any rename, reorder, or dtype change drops packets at the receiver with a
[`schema-mismatch`](../08-troubleshooting.md#schema-mismatch) warning.

Values cast to and from the declared dtype at the wire boundary. Integer overflow
saturates, so `mode: 500` into an `i8` arrives as `127`.

## `action_chunks`

A chunk is a fixed-horizon batch of typed per-field values, published as one
payload. This is the natural shape for a VLA policy that emits several future
timesteps per inference.

```yaml
action_chunks:
  - name: vla
    horizon: 16
    fields:
      - { name: joint_pos, dtype: f32 }
      - { name: gripper,   dtype: bool }
  - name: pose_targets
    horizon: 4
    fields:
      - { name: x,   dtype: f32 }
      - { name: y,   dtype: f32 }
      - { name: yaw, dtype: f32 }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Chunk name. Unique within this file. |
| `horizon` | int | Timesteps per published chunk. Must be greater than `0`. |
| `fields` | list | Per-field schema, same shape as `state` and `action`. |

Multiple chunks are allowed. Each is dispatched to its own callback by schema
fingerprint, and the fingerprint mixes in the name and horizon, so two chunks
cannot collide.

Chunks travel as byte streams rather than data packets, so a full horizon is not
bounded by the 15 KB packet limit.

## What is deliberately not in the file

Three things must be supplied at load time or set afterwards.

**`session`** is the second positional argument to `from_yaml_str` and
`from_yaml_file`.

**`role`** is the third argument to `PortalConfig.from_yaml_*`. `RobotConfig` and
`OperatorConfig` pin it for you.

**The E2EE key** is set with `cfg.set_e2ee_key(key)` after loading.

```python
import os

from livekit.portal import RobotConfig

cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
cfg.set_e2ee_key(os.environ["PORTAL_E2EE_KEY"].encode())
```

The split is intentional. A YAML file describes a wire contract, so it is meant
to be committed, shared, and templated. Identity and secrets belong in your
environment, your token-mint pipeline, or your secrets manager.

## YAML against code

The loader produces the same config you would build by hand.

| YAML field | Equivalent call |
|---|---|
| `fps` | `cfg.set_fps(...)` |
| `slack` | `cfg.set_slack(...)` |
| `tolerance` | `cfg.set_tolerance(...)` |
| `state_reliable` | `cfg.set_state_reliable(...)` |
| `action_reliable` | `cfg.set_action_reliable(...)` |
| `reuse_stale_frames` | `cfg.set_reuse_stale_frames(...)` |
| `ping_ms` | `cfg.set_ping_ms(...)` |
| `action_subscription` | `cfg.set_action_subscription(...)` |
| `videos[]` | `cfg.add_video(name, codec, quality, max_bitrate_kbps)` |
| `state[]` | `cfg.add_state_typed([...])` |
| `action[]` | `cfg.add_action_typed([...])` |
| `action_chunks[]` | `cfg.add_action_chunk(name, horizon, fields)` |

Two configs built from the same YAML and from the matching code are observably
identical: same fingerprints, same registered tracks, same sync config.

## Errors

`ConfigFileError` has four variants.

| Variant | Raised when |
|---|---|
| `Parse` | YAML parse failure. Covers unknown keys, missing `version`, wrong types, and bad codec or dtype strings. The message carries the parser position. |
| `UnsupportedVersion { got, supported }` | The file declares a `version` this build cannot read. |
| `Invalid` | Pre-flight validation failure. See the list below. |
| `Io` | `from_yaml_file` only. The file could not be opened. |

`Invalid` covers duplicate track names, duplicate chunk names, `horizon: 0`,
MJPEG quality outside `1..=100`, `max_bitrate_kbps` on a byte-stream codec or set
to zero, and `fps`, `slack`, or `tolerance` at zero or negative.

```python
from livekit.portal import ConfigFileError, RobotConfig

try:
    cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
except ConfigFileError as e:
    print(f"bad config: {e}")
    raise
```

In Python each variant is a catchable subclass:

```python
try:
    cfg = RobotConfig.from_yaml_str(yaml, "demo")
except ConfigFileError.UnsupportedVersion:
    # File format moved. Upgrade or downgrade the SDK.
    ...
except ConfigFileError.Invalid:
    # Schema bug. Surface it to a human.
    ...
except ConfigFileError as e:
    # Catch-all for parse and io.
    ...
```

## Validation walkthrough

Common mistakes and exactly what the loader does with them.

**A typo in a key.**

```yaml
version: 1
tolarance: 1.5
```

Raises `Parse`: unknown field `tolarance`. Misspellings never silently no-op.

**A duplicate track name.**

```yaml
version: 1
videos:
  - { name: cam, codec: h264 }
  - { name: cam, codec: mjpeg, quality: 80 }
```

Raises `Invalid("duplicate video track 'cam'")`. Names must be unique regardless
of codec.

**An unknown codec.**

```yaml
version: 1
videos:
  - { name: cam, codec: theora }
```

Raises `Parse`: unknown codec `theora`. The set is closed.

**A bitrate cap on a byte-stream codec.**

```yaml
version: 1
videos:
  - { name: cam, codec: mjpeg, quality: 80, max_bitrate_kbps: 4000 }
```

Raises `Invalid`, because the bitrate ceiling is a WebRTC encoder knob and there
is no encoder to cap here.

**A long-form dtype name.**

```yaml
version: 1
state:
  - { name: x, dtype: float64 }
```

Raises `Parse`: unknown dtype `float64`. Portal's names are short. Use `f64`.

**MJPEG quality out of range.**

```yaml
version: 1
videos:
  - { name: cam, codec: mjpeg, quality: 0 }
```

Raises `Invalid`. The range is `1..=100`, and omitting `quality` gives you `90`.

**A zero horizon.**

```yaml
version: 1
action_chunks:
  - { name: vla, horizon: 0, fields: [{ name: x, dtype: f32 }] }
```

Raises `Invalid("action chunk 'vla' horizon must be > 0")`.

**A future file format.**

```yaml
version: 99
```

Raises `UnsupportedVersion { got: 99, supported: 1 }`. Bumping the format is a
deliberate operation, and the loader will not misparse a future one.

## Sharing and templating

Portal configs are small enough to live next to your code or to be generated by
your deployment pipeline. Patterns people land on:

**One file in the repo.** Both sides load the same `portal.yaml` from the same
path. Put `LIVEKIT_*` in `.env.example` and let session naming flow through the
room name.

**Per-robot files.** `portal_so101.yaml`, `portal_widowx.yaml`, and so on. Pick
the file at boot. Useful when the hardware genuinely changes the wire shape.

**Generated at deploy time.** Render the YAML from Jinja, a Helm chart, or
whatever you already use, so production schemas are derived rather than
hand-edited.

**Versioned with the code.** Bump `version` and the SDK together. The loader
rejects unknown majors, so a stale peer reading a newer file fails loudly instead
of drifting.

Whatever you pick, the contract holds. Identity and secrets stay out of the file.
Everything else is fair game.

## Reference

- [Portal API](../03-portal-api.md). The programmatic equivalent.
- [Tuning](../04-tuning.md). What `fps`, `slack`, and `tolerance` actually do.
- [Frame video](../05-frame-video.md). Choosing a codec.
- [E2EE](e2ee.md). Why the key is not in the file.
