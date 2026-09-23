# Wire protocol

> The bytes on the wire. Everything needed to build a Portal peer in a LiveKit
> SDK with no Portal plugin.

Every other page describes the SDK surface. This one describes the wire. A peer
that follows everything here interoperates with a Portal `Robot` or `Operator`
without sharing any code.

Use it for a browser teleop UI, a Go service, a Swift app, or anything else that
speaks LiveKit but cannot import the Rust core or the Python package.

Portal adds no new transport. It rides four standard LiveKit primitives: reliable
data packets, byte streams, WebRTC video tracks, and participant attributes. The
contract is the set of topic names, attribute keys, RPC names, and binary layouts
below.

## Transport map

Each logical channel maps to one LiveKit primitive on one reserved topic.

| Channel | Topic | LiveKit primitive | Reliable | Publisher |
|---|---|---|---|---|
| State | `portal_state` | data packet | yes | robot |
| Action | `portal_action` | data packet | yes | operator |
| Frame video | `portal_frame_video` | byte stream | yes | robot |
| Clock | `portal_clock` | data packet | no | robot answers, others ping |
| Keypoint | `portal_keypoint` | data packet | yes | any role |
| WebRTC video | the track name | media track | n/a | robot |

All topic names are exact, case-sensitive literals. A peer must filter incoming
data and stream events by topic and ignore everything else. Other applications can
share the room on other topics without interfering.

