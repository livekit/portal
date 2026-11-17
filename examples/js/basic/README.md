# Browser teleoperator

The [`basic`](../../python/basic) teleoperator as a web page. It loads the same
`portal.yaml`, takes control of the Python robot, shows its camera and sends
actions back.

```bash
(cd ../../../js && pnpm install && pnpm build)
pnpm install
pnpm dev
```

Start `robot.py` from `examples/python/basic`, mint a token for the same room
with `canUpdateOwnMetadata` (for example `lk token create --api-key … --api-secret …
--identity web-teleop --grant '{"roomJoin":true,"room":"portal-demo","canPublish":true,"canSubscribe":true,"canUpdateOwnMetadata":true}'`),
and open `http://localhost:5173/?url=<LIVEKIT_URL>&room=portal-demo&token=<token>`.
