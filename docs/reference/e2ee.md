# End-to-end encryption

> Shared-key AES-GCM over every media track and data channel.

With E2EE on, content is encrypted before it leaves the sender and decrypted only
at the receiver. The LiveKit server routes the packets without being able to read
them.

## How it works

E2EE uses AES-GCM with a shared secret. Both peers supply the same key before
connecting.

Encryption is applied by libwebrtc on each RTP frame and on every data channel
packet. That means it covers everything Portal sends: WebRTC video, byte-stream
frame video, state, actions, action chunks, and RPC.

Portal does not implement any of this itself. It passes your key to the LiveKit
SDK, which handles the rest.

## Setup

Call `set_e2ee_key` on the config before `connect`.

```python
import os

from livekit.portal import DType, Robot, RobotConfig

cfg = RobotConfig("session-1")
cfg.add_video("cam1")
cfg.add_state_typed([("j1", DType.F32)])
cfg.set_e2ee_key(os.environ["PORTAL_E2EE_KEY"].encode())

robot = Robot(cfg)
await robot.connect(url, token)
```

The operator side is identical. Use `OperatorConfig` and supply the same key.

If you are loading your wire contract from
[a YAML file](config-file.md), the key is deliberately not part of it. Set it on
the config after loading:

```python
cfg = RobotConfig.from_yaml_file("portal.yaml", "session-1")
cfg.set_e2ee_key(os.environ["PORTAL_E2EE_KEY"].encode())
```

## Generating and distributing keys

Generate 256 bits of randomness:

```python
import os
key = os.urandom(32)
```

Treat it like any other secret. Patterns that work well:

- Load it from an environment variable or a secret manager at startup.
- Derive a per-session key from a master secret plus the session name.
- Pass it through job metadata when dispatching a remote policy.

Do not hardcode keys in source, and do not commit them next to your config.

## Coverage

| Traffic | Encrypted |
|---|---|
| WebRTC video (H264, VP8, VP9, AV1, H265) | Yes |
| Byte-stream video (MJPEG, PNG, RAW) | Yes |
| State packets | Yes |
| Action packets | Yes |
| Action chunk byte streams | Yes |
| RPC calls and replies | Yes |
| Participant identities and room metadata | No |
| Track names and signaling | No |
| Token exchange with the LiveKit server | No, TLS only |

The server sees who is in the room and what tracks exist. It cannot read the
contents of any of them.

Note that the active-operator pointer is a **participant attribute**, so it is not
covered. The server and everyone in the room can read which operator holds
control. That is by design, since the server manages attribute state.

## Mismatched or missing key

This is the failure mode worth knowing in advance, because it is quiet.

If one peer connects with no key, or with a different key, **decryption fails
silently**. There is no handshake error and no exception.

What you will see instead:

- Video is black, green, or visibly corrupt.
- State and action packets do not parse. You may see
  [`bad-payload`](../08-troubleshooting.md#bad-payload) warnings.
- Observations never fire, because no frames or states are usable.

The fix is always the same. Confirm both sides load byte-identical keys. A
trailing newline from a file read or a shell variable is the usual culprit.

```python
key = os.environ["PORTAL_E2EE_KEY"].encode()
print(len(key))   # 32, on both sides
```

## Reference

- [Portal API](../03-portal-api.md). Where `set_e2ee_key` sits in the config
  surface.
- [Config from YAML](config-file.md). Why the key is not in the file.
- [Wire protocol](wire-protocol.md#end-to-end-encryption). What a peer in another
  SDK has to do.
