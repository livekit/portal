# Wire protocol

This page is the implementer-facing contract. It is for building a Portal
peer in a LiveKit SDK that has no Portal plugin: a browser client, a Go
service, a Swift app, anything that speaks LiveKit but cannot import the
Rust core or the Python package.

Every other page in this folder describes the SDK surface. This page
describes the bytes on the wire. A peer that follows everything here will
interoperate with a Portal `Robot` or `Operator` without sharing any code.

Portal adds no new transport. It rides four standard LiveKit primitives:
reliable data packets, byte streams, WebRTC video tracks, and participant
attributes. The contract is the set of topic names, attribute keys, RPC
names, and binary layouts below.

## Transport map

Each logical channel maps to one LiveKit primitive on one reserved topic.

| Channel | Topic | LiveKit primitive | Reliable | Publisher |
|---|---|---|---|---|
| State | `portal_state` | data packet | yes | robot |
| Action | `portal_action` | data packet | yes | operator |
| Action chunk | `portal_action_chunk` | byte stream | yes | operator |
| Frame video | `portal_frame_video` | byte stream | yes | robot |
| RTT | `portal_rtt` | data packet | no | both |
| WebRTC video | (track name) | media track | n/a | robot |

All topic names are exact, case-sensitive literals. A peer consuming a
channel must filter incoming data and stream events by topic and ignore
everything else. Other applications can share the room on other topics.

