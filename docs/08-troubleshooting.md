# Troubleshooting

> Start from the symptom. Or, if you have a tagged warning, jump to its tag.

Every warning and error Portal logs carries a short tag in square brackets, like
`[sync-drop]`. The tag names the root cause, not the call site. Find it in the
[tag reference](#tag-reference) below for the cause and the fix.

If you do not have a tag yet, start with [common symptoms](#common-symptoms).

## Turning on logs

Portal logs through the Rust [`log`](https://docs.rs/log) facade. The FFI layer
initializes [`env_logger`](https://docs.rs/env_logger) once when the module
loads. The default level is `info`.

Set `RUST_LOG` **before** importing the library.

```bash
RUST_LOG=info python robot.py               # default: lifecycle plus warnings
RUST_LOG=warn python robot.py               # warnings and errors only
RUST_LOG=livekit_portal=debug python ...     # everything from this crate
RUST_LOG=off python robot.py                # silence it
```

Use the `livekit_portal` target to scope a level to this library and leave other
crates alone.

| Level | What you get |
|---|---|
| `error` | A callback panicked, or a send failed outright. The loop keeps running. |
| `warn` | Something was dropped or ignored. The session continues with reduced quality. |
| `info` | Connection lifecycle. Connect, disconnect, track published, publisher ready. |
| `debug` | Per-event detail. Off by default. |

A healthy session at `info` goes quiet after the startup lines. A steady run of
`warn` lines means a stream or a buffer is under pressure.

## Common symptoms

### Nothing is arriving

The operator connects and `on_observation` never fires. Work down this list.

**1. Did anyone claim control?** The robot starts with **no active operator** and
drops every action. This is the most common first-run problem, and it produces no
error at all.

```python
await op.set_active_operator(op.local_identity())
print(op.active_operator())   # should print your identity
```

**2. Are both sides in the same room?** The `session` string you pass to
`RobotConfig` is a **log label only**. The actual room comes from your token. Two
peers with the same `session` and different token rooms never meet.

```python
print(op.robot_identity())   # None means the robot was never discovered
```

**3. Do the schemas match exactly?** Same field names, same order, same dtypes.
Look for [`schema-mismatch`](#schema-mismatch) in the logs. A
[shared YAML file](reference/config-file.md) removes the possibility.

**4. Do the camera names match?** An observation needs **every** declared track
to have a frame. A camera declared on the operator and never published by the
robot means observations never fire. Check `metrics.sync.last_blocker_track` to
see which one is holding things up.

**5. Is state arriving at all?** Register `on_state` temporarily. It fires on
every state packet with no matching involved.

```python
op.on_state(lambda s: print("state", s.values))
```

If `on_state` fires and `on_observation` does not, the problem is matching. If
neither fires, the problem is transport or the room.

### Observations fire, but frames or fields are missing

A `KeyError` on `obs.frames["cam1"]` means that track was never declared with
`add_video` on this side. Declared tracks are the only keys present.

A field missing from `obs.state` means it is not in your declared schema. If the
robot sends it and you never declared it, the robot logs
[`unknown-field`](#unknown-field) and drops it.

### Connect fails with a metadata error

```
failed to publish role attribute (token may be missing canUpdateOwnMetadata)
```

Your token omitted `can_update_own_metadata`. Both `Robot` and `Operator`
self-set an `lk.portal.role` attribute on connect so peers can discover them, and
that write needs the grant.

```python
grants = api.VideoGrants(
    room_join=True,
    room=room,
    can_publish=True,
    can_subscribe=True,
    can_update_own_metadata=True,   # <- add this
)
```

### The robot receives no actions and reports no errors

The active-operator pointer is unset or naming someone else. Inactive operators
get no error, no exception, and no callback. Their packets reach the robot and are
dropped at the gate.

```python
print(op.active_operator())     # None, or someone else
await op.set_active_operator(op.local_identity())
```

If you are on the lerobot plugins with `auto_claim_control=False`, something else
has to claim on your behalf.

### `ImportError` or `ffi not initialized`

The native library did not load. Rebuild it:

```bash
bash scripts/build_ffi_python.sh release
```

Or point `LIVEKIT_PORTAL_FFI_LIB` at a prebuilt binary. If you installed from
PyPI on an unsupported platform there is no wheel, and you need a
[source build](01-quickstart.md#build-from-source).

### Latency is higher than RTT suggests

`rtt.rtt_us_p95` is pure network round trip. It is not your loop latency. The
number you want is `policy.e2e_us_p95`, which includes the match wait and your
inference time.

If `policy.e2e_us_p95` is `None`, you are not passing `in_reply_to_ts_us`. See
[Metrics: policy](07-metrics.md#policy).

If it is populated and large, decompose it. Subtract RTT to get everything that
is not network. Compare the remainder against your inference time. Frame video
adds a per-chunk floor on top, covered in
[Frame video](05-frame-video.md#the-latency-floor).

### Frames arrive at a resolution that changes mid-session

This is libwebrtc adapting, not Portal resizing anything. On the WebRTC codecs
the encoder defaults to `MAINTAIN_FRAMERATE`. Under CPU overuse or bandwidth
pressure it holds the frame rate and scales the frame down instead, then scales
back up once the pressure clears.

Confirm it by logging `frame.width` and `frame.height` in `on_video_frame`. A
size that drifts up and down over a session is adaptation. A single change at
startup is something else.

Pin the geometry by marking the source as screen content:

```python
cfg.add_video("front", screencast=True)
```

That switches libwebrtc to `MAINTAIN_RESOLUTION`, which drops frames under
pressure rather than rescaling. You trade smooth motion for a constant frame
shape. See
[Simulcast and screencast](03-portal-api.md#simulcast-and-screencast).

Worth checking alongside it: sustained encoder pressure usually means the CPU
is saturated or `max_bitrate_kbps` is set too low for the resolution and rate
you are asking for.

### Video looks wrong after enabling E2EE

If one peer has no key or a different key, decryption fails silently. Video goes
black or corrupt and data packets do not parse. There is no handshake error.

Confirm both sides load identical key bytes. See
[E2EE](reference/e2ee.md#mismatched-or-missing-key).

## Tag reference

### sync-drop

```
[sync-drop] dropping states: no frame within ±10ms of the state timestamp (video 47ms ahead). Throttling further [sync-drop] warnings to once per 5s.
[sync-drop] dropped 33 more states in 5s: no frame within ±10ms (video up to 51ms ahead).
```

A state was dropped because no video frame landed inside its match window. The
"video ahead" number is how far the video stream had already moved past the
dropped state, which is why nothing matched.

The first drop in a burst logs immediately. Further drops fold into a summary at
most once every five seconds. `metrics.sync.states_dropped` counts every one.

**Cause.** Video arrived later than state, stalled, or jittered by more than the
match window. State kept flowing while video did not.

**Fix.**

- Raise `tolerance` to widen the window. Keep it at 1 or above.
- Raise `slack` to buffer through longer stalls.
- Enable `reuse_stale_frames` to freeze on the last good frame instead of
  dropping. Good for data collection. Wrong for real-time control.
- Check `metrics.sync.last_blocker_track` to see which camera is behind.

See [Choosing `tolerance`](04-tuning.md#choosing-tolerance).

### state-overflow

```
[state-overflow] state buffer full (5), dropped 2 oldest. Further drops in this burst won't be re-logged.
```

The state buffer hit its cap and shed its oldest entries. States piled up with no
frame to match against, which usually means a video track stalled completely.
Logs once per burst.

**Fix.** Raise `slack` to tolerate longer stalls. Enable `reuse_stale_frames` if a
frozen frame is acceptable. If video stopped entirely, the fix is at the robot,
not in the buffer.

### video-overflow

```
[video-overflow] 'front' buffer full, evicted 3 frame(s)
```

A video track's buffer hit its cap and dropped its oldest frames. This is normal
when video arrives faster than state. The newest frames are kept, so matching
still works.

**Fix.** Usually nothing. If it pairs with [`sync-drop`](#sync-drop), the buffer
is too shallow to bridge the two rates, so raise `slack`. The cumulative count is
`metrics.buffers.evictions`.

### recv-overflow

```
[recv-overflow] 'front' frame processing is behind; dropped 120 frame(s) so far to keep the receive loop draining. A slow on-frame or on-observation callback is the usual cause.
```

Decoded frames arrive faster than they can be processed. Each subscribed track
drains libwebrtc's native queue on one task and does the per-frame work on
another, connected by a small bounded channel. When processing falls behind and
that channel fills, the oldest queued frame is dropped so the drain keeps pulling.

This sheds frames at the SDK boundary deliberately. The alternative is letting the
native queue overflow and flush thousands of frames at once.

**Fix.** Make the per-frame path faster. A heavy or blocking `on_video_frame` or
`on_observation` callback is the usual cause. Queue the work, downsample, or move
CPU-bound and GIL-bound processing to another thread so the callback returns
quickly. A steady stream of these means frames are being shed continuously, not
in a one-off burst.

### publish-full

```
[publish-full] topic 'state' queue full (cap=1024), dropping packet
[publish-full] frame_video 'front' queue full (cap=1024), dropping frame
```

The outbound queue for a topic, chunk, or frame-video track filled and a packet
was dropped before leaving the machine. The link cannot ship data as fast as you
produce it.

**Fix.** Lower the publish rate, the resolution, or the frame rate. Check the
network. A persistent warning is sustained backpressure, not a spike. Track it
with `metrics.transport.frames_dropped_publisher_full`. If you would rather shed
load than queue it, use a WebRTC video track instead of frame video.

### publish-failed

```
[publish-failed] data publish failed: <error>
[publish-failed] chunk 'grip' byte stream failed: <error>
[publish-failed] rtt publish failed: <error>
```

A publish call returned a transport error. The room is disconnected, or the peer
is gone.

**Fix.** Expect a few around a reconnect. If they persist, the session is not
connected. Check connectivity and the token TTL.

### schema-mismatch

```
[schema-mismatch] topic 'state': peer schema 0xAABBCCDD != ours 0x11223344, dropping packet
```

The sender and receiver declared different schemas for the same topic. The packet
is dropped because its layout cannot be trusted. Logged once per unique mismatch.

**Cause.** Any rename, reorder, or dtype change on one side. Field **order**
counts, which is the part that catches people out.

**Fix.** Make both sides declare the same fields, in the same order, with the same
dtypes. A shared [YAML config](reference/config-file.md) is the reliable way to
guarantee it.

### unknown-field

```
[unknown-field] topic 'state': field 'gripper2' not in schema, ignored
```

You sent a field the schema does not declare. That field is dropped and the rest
of the packet is sent. Logged once per offending key.

**Fix.** Add the field to the schema on both sides, or stop sending it. A typo is
the usual cause, and a typo looks exactly like a field that never arrives.

### saturated

```
[saturated] topic 'state': field 'angle' clamped to U8 range
```

A value did not fit its declared dtype and was clamped to that dtype's range. For
example, 300 sent as a `U8` becomes 255. Logged once per field, then silent.

The peer receives the clamped value and never learns the original. `NaN` becomes
`0` for integer dtypes and `false` for `Bool`, and both count as saturation.

**Fix.** Widen the dtype, or scale the value before sending.

### unknown-chunk

```
[unknown-chunk] on_action_chunk: chunk 'grip' not declared, callback ignored
[unknown-chunk] topic 'portal_action_chunk': unknown fingerprint 0x1A2B3C4D, dropping byte stream
```

A chunk name or fingerprint matches no declared chunk. Either you registered a
callback for a chunk that was never declared, or a byte stream arrived for a chunk
this side does not know. Receive-side warnings cap at 256 unique fingerprints,
then suppress.

**Fix.** Declare the chunk with the same name, horizon, and fields on both ends
before registering the callback or sending.

### unknown-track

```
[unknown-track] on_video_frame: track 'side' not registered, callback ignored
[unknown-track] frame_video: track 'side' not declared, dropping frame
```

A video track name matches no declared track.

**Fix.** Declare the track with `add_video` using the same name on both ends.

### codec-mismatch

```
[codec-mismatch] frame_video 'front': declared Mjpeg, got Png, dropping frame
```

A frame arrived encoded with a codec that disagrees with the track's declared
codec. The frame is dropped.

**Fix.** Declare the same codec on both ends.

### decode-failed

```
[decode-failed] frame_video 'front': decode failed: <error>
```

A frame's payload could not be decoded with the declared codec. It is malformed
or truncated. The frame is dropped and the loop continues.

**Fix.** A few around a reconnect are harmless. A steady stream points at a codec
or encoder problem on the sender.

### bad-payload

```
[bad-payload] frame_video: bad header (<error>)
[bad-payload] state deserialize failed: <error>
[bad-payload] failed to read chunk byte stream: <error>
```

A received payload could not be parsed. The header was malformed, the body failed
to deserialize, or a byte-stream read errored. The packet is dropped and the loop
continues.

**Fix.** Usually a transport hiccup or version skew between peers. If it persists,
confirm both ends run the same Portal version and the same schema.

### callback-panic

```
[callback-panic] observation callback panicked, event loop continues
```

One of your callbacks raised. Portal catches it so it cannot take down the event
loop, logs it, and keeps running. This covers `on_observation`, `on_drop`,
`on_state`, `on_action`, the video-frame callbacks, and the operator-roster
callbacks.

**Fix.** Fix the exception. Portal cannot report the line number, so wrap the
callback body while debugging:

```python
def on_observation(obs):
    try:
        ...
    except Exception:
        import traceback
        traceback.print_exc()
```

## Lifecycle lines

These `info` lines are not problems. They confirm the session is wired up, and
they are not tagged. `SESSION` below is your session label, not a tag.

| Message | Meaning |
|---|---|
| `[SESSION] connecting as ROLE to URL` | Connection attempt started. |
| `[SESSION] connected as ROLE` | Connection succeeded. |
| `[SESSION] published video track 'TRACK'` | A robot WebRTC track went live. |
| `[SESSION] ready to publish frame-video track 'TRACK' via byte stream` | A byte-stream video track is set up. |
| `[SESSION] ready to publish state via MODE data (N fields)` | The state publisher is set up. |
| `[SESSION] ready to publish action via MODE data (N fields)` | The action publisher is set up. |
| `[SESSION] ready to publish chunk 'NAME' via byte stream` | A chunk publisher is set up. |
| `[SESSION] subscribed to video track 'TRACK'` | The operator is receiving a robot track. |
| `[SESSION] participant 'ID' disconnected` | Someone left the room. |
| `[SESSION] reconnected, clearing sync buffers and latest slots` | The session recovered. Buffers were flushed. |
| `disconnecting` | Disconnect started. |

If you connect and then see no `subscribed` or `ready` lines, the two peers are
not seeing each other. Check the room name in the token, then the schemas.

## Logs against metrics

Warnings are throttled, so a log is never a count. Counters never throttle, so
`portal.metrics()` is the source of truth for totals.

Reach for the tag when you want the cause and the fix. Reach for the counter when
you want the number. Every field is in [Metrics](07-metrics.md).

## Next steps

- [Metrics](07-metrics.md). The counters behind these warnings.
- [Tuning](04-tuning.md). The knobs most of these fixes point at.
- [Config from YAML](reference/config-file.md). Removes schema mismatch as a
  class of bug.
