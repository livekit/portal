# RPC

> One-shot commands that do not belong in a control loop.

`home`, `calibrate`, `start_recording`, a configuration change. These happen
once, they need an answer, and modelling them as state or actions would be
awkward. Portal exposes LiveKit's RPC surface directly for them.

Either side can register methods. Either side can invoke them.

## Register and call

On the side that does the work:

```python
def home(data):
    hardware.home()
    return "ok"

robot.register_rpc_method("home", home)
```

On the side that asks:

```python
reply = await op.perform_rpc("home")
print(reply)   # "ok"
```

That is the whole happy path.

Handlers may be `def` or `async def`. They **must return a string**. Returning
anything else is an error.

Handlers can be registered before or after `connect()`. Portal stores the set
and reapplies it on every reconnect, so you do not have to re-register after a
network blip.

Remove one with `robot.unregister_rpc_method("home")`.

## Payloads

The payload is a **UTF-8 string** and Portal never looks inside it. JSON is the
convention, but any string works.

```python
import json

def move_to(data):
    target = json.loads(data.payload)
    hardware.move(target["x"], target["y"])
    return json.dumps({"ok": True})

robot.register_rpc_method("move_to", move_to)
```

```python
reply = await op.perform_rpc("move_to", payload=json.dumps({"x": 1.0, "y": 2.0}))
result = json.loads(reply)
```

The handler receives an `RpcInvocationData` with four fields:

| Field | What it is |
|---|---|
| `payload` | The caller's string. Opaque to Portal. |
| `caller_identity` | Who called. Use it to authorize or to label. |
| `request_id` | Matches on both sides. Useful in logs. |
| `response_timeout` | How long the caller will wait. Give up before this. |

If you need to send binary, base64-encode it yourself. If you find yourself
pushing near the size limit on every call, that data belongs on a stream rather
than in RPC.

### Limits

These come from the LiveKit SDK, not from Portal.

| Field | Limit |
|---|---|
| Request payload | 15 KB |
| Response payload | 15 KB |
| `RpcError.message` | 256 bytes |
| `RpcError.data` | 15 KB |

An over-limit request fails with transport error code 1402. An over-limit
response fails with 1504. Neither surfaces as a handler exception, so a handler
that returns a large string fails in a way its own `try` block cannot catch.

## Errors

To signal an application error, raise `RpcError.Error(code, message, data)` from
the handler. It is serialized, sent back, and re-raised on the caller as
`PortalError.Rpc`.

```python
from livekit.portal import RpcError

def home(data):
    if hardware.calibrating:
        raise RpcError.Error(4001, "cannot home while calibrating")
    hardware.home()
    return "ok"
```

```python
from livekit.portal import PortalError

try:
    await op.perform_rpc("home")
except PortalError.Rpc as e:
    print(f"robot refused: {e}")
```

Any other exception from a handler becomes a generic application error with code
1500. That works, but the caller loses your message, so raise `RpcError.Error`
when you want the reason to survive the trip.

Codes 1001 to 1999 are reserved by the LiveKit SDK for transport-level failures.
Pick your application codes outside that range.

## Routing

`perform_rpc` routes to the peer Portal has already identified, which is
whichever participant sent Portal traffic first. If no peer is known yet and the
room has exactly one remote participant, that one is used.

With several operators in the room, the robot is unambiguous because it is a
singleton. Addressing a specific operator is not, so pass `destination`:

```python
# Operator to robot. Explicit, though usually unnecessary.
await op.perform_rpc("home", destination=op.robot_identity())

# Robot to one specific operator.
await robot.perform_rpc("notify", payload="ack", destination="policy-v1")
```

If Portal cannot pick a peer it raises `PortalError.NoPeer` or
`PortalError.AmbiguousPeer`. Both mean the same thing in practice: name the
destination.

Set `response_timeout_ms` to bound a slow handler.

## The one reserved method

Portal reserves `portal.set_active_operator`. It is registered on the robot and
is how an operator asks the robot to move the active-operator pointer.

Call `set_active_operator(...)` instead of invoking it by name. The high-level
method handles the payload format and the robot-side attribute write for you.

Do not register your own method under that name.

## Next steps

- [Portal API](03-portal-api.md). The full surface.
- [Concepts: control handoff](02-concepts.md#control-handoff). What the reserved
  method is for.
- [Wire protocol](reference/wire-protocol.md#application-rpc). The contract, if
  you are implementing a peer in another SDK.
