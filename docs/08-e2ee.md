# End-to-end encryption

Portal supports end-to-end encryption (E2EE) for both media tracks and data
channels. When enabled, content is encrypted before it leaves the sender and
decrypted only at the receiver. The LiveKit server cannot read the payload.

## How it works

E2EE uses AES-GCM with a shared secret. Both peers supply the same key before
connecting. Encryption is applied by libwebrtc on each RTP frame and on every
data channel packet, so it covers H264 video, byte-stream frame video, state,
and actions.

## Setup

Call `set_e2ee_key` on `PortalConfig` before `connect`. Both peers must use the
same key.

```python
import os
from livekit.portal import Portal, PortalConfig, Role

cfg = PortalConfig("session-1", Role.ROBOT)
cfg.set_e2ee_key(os.environ["PORTAL_E2EE_KEY"].encode())
# ... add_video, add_state_typed, add_action_typed ...

portal = Portal(cfg)
await portal.connect(url, token)
```

The operator side is identical. Replace `Role.ROBOT` with `Role.OPERATOR` and
supply the same key.

## Key generation and distribution

Generate a key with `os.urandom(32)` (256 bits). Treat it like any other
secret. Common patterns:

- Load from an environment variable or secret manager at startup.
- Derive per-session keys from a master secret and the session name.
- Pass through job metadata when dispatching a remote policy.

Do not hardcode keys in source.

## What is and is not covered

| Traffic | Covered |
|---|---|
| H264 video (WebRTC media path) | Yes |
| Byte-stream video (MJPEG, PNG, RAW) | Yes |
| State packets (data channel) | Yes |
| Action packets (data channel) | Yes |
| RPC calls | Yes |
| Token exchange with the LiveKit server | No (TLS only) |
| Signaling metadata (track names, participant info) | No |

The LiveKit server sees participant identities and room metadata but cannot
read media or data payloads when E2EE is active.

## Mismatched or missing key

If one peer connects without a key or with a different key, media arrives but
decryption fails silently. Video will be black or corrupted. State and action
packets will not parse. There is no handshake error. Check that both sides load
the same key bytes if data stops flowing after enabling E2EE.
