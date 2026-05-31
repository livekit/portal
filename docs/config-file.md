# Config from YAML

`PortalConfig`, `RobotConfig`, and `OperatorConfig` can be built from a
YAML file. The file describes the **shareable wire contract** —
schemas, video tracks, sync knobs. Identity (`session`, `role`, E2EE
key) is supplied at load time, so the same file is reusable across the
robot and operator processes and never holds a secret.

```python
from livekit.portal import RobotConfig

cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
```

The Rust core does the parsing and validation. Bad codec names,
unknown dtypes, duplicate track names, zero horizons, and unknown
top-level keys all raise `ConfigFileError` with a message pointing at
the offending field. The same file produces an equivalent
`PortalConfig` whether you load it from Python, the FFI, or the Rust
crate — there is exactly one parser.

A runnable end-to-end version lives at
[`examples/python/basic/`](../examples/python/basic) — `portal.yaml`
plus `robot_yaml.py` / `teleoperator_yaml.py`.

## Loading API

Every `*Config` class exposes the same two entry points.

```python
PortalConfig.from_yaml_str(yaml: str, session: str, role: Role) -> PortalConfig
PortalConfig.from_yaml_file(path: str | os.PathLike, session: str, role: Role) -> PortalConfig

RobotConfig.from_yaml_str(yaml: str, session: str) -> RobotConfig
RobotConfig.from_yaml_file(path: str | os.PathLike, session: str) -> RobotConfig

OperatorConfig.from_yaml_str(yaml: str, session: str) -> OperatorConfig
OperatorConfig.from_yaml_file(path: str | os.PathLike, session: str) -> OperatorConfig
```

`RobotConfig` / `OperatorConfig` pin the role for you. Use them
unless you have a reason to drive the unified `PortalConfig` directly.

## Quick example

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

```python
from livekit.portal import OperatorConfig, RobotConfig

robot_cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
op_cfg = OperatorConfig.from_yaml_file("portal.yaml", "session-1")
```

## Kitchen-sink example

Every field the loader understands, set explicitly. Treat this as the
"all knobs at once" reference. None of these fields are required
except `version`.

```yaml
version: 1

# Sync and pacing
fps: 30                      # observation rate
slack: 5                     # pipeline headroom (tick intervals)
tolerance: 1.5               # state/frame match window (tick intervals)

# Transport reliability
state_reliable: true         # SCTP reliable for state packets
action_reliable: true        # SCTP reliable for action packets

# Sync behavior
reuse_stale_frames: false    # freeze frames during loss instead of dropping state

# Heartbeat
ping_ms: 1000                # RTT ping cadence; 0 disables active pinging on this side

# Operator-only opt-in
action_subscription: false   # operator receives executed actions (HITL recording)

# Video tracks
videos:
  - { name: front, codec: h264 }
  - { name: wrist, codec: mjpeg, quality: 90 }
  - { name: depth, codec: png }
  - { name: raw_test, codec: raw }

# Wire schemas (state from robot, action from operator)
state:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper, dtype: bool }
  - { name: mode, dtype: i8 }

action:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper, dtype: bool }
  - { name: mode, dtype: i8 }

# VLA-style fixed-horizon batched action
action_chunks:
  - name: vla
    horizon: 16
    fields:
      - { name: joint_pos, dtype: f32 }
      - { name: gripper, dtype: bool }
```

## Per-section reference

### Top level

| Field | Type | Default | Description |
|---|---|---|---|
| `version` | int | required | Major version of the file format. Currently `1`. Unknown majors are rejected. |
| `fps` | int | `30` | Observation rate. Drives `search_range = tolerance / fps`. |
| `slack` | int | `5` | Pipeline headroom in tick intervals. |
| `tolerance` | float | `1.5` | State/frame match window in tick intervals. See [tuning](tuning.md). |
| `state_reliable` | bool | `true` | Reliable transport for state packets. |
| `action_reliable` | bool | `true` | Reliable transport for action packets. |
| `reuse_stale_frames` | bool | `false` | Reuse the most recent frame on a track when the current state has no in-range match. See [tuning](tuning.md). |
| `ping_ms` | int | `1000` | RTT ping cadence in ms. `0` disables active pinging on this side; the pong path stays active so the peer can still measure. |
| `action_subscription` | bool | `false` | Operator-side opt-in for receiving executed actions (HITL recording). No-op on the robot side. |
| `videos` | list | `[]` | Declared video tracks. See below. |
| `state` | list | `[]` | Declared state schema. List of `{name, dtype}`. |
| `action` | list | `[]` | Declared action schema. List of `{name, dtype}`. |
| `action_chunks` | list | `[]` | Declared action chunks. See below. |

