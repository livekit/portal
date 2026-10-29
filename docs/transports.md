# Transports

Portal's logic lives in `livekit-portal-core`, a sans-IO state machine that builds for
native and `wasm32-unknown-unknown`. It never touches a socket, a task or a
clock. A **driver** feeds it transport events plus the local time and carries
out what it returns. This page describes the transport model the core speaks and
how each transport maps onto it.

| Transport | Status | Driver |
|---|---|---|
| LiveKit | implemented | `livekit-portal` (`src/wire.rs` holds the whole mapping) |
| Loopback | implemented | `livekit_portal_core::loopback`, in-process, for tests |
| Media over QUIC | design only | — |

## The model

**Streams are publisher-scoped and named.** Every peer publishes its own
streams, and subscribers receive them tagged with the publisher:

```
{session}/{peer_id}/{stream}    stream = state | action | clock | keypoint
                                         | control | presence | video/{track}
```

**Transports only broadcast.** Point-to-point messages carry their addressee:
RPC requests and responses name it in the payload, and clock pongs are matched
to the receiver's own outstanding pings by `(seq, t1)`. Receivers drop the
rest. A publish may carry a `to` hint that a transport with native addressing
can use, but the core never relies on it.

**Every publish states a delivery intent:**

| `Delivery` | Meaning | Used for |
|---|---|---|
| `Reliable` | every message, in order | actions*, keypoints, control, state* |
| `LatestOnly` | a newer value supersedes older ones; a late subscriber starts from the latest | presence, state when `state_reliable=false` |
| `BestEffort { max_age_ms }` | drop rather than deliver late | clock (500 ms), actions when `action_reliable=false` (200 ms) |

\* by default; `state_reliable` and `action_reliable` choose between the rows.

**Capabilities** (`TransportCaps`) say what the transport does natively. The
core polyfills the rest:

| Cap | Native | Polyfill |
|---|---|---|
| `native_rpc` | the transport carries request/response | JSON requests and responses on `control`, addressed by `to`, with timeouts from the core's wake-ups (default 15 s, codes 1400/1502/1503 as on LiveKit) |
| `native_presence` | the transport reports join/leave and per-peer state | JSON presence on `presence` (`LatestOnly`), sent on change and every 1 s. A peer silent for 3 s is gone |
| `video` | see below | — |
| `e2ee` | every stream is encrypted end to end | none: an E2EE key on a transport without it fails `connect`, so nothing is ever sent in the clear |
| `max_payload_bytes` | largest data message | state and action packets over it fail `connect` |

`VideoCaps`:

- `ByteFramesOnly`: only RAW/PNG/MJPEG, each frame as a Portal frame envelope
  (`livekit_portal_core::frame_video`) carrying its own timestamp. Declaring a WebRTC
  codec (H264/VP8/VP9/AV1/H265) fails `connect` with a pointer to RAW, PNG or
  MJPEG.
- `TransportEncoded`: the transport takes raw frames, encodes them and carries
  the timestamp in its own metadata.
- `DriverEncoded`: the driver encodes with its own encoder and carries the
  timestamp in Portal's envelope.

**Trust.** The core treats every peer id a transport hands it as authenticated.
The active-operator gate and the `set_active_operator` check rest on that. A
transport must guarantee that a message tagged `from: X` was published by X.

### Driver contract

```rust
let mut core = PortalCore::new(CoreInit { config, caps });
let out = core.connect(Connect { local_peer, active_operator, peers, now_us })?; // validates caps
let out = core.handle(Step { input, now_us });   // PeerJoined, PresenceChanged, PeerLeft, Data,
                                                 // FrameBytes, VideoFrame, RpcRequest, RpcResponse,
                                                 // PresenceSet, LinkRestored, Wake
let out = core.send_state(SendState { values, timestamp_us, now_us })?;
let route = core.send_video_frame(SendVideoFrame { track, timestamp_us, now_us })?;
```

Outputs are `Publish { stream, payload, delivery, to }`, `SetPresence` and
`Rpc`/`RpcReply` (native transports only), `Emit(Event)` for the application,
and `ScheduleWake { at_us }`. A driver keeps one timer and feeds `Input::Wake`
when it fires.