The role column is the default direction. With operator-side action
subscription on (see [portal-api.md](03-portal-api.md#operator-side-action-subscription-hitl-recording)),
an operator also reads `portal_action` and `portal_action_chunk`.

## Identity, roles, and discovery

The room name and participant identities come from the access token, not
from Portal. The `session` string in Portal config is a local log label
and is never compared against the room name. Pick identities at token-mint
time.

On connect, every Portal peer self-sets one participant attribute:

| Attribute key | Value | Set by |
|---|---|---|
| `lk.portal.role` | `"robot"` or `"operator"` | every peer, on connect |
| `lk.portal.active_operator` | active operator identity, or `""` | robot only |

A peer discovers the robot by scanning remote participants for
`lk.portal.role == "robot"`. There is at most one. Operators are every
participant with `lk.portal.role == "operator"`. A participant with no
`lk.portal.role` attribute is not a Portal peer and is ignored.

Both attributes are plain participant attributes. Read them from the
participant-attributes-changed event and from the initial participant list
on join. To behave as a robot, set `lk.portal.role` to `"robot"` yourself
after connecting.

**Token requirements.** Robot and operator tokens must grant
`can_update_own_metadata = true`. Both roles self-set `lk.portal.role` on
connect, and that call fails without the grant. A token may also seed the
robot's `lk.portal.active_operator` attribute at mint time so the pointer
is set before anyone connects.

## State and action data packets

State and action are reliable LiveKit data packets. Both share a
little-endian binary layout. Action carries one extra header field.

State packet on `portal_state`:

```
[u32 fingerprint        little-endian]
[u64 timestamp_us       little-endian]
[field 0 bytes]
[field 1 bytes]
...
```

Action packet on `portal_action`:

```
[u32 fingerprint        little-endian]
[u64 timestamp_us       little-endian]
[u64 in_reply_to_ts_us  little-endian]   # 0 means "no correlation"
[field 0 bytes]
[field 1 bytes]
...
```

Field bytes are emitted in declared schema order. Each field's width is
fixed by its dtype (see [dtype reference](#dtype-reference)). There is no
per-field tag or length on the wire. The schema, shared out of band,
is the only thing that lets a receiver parse the payload. The fixed header
is 12 bytes for state and 20 bytes for action.

`timestamp_us` is microseconds since the Unix epoch on the sender's clock.
`in_reply_to_ts_us` lets an operator stamp which observation an action
answers. `0` is the no-correlation sentinel and is safe because real epoch
timestamps are never zero.

The `fingerprint` is the receiver's parse gate. Compute it from your local
schema (algorithm below). On receive, if the packet's fingerprint does not
match your expected fingerprint for that topic, drop the packet. Do not try
to parse it. A mismatch means the peer's schema disagrees with yours.

## Schema fingerprint

The fingerprint is a 32-bit FNV-1a hash over the ordered field names and
dtype tags. It detects any rename, dtype change, or reorder. Both peers must
compute it identically or all traffic drops.

Constants: offset basis `0x811c9dc5`, prime `0x01000193`. All arithmetic
is 32-bit wrapping multiply and xor.

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

The state topic uses this base value directly. The action and chunk topics
xor a per-stream tag onto it so a peer running an older wire format (one
without the `in_reply_to_ts_us` slot) fails the fingerprint check instead
of misparsing the header:

```
state_fingerprint  = schema_fingerprint(state_fields)
action_fingerprint = schema_fingerprint(action_fields) XOR 0xa1c0b001
```

The chunk fingerprint mixes in the chunk name and horizon as well. See
[action chunks](#action-chunks).

This hash is not cryptographic. It is a cheap agreement check, not a
security boundary.

## dtype reference

Each field declares a dtype. The dtype fixes the on-wire byte width, the
encoding, and the stable tag fed into the fingerprint. Never renumber the
tags. A different tag on either side breaks fingerprint agreement.

| dtype | Tag | Width (bytes) | On-wire encoding (little-endian) |
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

Values are carried as `f64` in the API and converted at the wire boundary.
On encode, out-of-range integers saturate to the dtype's min or max, and
`NaN` becomes `0` (or `false` for `Bool`). A receiver decodes each field
back to the declared type. There is no in-band signal that a value
saturated. The publisher logs it locally and the peer sees only the clipped
value.

## Action chunks

An action chunk is a fixed-horizon batch of actions, the standard output of
a VLA policy that emits several future steps per inference. Chunks travel as
byte streams on `portal_action_chunk`, not data packets, because a horizon
of rows can exceed the data-packet size limit.

A chunk schema is a named tensor of shape `[horizon, n_fields]` with a
per-field dtype. The byte-stream payload is:

```
[u32 fingerprint        little-endian]
[u64 timestamp_us       little-endian]
[u64 in_reply_to_ts_us  little-endian]
[row 0: field 0, field 1, ... in schema order]
[row 1: field 0, field 1, ...]
...
[row horizon-1: ...]
```

The header is the same 20-byte correlated header as an action packet. The
body is row-major: all fields of timestep 0, then all fields of timestep 1,
and so on for `horizon` rows. Each field uses its dtype width.

The chunk fingerprint extends the base schema fingerprint with the chunk
name and horizon, then xors a distinct stream tag so a chunk and an action
with identical fields can never collide:

```
h = schema_fingerprint(chunk.fields)
for each byte b of chunk.name (UTF-8):
    h = (h XOR b) * prime
h = (h XOR 0xff) * prime
for each byte b of chunk.horizon as u32 little-endian:   # 4 bytes
    h = (h XOR b) * prime
chunk_fingerprint = h XOR 0xc1c0b001
```

A peer can register more than one chunk schema. The receiver dispatches each
incoming stream to the right handler by matching the fingerprint in the
header. An unknown fingerprint is dropped.

## Frame video

When a video track is configured with a byte-stream codec (`RAW`, `PNG`, or
`MJPEG`) instead of a WebRTC codec, frames travel as byte streams on
`portal_frame_video`. One byte stream per frame. All byte-stream video
tracks share this single topic and are demultiplexed by the track name in
the header.

Header is 16 fixed bytes plus the track name:

```
[u8  version = 1]
[u8  codec_id = 0 RAW | 1 PNG | 2 MJPEG]
[u16 width            little-endian]
[u16 height           little-endian]
[u64 timestamp_us     little-endian]
[u16 track_name_len   little-endian]
[u8 * track_name_len  track name, UTF-8]
[u8 * N               encoded codec payload]
```

The decoded payload is always packed RGB24, byte order `R, G, B`, row-major,
`stride = width * 3`. RAW is exactly that. PNG and MJPEG decode to it. Track
name is capped at 256 bytes. The receiver routes the frame to the matching
track by the name in the header.

Full codec tradeoffs, the chunking latency floor, and per-track fps ceilings
are in [frame-video.md](05-frame-video.md).

## WebRTC video tracks

The default video path is a standard WebRTC media track, one per camera. The
LiveKit track name is the camera name passed to `add_video`. A subscribing
peer matches the track by that name.

Two requirements bind a Portal-compatible WebRTC video publisher:

- **Codec.** Default is H.264. VP8, VP9, AV1, and H.265 are also valid. Both
  ends must negotiate the same codec, so AV1 and H.265 depend on peer
  support.
- **Per-frame timestamp.** Every published frame must carry `user_timestamp`
  in its LiveKit packet-trailer metadata, in microseconds on the sender's
  clock. The publisher must enable `PacketTrailerFeatures.user_timestamp` on
  the track. This is mandatory, not optional. The operator's
  synchronization aligns frames to state by this timestamp, and a frame
  without it cannot be matched.

A subscribed track from a publisher that does not set `user_timestamp` is
unsupported. Either republish it through a Portal-compatible publisher or
enable the trailer upstream.

Portal publishes with simulcast off and `max_framerate` set to twice the
configured fps. Neither is required for interop, but matching them avoids
surprises.

## RTT

Round-trip time is an optional liveness and latency probe. It is a data
packet on `portal_rtt`, sent **unreliable** so retransmits do not inflate the
measurement.

```
[u8  kind = 0 ping | 1 pong]
[u64 timestamp_us  little-endian]
```

A peer sends a ping with its current timestamp on a timer. The receiver
echoes the same payload back as a pong, preserving the original timestamp.
The original sender computes RTT as now minus the echoed timestamp. A peer
that does not implement RTT can ignore the topic. Nothing else depends on it.

## Control plane: active operator

The robot accepts actions from exactly one operator at a time, named by its
`lk.portal.active_operator` attribute. The robot's attribute is the single
source of truth. Actions from any other sender are dropped at the robot's
receive gate with no error and no reply.

To read the active operator, read the robot's `lk.portal.active_operator`
attribute. Empty string means no active operator, and the robot drops every
action until one is set.

To change it, the robot writes its own attribute directly. Any other peer
asks the robot to write it by calling one reserved RPC:

| RPC method | Registered on | Payload | Reply |
|---|---|---|---|
| `portal.set_active_operator` | robot | new identity, or `""` to clear | `""` on success |

The payload is the raw identity string, not JSON. An empty payload clears
the pointer. The robot's handler writes its attribute, updates its internal
pointer, and the change propagates to everyone through the normal
attribute-changed event. On failure the handler returns an RPC error:
code `2001` if the robot is not connected, `2002` if the attribute write
failed.

When the active operator disconnects, the robot leaves the pointer pinned at
that identity. A reconnect with the same identity resumes control. To
reassign, any peer calls `portal.set_active_operator` again.

To claim control as an operator, call `portal.set_active_operator` on the
robot with your own identity as the payload.

## Application RPC

Beyond the one reserved method, RPC is plain LiveKit RPC. Either side
registers methods, either side invokes. Payloads are UTF-8 strings, opaque
to Portal, JSON by convention. The LiveKit SDK limits apply: 15 KB request,
15 KB response, 256-byte error message, 15 KB error data. See
[rpc.md](07-rpc.md) for the SDK-level surface. No Portal-specific framing is
involved, so any LiveKit RPC client interoperates as is.

## End-to-end encryption

If the Portal peers use E2EE, set the same shared AES-GCM key on your client
through your SDK's E2EE support before connecting. LiveKit encrypts all media
tracks and data channels transparently below the layouts above, so the wire
formats are unchanged. Both ends must use the same key or all traffic fails
to decrypt. See [e2ee.md](08-e2ee.md).

## Timestamps and clocks

Every timestamp on the wire is `u64` microseconds since the Unix epoch, taken
from the sender's wall clock, little-endian. State, action, chunk, frame
video, and RTT all use this unit. The operator's synchronization compares the
state timestamp against each video frame timestamp, so the robot must stamp
its state packets and its frames from the same clock. Operator and robot
clocks need not be tightly synchronized for the gate to work, but large skew
shifts which frames match which state. Keep both peers on NTP for sane
matching.

## Minimal implementation checklist

To act as an **operator** against a Portal robot:

1. Connect with a token granting `can_update_own_metadata`. Set your own
   `lk.portal.role` attribute to `"operator"`.
2. Find the robot: the remote participant with `lk.portal.role == "robot"`.
3. Subscribe to its video tracks by name. For WebRTC tracks, read
   `user_timestamp` from each frame's packet trailer. For byte-stream tracks,
   read `portal_frame_video` and parse the frame header.
4. Read `portal_state` data packets. Verify the fingerprint, then parse the
   header and fields against the shared state schema.
5. Match frames to state by timestamp to form observations (see
   [synchronization.md](09-synchronization.md)), or consume the streams
   independently.
6. Claim control: call `portal.set_active_operator` on the robot with your
   identity.
7. Publish actions on `portal_action` with the action fingerprint, your
   timestamp, an optional `in_reply_to_ts_us`, and the fields in schema order.

To act as a **robot** against Portal operators:

1. Connect with the same metadata grant. Set `lk.portal.role` to `"robot"`.
2. Publish video tracks. On WebRTC tracks, enable
   `PacketTrailerFeatures.user_timestamp` and stamp every frame.
3. Publish `portal_state` data packets with the state fingerprint and a
   timestamp.
4. Register the `portal.set_active_operator` RPC. On call, write your
   `lk.portal.active_operator` attribute to the payload.
5. Read `portal_action` data packets. Drop any whose sender identity is not
   your current `active_operator`. Verify the fingerprint, then parse.

The shared schemas (state fields, action fields, chunk specs, video track
names and codecs) are the out-of-band contract that makes the bytes
parseable. Distribute them as a [YAML config file](04-config-file.md) or
agree on them by other means. The fingerprints only detect disagreement.
They cannot describe the schema for you.

## Reference

- [Concepts](02-concepts.md). The role model and observation model the wire
  formats serve.
- [Config from YAML](04-config-file.md). The shareable schema file both peers
  build from.
- [Frame video](05-frame-video.md). Full codec and latency detail for the
  byte-stream video path.
- [Synchronization](09-synchronization.md). How an operator turns the
  separate state and frame streams into matched observations.
- [RPC](07-rpc.md). The SDK-level RPC surface.
- [E2EE](08-e2ee.md). Shared-key encryption setup.
</content>
</invoke>