Anything else at the top level is a hard error. The loader uses
`deny_unknown_fields`, so a misspelled `tolarance: 1.5` raises rather
than being silently ignored.

### `videos`

```yaml
videos:
  - { name: front,    codec: h264, max_bitrate_kbps: 8000 }  # WebRTC media path, capped at 8 Mbps
  - { name: wide,     codec: vp9 }                    # WebRTC media path, default ceiling
  - { name: wrist,    codec: mjpeg, quality: 90 }     # byte-stream, lossy
  - { name: depth,    codec: png }                    # byte-stream, lossless
  - { name: raw_cam,  codec: raw }                    # byte-stream, uncompressed RGB
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Track name. Unique across all `videos` entries. |
| `codec` | string | One of `h264`, `vp8`, `vp9`, `av1`, `h265`, `mjpeg`, `png`, `raw`. Case-insensitive (`H264` works too; `hevc` is an alias for `h265`). |
| `quality` | int (optional) | `1..=100` for `mjpeg`. Defaults to `90`. Ignored for every other codec. |
| `max_bitrate_kbps` | int (optional) | Encoder bitrate ceiling (kbps) for the WebRTC codecs. A cap, not a target. Defaults to `10000` (10 Mbps). Rejected on the byte-stream codecs. |

Codec choice picks both the encoding and the wire transport:

- **`h264` / `vp8` / `vp9` / `av1` / `h265`** — WebRTC media path. Real-time
  RTP/SRTP, lossy, best-effort delivery. libwebrtc picks the operating
  bitrate up to `max_bitrate_kbps`. Lowest end-to-end latency at scale. VP9
  and AV1 compress better than H264 at higher CPU cost; AV1 and H265 support
  is platform- and peer-dependent, so confirm both ends negotiate the codec.
- **`mjpeg`** — per-frame byte-stream, lossy. ~10-20x compression at
  q=90. Sub-millisecond decode. Each frame is independent.
- **`png`** — per-frame byte-stream, lossless. ~2-3x compression on
  natural images.
- **`raw`** — per-frame byte-stream, uncompressed RGB24. Largest
  payload, zero encode cost. Caller passes dimensions in the framing
  header.

See [frame video](frame-video.md) for codec selection guidance and
latency math.

### `state` and `action`

```yaml
state:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper,   dtype: bool }
  - { name: mode,      dtype: i8 }

action:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper,   dtype: bool }
  - { name: mode,      dtype: i8 }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Field name. |
| `dtype` | string | One of `f64`, `f32`, `i32`, `i16`, `i8`, `u32`, `u16`, `u8`, `bool`. Case-insensitive. |

**Order is significant.** Both peers must declare the same fields in
the same order, with the same dtypes. The schema fingerprint is
computed from this list — any disagreement (rename, reorder, dtype
flip) drops packets at the receiver with a warning.

Numbers cast to and from the declared dtype at the wire boundary, with
saturation on integer overflow (e.g., `mode: 500` → `i8::MAX = 127`,
flagged as saturated).

### `action_chunks`

A chunk is a fixed-horizon batch of typed per-field values published
as one packet. Use this for VLA-style policies that emit a horizon of
future actions per inference step.

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
| `name` | string | Chunk name. Unique per Portal. |
| `horizon` | int | Number of timesteps per published chunk. Must be `> 0`. |
| `fields` | list | Per-field schema, same shape as `state` / `action`. |

Multiple chunks are allowed. Each is dispatched to its own callback by
schema fingerprint, so chunk names are unique per Portal but
cross-Portal collisions are impossible by construction.

The chunk's payload travels as a LiveKit byte stream (not a data
packet), so it isn't bounded by the 15 KB packet limit.

## What's *not* in the file

Three things are deliberately omitted and must be supplied at load
time or set on the loaded config:

- **`session`** — passed as the second positional arg to
  `from_yaml_str` / `from_yaml_file`.
- **`role`** — passed as the third arg to `PortalConfig.from_yaml_*`.
  `RobotConfig` and `OperatorConfig` pin it for you.
- **`shared_key`** (E2EE) — call `cfg.set_e2ee_key(key)` after
  loading. Keep keys out of config repos.

The split is intentional. A YAML file describes a wire contract and is
meant to be checked in, shared, or templated. Identity and secrets
belong in your environment, your token-mint pipeline, or your secrets
manager.

```python
import os
from livekit.portal import RobotConfig

cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
cfg.set_e2ee_key(os.environ["PORTAL_E2EE_KEY"].encode())
```

## YAML ↔ programmatic equivalence

The loader produces the same `PortalConfig` you would build by hand.
This table maps each YAML field to the setter or builder call it
ends up making.

| YAML field | Equivalent code |
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

Two configs built from the same YAML and from the matching code path
are observably identical: same schema fingerprints, same registered
tracks, same sync config.