The publisher column is the default direction. With
[operator-side action subscription](../03-portal-api.md#operator-side-action-subscription)
on, an operator also reads `portal_action`.

## Identity, roles, and discovery

The room name and participant identities come from the access token, not from
Portal. The `session` string in Portal config is a local log label and is never
compared against the room name. Pick identities at token-mint time.

On connect, every Portal peer self-sets its role and protocol version.

| Attribute key | Value | Set by |
|---|---|---|
| `lk.portal.role` | `"robot"`, `"operator"` or `"observer"` | every peer, on connect |
| `lk.portal.version` | protocol version, currently `"3"` | every peer, on connect |
| `lk.portal.active_operator` | active operator identity, or `""` | robot only |

Discovery falls out of that. The robot is the remote participant with
`lk.portal.role == "robot"`, and there is at most one. Operators are every
participant with `lk.portal.role == "operator"`, and observers every participant
with `lk.portal.role == "observer"`. Observers never publish state or actions;
they subscribe like operators. A participant with no `lk.portal.role` is not a
Portal peer and should be ignored.

Peers only recognise each other when `lk.portal.version` matches exactly. A
peer with a missing or different version is logged as `[version-mismatch]` and
left out of every roster. A robot also drops actions from any sender that is not
a recognised operator, even if the active-operator pointer names it.

Both are plain participant attributes. Read them from the
participant-attributes-changed event, and from the initial participant list on
join. To act as a robot, set `lk.portal.role` to `"robot"` yourself after
connecting.

**Token requirements.** Robot and operator tokens must grant
`can_update_own_metadata = true`, because that self-set write fails without it. A
token may also seed the robot's `lk.portal.active_operator` at mint time so the
pointer is set before anyone connects.

## State and action packets

Both are reliable LiveKit data packets sharing a little-endian layout. Action
carries one extra header field.

State, on `portal_state`:

```
[u32 fingerprint        little-endian]
[u64 timestamp_us       little-endian]
[field 0 bytes]
[field 1 bytes]
...
```

Action, on `portal_action`:

```
[u32 fingerprint        little-endian]
[u64 timestamp_us       little-endian]
[u64 in_reply_to_ts_us  little-endian]   # 0 means no correlation
[field 0 bytes]
[field 1 bytes]
...
```

The fixed header is 12 bytes for state and 20 for action.

Field bytes follow in declared schema order. Each field's width is fixed by its
dtype. There is no per-field tag and no per-field length. **The schema, shared out
of band, is the only thing that makes the payload parseable.**

`timestamp_us` is microseconds since the Unix epoch on the sender's clock.

`in_reply_to_ts_us` lets an operator stamp which observation an action answers.
`0` is the no-correlation sentinel, which is safe because a real epoch timestamp
is never zero.

The `fingerprint` is your parse gate. Compute it from your local schema. On
receive, if a packet's fingerprint does not match what you expect for that topic,
**drop the packet and do not attempt to parse it.** A mismatch means the peer's
schema disagrees with yours, and the layout cannot be trusted.

## Schema fingerprint

A 32-bit FNV-1a hash over the ordered field names and dtype tags. It detects any
rename, dtype change, or reorder. Both peers must compute it identically or all
traffic drops.

Constants are the FNV-1a 32-bit standard: offset basis `0x811c9dc5`, prime
`0x01000193`. All arithmetic is 32-bit wrapping multiply and xor.

Base schema fingerprint, over fields in declared order:

```
h = 0x811c9dc5
for each field:
    for each byte b of field.name (UTF-8):
        h = (h XOR b) * prime
    h = (h XOR 0xff) * prime          # name terminator
    h = (h XOR dtype_tag(field)) * prime
    h = (h XOR 0xff) * prime          # field terminator
return h
```

The state topic uses that value directly. The action topic xors a stream tag
onto it, so a peer running an older wire format without the
`in_reply_to_ts_us` slot fails the fingerprint check instead of misparsing the
header:

```
state_fingerprint  = schema_fingerprint(state_fields)
action_fingerprint = schema_fingerprint(action_fields) XOR 0xa1c0b001
```

This hash is not cryptographic. It is a cheap agreement check, not a security
boundary.

## dtype reference

Each dtype fixes the on-wire width, the encoding, and a stable tag fed into the
fingerprint. **Never renumber the tags.** A different tag on either side breaks
fingerprint agreement.

| dtype | Tag | Width | On-wire encoding, little-endian |
|---|---|---|---|
| F64 | 1 | 8 | IEEE-754 double |
| F32 | 2 | 4 | IEEE-754 float |
| I32 | 3 | 4 | signed two's complement |
| I16 | 4 | 2 | signed two's complement |
| I8 | 5 | 1 | signed two's complement |
| U32 | 6 | 4 | unsigned |
| U16 | 7 | 2 | unsigned |
| U8 | 8 | 1 | unsigned |
| Bool | 9 | 1 | `0` false, `1` true |

Values are carried as `f64` in the API and converted at the wire boundary. On
encode, out-of-range integers saturate to the dtype's min or max, and `NaN`
becomes `0`, or `false` for `Bool`.

There is no in-band signal that a value saturated. The publisher logs it locally
and the peer only ever sees the clipped value.

## Frame video

When a video track uses a byte-stream codec (`RAW`, `PNG`, or `MJPEG`) instead of
a WebRTC codec, frames travel as byte streams on `portal_frame_video`. One byte
stream per frame. All byte-stream video tracks share this single topic and are
demultiplexed by the track name in the header.

The header is 16 fixed bytes plus the track name:

```
[u8  version = 1]
[u8  codec_id = 0 RAW | 1 PNG | 2 MJPEG]
[u16 width            little-endian]
[u16 height           little-endian]
[u64 timestamp_us     little-endian]
[u16 track_name_len   little-endian]
[u8 × track_name_len  track name, UTF-8]
[u8 × N               encoded payload]
```

The decoded payload is always packed RGB24: byte order `R, G, B`, row-major,
`stride = width * 3`. RAW is exactly that already. PNG and MJPEG decode to it.

Track names are capped at 256 bytes. Route each frame to the track named in its
header.

Codec tradeoffs, the chunking latency floor, and per-track fps ceilings are in
[Frame video](../05-frame-video.md).

## WebRTC video tracks

The default video path is a standard WebRTC media track, one per camera. The
LiveKit track name is the camera name passed to `add_video`, and a subscribing
peer matches on that name.

Two requirements bind a Portal-compatible publisher.

**Codec.** The default is H.264. VP8, VP9, AV1, and H.265 are also valid. Both
ends must negotiate the same codec, so AV1 and H.265 depend on peer support.

**Per-frame timestamp.** Every published frame must carry `user_timestamp` in its
LiveKit packet-trailer metadata, in microseconds on the sender's clock. The
publisher must enable `PacketTrailerFeatures.user_timestamp` on the track.

That second one is **mandatory, not optional.** The operator's synchronization
aligns frames to state by this timestamp, so a frame without it can never be
matched. A subscribed track from a publisher that does not set it is unsupported.
Either republish it through a Portal-compatible publisher, or enable the trailer
upstream.

Portal publishes with simulcast off by default and `max_framerate` set to twice
the configured fps. Neither is required for interop, but matching them avoids
surprises. Simulcast is per-track configurable via `add_video(simulcast=True)`,
as is libwebrtc's content-type hint via `screencast=True`.

## Clock

Time sync, NTP-style. Every peer except the robot estimates the robot's clock
from a ping/pong exchange on `portal_clock`. Packets are **unreliable** so
retransmits cannot inflate the round trip, and addressed to one participant
(`destination_identities`), never broadcast. All integers are little-endian.

```
ping  [u8 kind = 0][u32 seq][u64 t1]                    peer → robot
pong  [u8 kind = 1][u32 seq][u64 t1][u64 t2][u64 t3]    robot → pinger
```

`t1` is when the peer sent the ping, on its own clock. The robot answers every
ping, echoing `seq` and `t1` and adding `t2` (ping received) and `t3` (pong
sent), both on the robot's clock. The peer notes `t4` when the pong arrives and
computes:

```
offset = ((t2 - t1) + (t3 - t4)) / 2      robot clock minus local clock
rtt    = (t4 - t1) - (t3 - t2)
```

Portal pings at 4 Hz until synced, then 1 Hz. It keeps a 10 s window of samples
and uses the offset from the one with the smallest `rtt`, since the
least-delayed sample is closest to the true offset. It needs 4 samples before
the first estimate. A sample implying a jump of more than 1 s is ignored unless
30 in a row agree, which counts as a resync. When the robot leaves the room the
estimate starts over.

The estimate never moves timestamps backwards. A forward correction applies at
once. A backward correction is absorbed gradually: the clock runs at 95% of real
speed until it has caught up.

A packet with an unknown kind or the wrong length is ignored. A peer that does
not implement time sync can ignore the topic; it just keeps timestamping on its
own clock.

## Control plane: active operator

The robot accepts actions from exactly one operator at a time, named by its
`lk.portal.active_operator` attribute. The robot's attribute is the single source
of truth. Actions from any other sender are dropped at the robot's receive gate,
with no error and no reply.

To read it, read that attribute on the robot. An empty string means no active
operator, and the robot drops every action until one is set.

To change it, the robot writes its own attribute directly. Any other peer asks the
robot to write it, using one reserved RPC:

| RPC method | Registered on | Payload | Reply |
|---|---|---|---|
| `portal.set_active_operator` | robot | new identity, or `""` to clear | `""` on success |

The payload is the raw identity string, not JSON. An empty payload clears the
pointer.

The robot's handler writes its attribute and updates its internal pointer, and the
change propagates to everyone through the normal attribute-changed event. On
failure the handler returns an RPC error: code `2001` if the robot is not
connected, `2002` if the attribute write failed, `2003` if the caller is neither
an operator nor an observer. A caller's role attribute can reach the robot after
its RPC does, so the robot waits up to 1.5 s for an unknown caller to appear
before refusing.

When the active operator disconnects, the robot leaves the pointer pinned at that
identity, so a reconnect with the same identity resumes control. To reassign, any
peer calls the RPC again.

To claim control as an operator, call `portal.set_active_operator` on the robot
with your own identity as the payload.

## Keypoint

A free-form annotation, broadcast as a **reliable** data packet on
`portal_keypoint`. The payload is UTF-8 JSON:

```json
{"type": "recording", "payload": {"task_description": "pick the blue cube"}, "timestamp_us": 1790184569394000}
```

- `type` is any string and `payload` any JSON object; `payload` may be omitted
  and reads as `{}`. Portal defines nothing else and interprets neither field.
- `timestamp_us` is where the mark belongs, on the robot's clock.
- Receivers ignore unknown fields, and drop malformed packets and packets from
  participants that are not a recognised Portal peer.
- A keypoint must fit in one data packet (64 KB negotiated on a local server);
  a larger one fails at send time.

## Application RPC

Beyond that one reserved method, RPC is plain LiveKit RPC. Either side registers
methods, either side invokes. Payloads are UTF-8 strings, opaque to Portal, JSON by
convention.

The LiveKit SDK limits apply: 15 KB request, 15 KB response, 256-byte error
message, 15 KB error data. Codes 1001 to 1999 are reserved for transport errors.

No Portal-specific framing is involved, so any LiveKit RPC client interoperates as
is. See [RPC](../06-rpc.md) for the SDK-level surface.

## End-to-end encryption

If the Portal peers use E2EE, set the same shared AES-GCM key on your client
through your SDK's E2EE support before connecting.

LiveKit encrypts all media tracks and data channels transparently below the
layouts above, so **the wire formats are unchanged**. Both ends must use the same
key or all traffic fails to decrypt, silently. See [E2EE](e2ee.md).

## Timestamps and clocks

Every timestamp on the wire is `u64` microseconds since the Unix epoch,
little-endian. State, action, frame video, and clock packets all use this unit.

Portal stamps state, frames and actions with `now_us()`: the robot's clock on
the robot, and the time sync estimate of it everywhere else (see [Clock](#clock)).
A peer may pass its own timestamps instead.

The operator's synchronization compares a state timestamp against each video frame
timestamp, so **the robot must stamp its state packets and its frames from the same
clock.** Two different clocks on the robot is the one mistake that breaks matching
in a way that looks like network trouble.

Operator and robot clocks do not need to be synchronized for the gate to work.
Time sync puts actions on the robot's timeline anyway, so recordings from
several peers line up without NTP.

## Minimal implementation checklist

To act as an **operator** against a Portal robot:

1. Connect with a token granting `can_update_own_metadata`. Set your own
   `lk.portal.role` attribute to `"operator"` and `lk.portal.version` to `"3"`.
2. Find the robot: the remote participant with `lk.portal.role == "robot"`.
3. Subscribe to its video tracks by name. For WebRTC tracks, read `user_timestamp`
   from each frame's packet trailer. For byte-stream tracks, read
   `portal_frame_video` and parse the frame header.
4. Read `portal_state` packets. Verify the fingerprint, then parse the header and
   fields against the shared state schema.
5. Match frames to state by timestamp to form observations, or consume the streams
   independently. See [Synchronization](synchronization.md).
6. Claim control: call `portal.set_active_operator` on the robot with your
   identity.
7. Publish actions on `portal_action` with the action fingerprint, your timestamp,
   an optional `in_reply_to_ts_us`, and the fields in schema order.

To act as a **robot** against Portal operators:

1. Connect with the same metadata grant. Set `lk.portal.role` to `"robot"`.
2. Publish video tracks. On WebRTC tracks, enable
   `PacketTrailerFeatures.user_timestamp` and stamp every frame.
3. Publish `portal_state` packets with the state fingerprint and a timestamp from
   the same clock as your frames.
4. Register the `portal.set_active_operator` RPC. On call, write your
   `lk.portal.active_operator` attribute to the payload.
5. Read `portal_action` packets. **Drop any whose sender identity is not your
   current active operator.** Then verify the fingerprint and parse.

The shared schemas, meaning state fields, action fields, and video track names
and codecs, are the out-of-band contract that makes any of these bytes
parseable. Distribute them as a [YAML config file](config-file.md) or agree on
them some other way. The fingerprints only detect disagreement. They cannot
describe the schema for you.

## Reference

- [Concepts](../02-concepts.md). The role and observation models these formats
  serve.
- [Config from YAML](config-file.md). The shareable schema file both peers build
  from.
- [Frame video](../05-frame-video.md). Codec and latency detail for the
  byte-stream video path.
- [Synchronization](synchronization.md). How an operator turns separate streams
  into matched observations.
- [RPC](../06-rpc.md). The SDK-level RPC surface.
- [E2EE](e2ee.md). Shared-key encryption setup.
