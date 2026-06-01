# RPC

For imperative commands that don't fit the continuous state/action/observation
loop (`home`, `start_recording`, `calibrate`, one-off configuration), Portal
exposes the LiveKit RPC surface directly. Either side can register methods.
Either side can invoke.

## Register and call

```python
# Robot side: register a handler
def say(data):
    print(f"operator says: {data.payload}")
    return "ok"

robot.register_rpc_method("say", say)
```

```python
# Operator side: invoke it
reply = await op.perform_rpc("say", payload="hello")
```

Handlers may be `def` or `async def` and **must return a string**.

Handlers can be registered before or after `connect()`. The stored set is
reapplied on every reconnect.

## Errors

To signal an application error from a handler, raise
`RpcError.Error(code, message, data)`. It is serialized and re-raised as
`PortalError.Rpc` on the caller's side.

```python
from livekit.portal import RpcError

def home(data):
    if robot.calibrating:
        raise RpcError.Error(4001, "cannot home while calibrating")
    robot.home()
    return "ok"
```

Any other exception becomes a generic application error (code 1500).

## Routing

`perform_rpc` routes to the peer Portal has identified (whoever has sent
Portal-topic traffic first). If no peer is known yet but the room has a
single remote participant, it's used as a fallback. With multiple
operators in the room, the robot's identity is unambiguous (singleton),
but if you ever need to address a specific operator (a supervisor sending
a message to one peer, for instance), pass `destination` explicitly.

```python
# Operator → robot
await op.perform_rpc("home", payload="{}", destination=op.robot_identity())

# Robot → operator (e.g. send something to a specific operator)
await robot.perform_rpc("notify", payload="ack", destination="policy-v1")
```

Portal also reserves one built-in RPC: `portal.set_active_operator`, used
internally by the operator-side `set_active_operator(...)` method to ask
the robot to update its attribute. You generally call the high-level
method rather than invoking the RPC by name.

## Payload format and limits

**Payload is a UTF-8 string**, opaque to Portal. Convention is JSON
(`json.dumps` / `json.loads`), but any string works. Limits from the LiveKit
SDK:

| Field | Limit |
|---|---|
| Request payload | 15 KB |
| Response payload | 15 KB |
| `RpcError.message` | 256 bytes |
| `RpcError.data` | 15 KB |

Over-limit requests fail with transport error code 1402 (request) or 1504
(response), not a handler exception. If you need binary, base64-encode it
yourself. If you're pushing close to the limit continuously, that's a signal
the data belongs on a stream, not in RPC.