## Errors

`ConfigFileError` is raised in four situations:

1. **`Parse`** — YAML parse failures, including unknown top-level
   keys, missing `version`, wrong types for known keys, and bad
   codec/dtype strings. The message contains the position the parser
   was at when it gave up.
2. **`UnsupportedVersion { got, supported }`** — the file declares a
   `version` the build doesn't know how to read.
3. **`Invalid`** — pre-flight validation failures. Duplicate track
   names, duplicate chunk names, `horizon: 0`, MJPEG quality outside
   `1..=100`, `max_bitrate_kbps` on a byte-stream codec or set to zero,
   `fps` / `slack` / `tolerance` set to zero or negative.
4. **`Io`** — `from_yaml_file` only. The file couldn't be opened.

```python
from livekit.portal import ConfigFileError, RobotConfig

try:
    cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
except ConfigFileError as e:
    print(f"bad config: {e}")
    raise
```

In Python, `ConfigFileError` is a `flat_error` (UniFFI), so each
variant is a subclass you can catch individually if you want to:

```python
try:
    cfg = RobotConfig.from_yaml_str(yaml, "demo")
except ConfigFileError.UnsupportedVersion as e:
    # Bumped the file format; downgrade or upgrade the SDK.
    ...
except ConfigFileError.Invalid as e:
    # Schema bug. Surface to the human.
    ...
except ConfigFileError as e:
    # Catch-all (parse, io).
    ...
```

## Validation walk-through

A handful of common mistakes and what the loader does with them.

```yaml
# tolarance is a typo of tolerance.
version: 1
tolarance: 1.5
```

→ `ConfigFileError.Parse(...)`: unknown field `tolarance`. The
parser uses `deny_unknown_fields` everywhere, so misspellings never
silently no-op.

```yaml
version: 1
videos:
  - { name: cam, codec: h264 }
  - { name: cam, codec: mjpeg, quality: 80 }
```

→ `ConfigFileError.Invalid("duplicate video track 'cam'")`. Track
names must be unique across all `videos` entries regardless of
codec.

```yaml
version: 1
videos:
  - { name: cam, codec: theora }
```

→ `ConfigFileError.Parse(...)`: unknown codec `theora`. The codec set
is closed: `h264`, `vp8`, `vp9`, `av1`, `h265`, `raw`, `png`, `mjpeg`.

```yaml
version: 1
videos:
  - { name: cam, codec: mjpeg, quality: 80, max_bitrate_kbps: 4000 }
```

→ `ConfigFileError.Invalid("video 'cam': max_bitrate_kbps applies to
the WebRTC codecs only, not Mjpeg")`. The bitrate ceiling is a WebRTC
encoder knob; the byte-stream codecs reject it.

```yaml
version: 1
state:
  - { name: x, dtype: float64 }
```

→ `ConfigFileError.Parse(...)`: unknown dtype `float64`. The Portal
dtype names are short: `f64`, `f32`, `i32`, `i16`, `i8`, `u32`,
`u16`, `u8`, `bool`.

```yaml
version: 1
videos:
  - { name: cam, codec: mjpeg, quality: 0 }
```

→ `ConfigFileError.Invalid("video 'cam': mjpeg quality must be in
1..=100, got 0")`. Quality range is enforced for MJPEG only;
omitting `quality` falls back to `90`.

```yaml
version: 1
action_chunks:
  - { name: vla, horizon: 0, fields: [{ name: x, dtype: f32 }] }
```

→ `ConfigFileError.Invalid("action chunk 'vla' horizon must be > 0")`.

```yaml
version: 99
```

→ `ConfigFileError.UnsupportedVersion { got: 99, supported: 1 }`.
Bumping the file format is a deliberate, named operation; the
loader will not silently misparse a future format.

## Sharing and templating

Portal configs are deliberately small enough to live next to your
code or to be templated by your deployment pipeline. A few patterns
people land on:

- **Single file in the repo.** Both robot and operator load the same
  `portal.yaml` from the same path. Add `LIVEKIT_*` to `.env.example`
  and let session naming flow through the room name.
- **Per-robot file plus a template.** One `portal_so101.yaml`,
  `portal_widowx.yaml`, etc. Pick the file at boot. Useful when the
  hardware genuinely changes the wire shape.
- **Generated at deploy time.** Render the YAML from a templating
  layer (Jinja, Helm chart, etc.) so production schemas are derived
  rather than hand-edited.
- **Versioned alongside the code.** Bump `version` (and the SDK) in
  lockstep. The loader rejects unknown majors, so a stale operator
  trying to read a newer file fails loudly instead of silently
  drifting.

Whatever you pick, the contract is the same: identity and secrets
stay out of the file, everything else is fair game.
