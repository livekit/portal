# Config from YAML

`PortalConfig`, `RobotConfig`, and `OperatorConfig` can be built from a YAML
file. The YAML describes the **shareable wire contract** — schemas, video
tracks, sync knobs. Identity (`session`, `role`, E2EE key) is supplied at
load time so the same file is reusable across the robot side and the
operator side and contains no secrets.

```python
from livekit.portal import RobotConfig

cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
robot = Robot(cfg)
```

The Rust core does the parsing and validation. Bad codec names, unknown
dtypes, duplicate track names, zero horizons, and unknown top-level keys
all raise `ConfigFileError` with a message pointing at the offending
field. The same file produces an equivalent `PortalConfig` whether you
load it from Python, the FFI, or the Rust crate directly — there's only
one parser.

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

A runnable end-to-end version lives at
[`examples/python/basic/`](../examples/python/basic) — `portal.yaml` plus
`robot_yaml.py` / `teleoperator_yaml.py`.

## Schema reference

### Top level

| Field | Type | Default | Description |
|---|---|---|---|
| `version` | int | required | Major version of the file format. Currently `1`. Unknown majors are rejected. |
| `fps` | int | `30` | Observation rate. Drives `search_range = tolerance / fps`. |
| `slack` | int | `5` | Pipeline headroom in tick intervals. |
| `tolerance` | float | `1.5` | State/frame match window in tick intervals. See [tuning](tuning.md). |
| `state_reliable` | bool | `true` | Reliable transport for state packets. |
| `action_reliable` | bool | `true` | Reliable transport for action packets. |
| `reuse_stale_frames` | bool | `false` | Reuse the most recent frame on a track when the current state has no in-range match. |
| `ping_ms` | int | `1000` | RTT ping cadence in ms. `0` disables active pinging on this side. |
| `action_subscription` | bool | `false` | Operator-side opt-in for receiving executed actions (HITL recording). No-op on the robot side. |
| `videos` | list | `[]` | Declared video tracks. See below. |
| `state` | list | `[]` | Declared state schema. List of `{name, dtype}`. |
| `action` | list | `[]` | Declared action schema. List of `{name, dtype}`. |
| `action_chunks` | list | `[]` | Declared action chunks. See below. |

### `videos`

```yaml
videos:
  - { name: front, codec: h264 }
  - { name: wrist, codec: mjpeg, quality: 90 }
  - { name: depth, codec: png }
  - { name: raw, codec: raw }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Track name. Unique across all `videos` entries. |
| `codec` | string | One of `h264`, `mjpeg`, `png`, `raw`. Case-insensitive. `h264` rides the WebRTC media path; the others ride per-frame byte streams. |
| `quality` | int (optional) | `1..=100` for `mjpeg`. Defaults to `90`. Ignored for `raw`, `png`, `h264`. |

### `state` and `action`

```yaml
state:
  - { name: joint_pos, dtype: f32 }
  - { name: gripper, dtype: bool }
  - { name: mode, dtype: i8 }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Field name. |
| `dtype` | string | One of `f64`, `f32`, `i32`, `i16`, `i8`, `u32`, `u16`, `u8`, `bool`. Case-insensitive. |

Order is significant. Both peers must declare the same fields in the
same order, or the schema fingerprint differs and packets are dropped.

### `action_chunks`

```yaml
action_chunks:
  - name: vla
    horizon: 16
    fields:
      - { name: joint_pos, dtype: f32 }
      - { name: gripper, dtype: bool }
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Chunk name. Unique per Portal. |
| `horizon` | int | Number of timesteps per published chunk. Must be `> 0`. |
| `fields` | list | Per-field schema, same shape as `state` / `action`. |

## What's not in the file

Three things are deliberately omitted and must be supplied at load time
or set on the loaded config:

- **`session`** — passed as the second positional arg to
  `from_yaml_str` / `from_yaml_file`.
- **`role`** — passed as the third arg to `PortalConfig.from_yaml_*`.
  `RobotConfig` and `OperatorConfig` pin it for you.
- **`shared_key`** (E2EE) — call `cfg.set_e2ee_key(key)` after loading.
  Keep keys out of config repos.

## Errors

`ConfigFileError` is raised on:

- YAML parse failures (malformed, unknown top-level keys, missing
  `version`, wrong types).
- Unknown major version.
- Validation failures: duplicate track names, duplicate chunk names,
  zero horizon, out-of-range MJPEG quality, `fps` / `slack` /
  `tolerance` set to zero or negative.

Catch it like any other exception:

```python
from livekit.portal import ConfigFileError, RobotConfig

try:
    cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
except ConfigFileError as e:
    print(f"bad config: {e}")
    raise
```
