# Logging

This page explains how `livekit-portal` logs and what each message means.
Warnings and errors carry a short reference tag in square brackets. Find the
tag below for the cause and the fix.

## How logging works

The library logs through the Rust [`log`](https://docs.rs/log) facade. The
FFI layer initializes [`env_logger`](https://docs.rs/env_logger) once when
the module loads. The default level is `info`. Timestamps print with
millisecond precision.

Set verbosity with the `RUST_LOG` environment variable before importing the
library.

```bash
RUST_LOG=info python robot.py            # default: lifecycle + warnings
RUST_LOG=warn python robot.py            # warnings and errors only
RUST_LOG=livekit_portal=debug python ... # everything from this crate
RUST_LOG=off python robot.py             # silence the library
```

The target name is the crate path. Use `livekit_portal` to scope a level to
this library and leave other crates alone.

## Log levels

| Level | What you get |
|-------|--------------|
| `error` | A user callback panicked, or a send failed outright. The loop keeps running. |
| `warn` | Something was dropped or ignored. The session continues, but quality or completeness suffered. |
| `info` | Connection lifecycle. Connect, disconnect, track published, publisher ready. |
| `debug` | Per-event detail. Off by default. |

A healthy session at `info` is quiet after the startup lines. A steady run
of `warn` lines means a stream or a buffer is under pressure.

## Reference tags

Every warning and error starts with a tag like `[sync-drop]`. The tag names
the root cause, not the call site. Several messages can share one tag.

To troubleshoot, copy the tag and find its section on this page. Each
section is anchored by its tag. For `[sync-drop]`, that is
`docs/logging.md#sync-drop`.

Lifecycle `info` lines are not tagged. They are listed under
[Connection lifecycle](#connection-lifecycle).

## Logs vs metrics

Logs tell you that something happened. Metrics tell you how much and how
often. Every drop logged here is also counted in `portal.metrics()`, and
that counter never throttles. When a warning is rate-limited, the metric is
the source of truth for the exact total.

Reach for `metrics()` when you want a number. Reach for the tag when you want
the cause and the fix. See [Tuning](tuning.md) for the counters and the
knobs they map to.

## Tag reference

### sync-drop

```
[sync-drop] dropping states: no frame within ±10ms of the state timestamp (video 47ms ahead). Throttling further [sync-drop] warnings to once per 5s.
[sync-drop] dropped 33 more states in 5s: no frame within ±10ms (video up to 51ms ahead).
```

A state was dropped because no video frame landed inside its match window.
The window is `±tolerance` ticks wide. The "video ahead" number is how far
the video stream had already moved past the dropped state, which is why
nothing matched.

The first drop in a burst logs at once. Further drops fold into a summary
emitted at most once every 5 seconds. `metrics.sync.states_dropped` counts
every one.

**Cause.** Video arrived later than state, stalled, or jittered by more than
the match window. State kept flowing while video did not.

**Fix.**
- Raise `tolerance` to widen the match window. Aim for `tolerance ≥ 1` tick.
- Raise `slack` to buffer through longer stalls.
- Enable `reuse_stale_frames` to freeze on the last good frame instead of dropping. Use this for data collection. Leave it off for real-time control.
- Check `metrics.sync.last_blocker_track` to see which camera is behind.

See [Choosing `tolerance`](tuning.md#choosing-tolerance).

### state-overflow

```
[state-overflow] state buffer full (5), dropped 2 oldest. Further drops in this burst won't be re-logged.
```

The state buffer hit its cap and shed its oldest entries. States piled up
with no video frame to match against. This usually means a video track has
stalled completely. It logs once per burst, not once per drop.

**Fix.** Raise `slack` to tolerate longer stalls. Enable `reuse_stale_frames`
if a frozen frame is acceptable. If video stopped entirely, the fix is at the
robot, not in the buffer.

### video-overflow

```
[video-overflow] 'front' buffer full, evicted 3 frame(s)
```

A video track's buffer hit its cap and dropped its oldest frames. This is
normal when video arrives faster than state. The newest frames are kept, so
sync still works.

**Fix.** Usually nothing. If it pairs with `[sync-drop]`, the buffer is too
shallow to bridge the two rates. Raise `slack`. The cumulative count is
`metrics.buffers.evictions`.

### publish-full

```
[publish-full] topic 'state' queue full (cap=1024), dropping packet
[publish-full] frame_video 'front' queue full (cap=1024), dropping frame
```

The outbound queue for a topic, chunk, or frame-video track filled up and a
packet was dropped before it left the machine. The link cannot ship data as
fast as you are producing it.

**Fix.** Lower the publish rate, the resolution, or the frame rate. Check the
network. A persistent warning is sustained backpressure, not a spike. For
frame video, track it with `metrics.transport.frames_dropped_publisher_full`.
For lossy transport that sheds load instead of queuing, use a WebRTC video
track. See [Frame video](frame-video.md).

### publish-failed

```
[publish-failed] data publish failed: <error>
[publish-failed] chunk 'grip' byte stream failed: <error>
[publish-failed] rtt publish failed: <error>
```

A publish call returned an error from the transport. The room is
disconnected, or the participant is gone.

**Fix.** Expect these around a reconnect. If they persist, the session is not
connected. Check connectivity and the token.

### schema-mismatch

```
[schema-mismatch] topic 'state': peer schema 0xAABBCCDD != ours 0x11223344, dropping packet
```

The sender and receiver declared different schemas for the same topic. The
packet is dropped because the layout cannot be trusted. Logged once per
unique mismatch.

**Fix.** Make the robot and operator declare the same fields, in the same
order, with the same dtypes. A shared YAML config is the reliable way. See
[Config from YAML](config-file.md).

### unknown-field

```
[unknown-field] topic 'state': field 'gripper2' not in schema, ignored
```

You sent a field the schema does not declare. The field is dropped. The rest
of the packet is sent. Logged once per offending key.

**Fix.** Add the field to the schema, or stop sending it. Check for a typo in
the field name.

### saturated

```
[saturated] topic 'state': field 'angle' clamped to U8 range
```

A value did not fit the field's declared dtype and was clamped to the dtype
range. For example, a value above 255 sent as a `U8`. Logged once per field.

**Fix.** Widen the dtype, or scale the value before sending.

### unknown-chunk

```
[unknown-chunk] on_action_chunk: chunk 'grip' not declared, callback ignored
[unknown-chunk] topic 'portal_action_chunk': unknown fingerprint 0x1A2B3C4D, dropping byte stream
```

A chunk name or fingerprint does not match any declared chunk. Either you
registered a callback for a chunk that was never declared, or a byte stream
arrived for a chunk the receiver does not know. Receive-side warnings are
capped at 256 unique fingerprints, then suppressed.

**Fix.** Declare the chunk with the same name on both ends before registering
the callback or sending.

### unknown-track

```
[unknown-track] on_video_frame: track 'side' not registered, callback ignored
[unknown-track] frame_video: track 'side' not declared, dropping frame
```

A video track name does not match any declared track. Either you registered
a callback for an unknown track, or a frame arrived for a track the receiver
does not know.

**Fix.** Declare the track with `add_video` using the same name on both ends.

### codec-mismatch

```
[codec-mismatch] frame_video 'front': declared Mjpeg, got Png, dropping frame
```

A frame arrived encoded with a codec that does not match the track's declared
codec. The frame is dropped.

**Fix.** Declare the same codec on both ends with `add_video`.

### decode-failed

```
[decode-failed] frame_video 'front': decode failed: <error>
```

A frame's payload could not be decoded with the declared codec. The payload
is malformed or truncated. The frame is dropped and the loop continues.

**Fix.** A few of these around a reconnect are harmless. A steady stream
points at a codec or encoder problem on the sender.

### bad-payload

```
[bad-payload] frame_video: bad header (<error>)
[bad-payload] state deserialize failed: <error>
[bad-payload] failed to read chunk byte stream: <error>
```

A received payload could not be parsed. The header was malformed, the body
failed to deserialize, or a byte stream read errored. The packet or frame is
dropped and the loop continues.

**Fix.** Usually a transport hiccup or a version skew between peers. If it
persists, confirm both ends run the same portal version and the same schema.

### callback-panic

```
[callback-panic] observation callback panicked, event loop continues
```

One of your registered callbacks raised. The library catches the panic so it
does not take down the event loop, logs it, and keeps running. This covers
`on_observation`, `on_drop`, `on_state`, `on_action`, the video-frame
callbacks, and the operator-roster callbacks.

**Fix.** Fix the exception in your callback. The library cannot report the
line, so wrap the callback body in `try`/`except` and print a traceback to
find it.

## Connection lifecycle

These `info` lines are not problems. They confirm the session is wired up.
They are not tagged.

| Message | Meaning |
|---------|---------|
| `[SESSION] connecting as ROLE to URL` | Connection attempt started. |
| `[SESSION] connected as ROLE` | Connection succeeded. |
| `[SESSION] published video track 'TRACK'` | A robot video track went live. |
| `[SESSION] ready to publish state via MODE data (N fields)` | The state publisher is set up. |
| `[SESSION] subscribed to video track 'TRACK'` | The operator is receiving a robot track. |
| `disconnecting` | Disconnect started. |

`SESSION` here is the session id, not a reference tag. If you connect and
then see no `subscribed` or `ready` lines, the two peers are not seeing each
other. Check the room name and the token.
</content>