Encoding and decoding stay with the driver so they can run off whatever lock
guards the core: `send_video_frame` only returns a route, and a driver may
decode envelopes itself and feed `Input::VideoFrame`.

## LiveKit (implemented)

LiveKit reports full caps, so no polyfill runs and the wire is unchanged from
before the split.

| Portal | LiveKit |
|---|---|
| caps | `TransportEncoded`, `native_rpc`, `native_presence`, `e2ee`, 64 KB packets |
| peer id | participant identity, which the SFU takes from the JWT |
| `state`, `action`, `clock`, `keypoint` | data packets on `portal_state`, `portal_action`, `portal_clock`, `portal_keypoint`; sender = publisher |
| `Reliable` / other | `reliable: true` / `reliable: false` |
| `to` hint | `destination_identities` (clock pings and pongs) |
| presence | attributes `lk.portal.role`, `lk.portal.version`, `lk.portal.active_operator` |
| join / leave | `ParticipantConnected` / `ParticipantDisconnected` |
| RPC | LiveKit RPC, method `portal.set_active_operator` |
| WebRTC-codec video | a published track; timestamp in `FrameMetadata.user_timestamp` |
| RAW/PNG/MJPEG video | one byte stream per frame on `portal_frame_video` |
| E2EE | the room's shared-key GCM encryption |
| link restored | `RoomEvent::Reconnected` |

## Loopback (implemented)

An in-process transport on a fake clock (`livekit_portal_core::loopback`), with per-link
latency, jitter, loss and reordering, and per-peer clock skew. Scenario tests
(`livekit-portal-core/tests/loopback.rs`) run whole sessions in plain `cargo test`.

Its default caps (`pubsub_caps`) are deliberately MoQ-shaped, so a LiveKit-ism
leaking into the core fails a test: broadcast only, no native RPC or presence,
`ByteFramesOnly`, no E2EE. It honors `Delivery` the way MoQ would.
`LatestOnly` is one group per value, dropped only when superseded, and it is
retained for late subscribers. `BestEffort` is subject to the link's loss and
is dropped past its age. `room_caps` instead emulates a room with native
presence and RPC and transport-encoded video, which exercises the paths the
LiveKit driver takes.

## Media over QUIC (design only)

MoQ drafts churn (IETF `moq-transport` versus `moq-lite`), so a driver should
isolate the draft it speaks behind its own module, the way `wire.rs` isolates
LiveKit.

| Portal | MoQ |
|---|---|
| caps | `DriverEncoded`, no native RPC or presence, `e2ee: false` until payload encryption exists |
| stream `{session}/{peer}/{stream}` | namespace `{session}/{peer}`, track `{stream}` |
| `Reliable` | objects in one group per stream, delivered in order |
| `LatestOnly` | a new group per value. Subscribers start from the latest group |
| `BestEffort { max_age_ms }` | a delivery timeout of `max_age_ms` |
| priorities | `clock` > `action` > `state` > `presence` > `video` > `keypoint` |
| presence | the `presence` track: latest group for late joiners, heartbeats for liveness (polyfill) |
| RPC, clock replies | the `control` and `clock` tracks, addressed in the payload (polyfill) |
| video | `video/{track}`: a GOP is a group, a frame is an object, and the timestamp travels in the object (Portal envelope), encoded by the driver (WebCodecs in a browser, ffmpeg natively) |
| auth | the relay must only let peer X publish under `{session}/X/`, which is what makes peer ids trustworthy |
| E2EE | relays see payloads, so E2EE needs payload-level encryption in the driver or core. Future work, so `e2ee: false` refuses an E2EE key |

## Options that only apply to some transports

| Option | Applies to | Elsewhere |
|---|---|---|
| WebRTC codecs (H264, VP8, VP9, AV1, H265) | `TransportEncoded`, `DriverEncoded` | `connect` fails on `ByteFramesOnly` |
| `simulcast`, `screencast`, `max_bitrate_kbps` | `TransportEncoded` (LiveKit) | ignored, with a warning at `connect` |
| E2EE key | transports with `e2ee` | `connect` fails |
| `BYTE_STREAM_CHUNK_SIZE` | LiveKit byte streams | not applicable |
