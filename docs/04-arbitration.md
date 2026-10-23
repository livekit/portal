# Arbitration

When a policy needs help, a human can take over the robot, demonstrate a
correction, and return control. Both remain connected and receive observations
throughout the session. **Arbitration** determines which operator's actions
the robot accepts.

This guide adds `policy.py`, `teleop.py`, and an optional `orchestrator.py` to
the [quickstart](01-quickstart.md). Reuse its `portal_token.py`, `robot.py`, and
`recorder.py`.

| Process | Role | Identity | Purpose |
|---|---|---|---|
| `robot.py` | `Robot` | `robot` | Publishes frames and state; receives active actions. |
| `policy.py` | `Operator` | `policy-v1` | Computes actions, including while the human has control. |
| `teleop.py` | `Operator` | `human` | Takes control to demonstrate a correction. |
| `recorder.py` | `Observer` | `recorder` | Records the session without sending actions. |

The recorder subscribes to active actions by default. To also record what an
inactive policy would have done, set its action subscription to `"all"` as shown
below.

## How arbitration works

The robot accepts actions from the **active operator**, identified by a
participant attribute on the robot. Operators and observers can request a change
with `set_active_operator(identity)`.

- **Claim control before sending actions.** A new robot has no active operator,
  unless its token seeds one, and rejects actions until one is selected.
- **Coordinate handoffs in your application.** There is no built-in priority
  between callers. The last accepted request determines the active operator.
  Use the [orchestrator](#orchestration) pattern when one process should decide.
- **A handoff keeps participants connected.** All operators can continue
  producing actions; the robot delivers only the active operator's actions to
  its callback.
- **Disconnecting does not clear control.** The active identity remains set,
  allowing the same operator to resume after reconnecting. Another participant
  must explicitly transfer control elsewhere.
- **`None` rejects new actions from all operators.** This does not issue a stop
  command to the hardware. Implement the required behavior in your robot code.

Recorded actions include `sender` and `active`, evaluated at the receiving
participant's control gate. These identify the sender and whether it was active
there; they do not acknowledge physical execution. Record hardware feedback
separately if you need to confirm motion.

A participant without a recognized Portal operator or observer role cannot
request a handoff; the robot returns RPC error `2003`.

## 1. Write the policy

The policy claims control at startup and then produces an action for every
observation, whether or not it's in control. While the human drives, the
robot drops the policy's actions. A recorder can still keep them, though, which
gives you the policy's prediction next to the human's correction at each
instant.

Save it as `policy.py`.

```python
# policy.py
import asyncio
import os

from livekit.portal import Operator, OperatorConfig

from portal_token import ROOM, mint
from robot import FPS, SCHEMA

IDENTITY = "policy-v1"


async def main() -> None:
    cfg = OperatorConfig(ROOM)
    cfg.add_video("cam1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    cfg.set_fps(FPS)

    op = Operator(cfg)

    def on_observation(obs) -> None:
        # Your model goes here. This one mirrors the state back.
        action = dict(obs.state)
        # Sent even when another operator is in control: the robot drops it,
        # but a recorder subscribed to "all" keeps it.
        op.send_action(action, in_reply_to_ts_us=obs.timestamp_us)

    op.on_observation(on_observation)
    op.on_active_operator_changed(
        lambda who: print(f"[policy] {'in control' if who == IDENTITY else f'standing by, {who} drives'}")
    )

    await op.connect(os.environ["LIVEKIT_URL"], mint(IDENTITY))
    await op.set_active_operator(IDENTITY)

    try:
        await asyncio.sleep(60)
    finally:
        await op.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
```

`from robot import FPS, SCHEMA` reuses the quickstart robot's schema so the
two can't drift. Importing `robot.py` doesn't start it.

## 2. Write the teleoperator

The human side. A real teleoperator reads a leader arm or a gamepad. This one
waits 10 seconds, takes over, holds every joint at zero for 10 seconds, and
hands control back. Each intervention is bracketed with keypoints, so the
recording says where it started, where it ended, and why.

Save it as `teleop.py`.

```python
# teleop.py
import asyncio
import os

from livekit.portal import Operator, OperatorConfig

from portal_token import ROOM, mint
from robot import FPS, SCHEMA

IDENTITY = "human"
POLICY = "policy-v1"


def read_leader_arm(state) -> dict:
    """Stand-in for real input: hold every joint at zero."""
    return {"j1": 0.0, "j2": 0.0, "j3": 0.0, "gripper": False, "mode": 0}


async def main() -> None:
    cfg = OperatorConfig(ROOM)
    cfg.add_video("cam1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    # A teleoperator flies on the newest frame and state. It doesn't need
    # them bundled into observations.
    cfg.set_observation_sync(False)

    op = Operator(cfg)
    op.on_active_operator_changed(lambda who: print(f"[teleop] active operator: {who}"))

    await op.connect(os.environ["LIVEKIT_URL"], mint(IDENTITY))

    try:
        await asyncio.sleep(10)

        # Take over, and mark it.
        await op.set_active_operator(IDENTITY)
        await op.send_keypoint("intervention_start", {"reason": "gripper misaligned"})

        for _ in range(FPS * 10):
            state = op.get_state()
            if state is not None:
                op.send_action(read_leader_arm(state), in_reply_to_ts_us=state.timestamp_us)
            await asyncio.sleep(1 / FPS)

        # Hand back, and mark it.
        await op.send_keypoint("intervention_end")
        await op.set_active_operator(POLICY)

        await asyncio.sleep(10)
    finally:
        await op.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
```

Take over first, then mark. A keypoint is stamped with `now_us()` on the
robot's clock when it is sent, so sending it right after the handoff puts it
on the first instant the human is in control.

## 3. Record the policy's shadow too

Use the quickstart's [`recorder.py`](01-quickstart.md#7-add-an-observer-to-record-data)
with one more line in its config:

```python
cfg.set_action_subscription("all")   # default "active": only active actions
```

With `"active"`, the recorder keeps active actions: the policy's
actions, then the human's, then the policy's again. With `"all"`, it also
keeps the policy's actions from during the intervention, with `active=False`.

## 4. Run it

Four terminals, same directory. Start the robot first.

```bash
python robot.py       # terminal 1
```

```bash
python policy.py      # terminal 2
```

```bash
python recorder.py    # terminal 3
```

```bash
python teleop.py      # terminal 4
```

The quickstart recorder stops after 30 seconds and the intervention runs from
10 to 20 seconds after `teleop.py` starts, so start the last two within a few
seconds of each other.

The robot prints the sender of every action it receives, so the handoff shows
up there:

```
[robot] action from policy-v1: {'j1': 0.84, ...}
[robot] action from human: {'j1': 0.0, 'j2': 0.0, 'j3': 0.0, 'gripper': False, 'mode': 0}
...
[robot] action from policy-v1: {'j1': -0.41, ...}
```

```
[policy] in control
[policy] standing by, human drives
[policy] in control
```

Open the recording with `rerun data/sessions/<file>.rrd`:

| Entity | What you see |
|---|---|
| `action/policy-v1/*` | The policy's actions for the whole session |
| `action/policy-v1/active` | 1, then 0 during the intervention, then 1 again |
| `action/human/*` | The human's actions, only during the intervention |
| `keypoints/intervention_start`, `keypoints/intervention_end` | The boundaries, with the reason in the payload |

During the intervention, `action/policy-v1/*` and `action/human/*` share the
`robot_time` timeline. That gives you what the policy predicted and what the
human did instead, frame by frame. A converter that builds a training dataset
can split episodes at the keypoints, keep the human's segments as
corrections, and label each row from `active`. See
[Data recording](03-data-recording.md#recording-in-production).

## Orchestration

Above, the human moves the pointer directly. That works when there is one
human and one policy that trust each other. When you have more operators, or
rules about who may take over and when, move all arbitration into one
**orchestrator**: an observer that is the only peer that ever calls
`set_active_operator`. Operators just ask.

This orchestrator gives the human control when it asks with a `takeover`
keypoint, hands it back to the policy on `release`, and recovers when the
active operator disconnects, so the pointer doesn't stay pinned to someone
who is gone.

Save it as `orchestrator.py`.

```python
# orchestrator.py
import asyncio
import os

from livekit.portal import Observer, ObserverConfig, PortalError

from portal_token import ROOM, mint
from robot import SCHEMA

POLICY = "policy-v1"


async def main() -> None:
    cfg = ObserverConfig(ROOM)
    cfg.add_video("cam1")
    cfg.add_state_typed(SCHEMA)
    cfg.add_action_typed(SCHEMA)
    cfg.set_action_subscription("none")   # it only steers

    obs = Observer(cfg)
    requests: asyncio.Queue = asyncio.Queue()

    # Callbacks run on this asyncio loop, so they can feed a queue directly.
    def on_keypoint(kp) -> None:
        if kp.type == "takeover":
            requests.put_nowait(kp.sender)
        elif kp.type == "release" and kp.sender == obs.active_operator():
            requests.put_nowait(POLICY)

    def on_operator_left(identity: str) -> None:
        if identity == obs.active_operator():
            # Fall back to the policy, or stop if the policy itself left.
            requests.put_nowait(POLICY if identity != POLICY else None)

    obs.on_keypoint(on_keypoint)
    obs.on_operator_left(on_operator_left)
    obs.on_active_operator_changed(lambda who: print(f"[orchestrator] active operator: {who}"))

    await obs.connect(os.environ["LIVEKIT_URL"], mint("orchestrator"))
    await obs.set_active_operator(POLICY)

    try:
        while True:
            driver = await requests.get()
            if driver is not None and driver not in obs.operators():
                continue   # only hand control to an operator that is here
            try:
                await obs.set_active_operator(driver)
            except PortalError as e:
                # NoPeer while the robot is away, for example. Keep running.
                print(f"[orchestrator] could not hand control to {driver}: {e}")
    finally:
        await obs.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
```

Then make the other two ask instead of take:

- In `policy.py`, drop `await op.set_active_operator(IDENTITY)`. The
  orchestrator hands it control at startup.
- In `teleop.py`, replace the two `set_active_operator` calls with keypoints:
  `await op.send_keypoint("takeover", {"reason": "gripper misaligned"})` to
  start, `await op.send_keypoint("release")` to end. They double as the
  intervention markers in the recording, so the `intervention_*` keypoints
  can go.

Run `orchestrator.py` in a fifth terminal, after the robot and before the
policy. It runs until you stop it with Ctrl-C.

The rules live in one place now, so they're easy to change: only some
identities may take over, a human always beats a policy, nobody takes over
during a grasp, or control rotates every few minutes to collect data from
several policies.

Three things to keep in mind:

- **The orchestrator decides, the robot enforces.** Nothing stops an operator
  from calling `set_active_operator` itself. If that matters, issue
  operator tokens from a server you control and keep the orchestrator the
  only code that moves the pointer.
- **Give the robot a safe default for `None`.** If the orchestrator clears
  the pointer, the robot gets no actions. Use `robot.on_active_operator_changed`
  to hold position or brake when it becomes `None`.
- **A crash is noticed late.** A clean `disconnect()` fires `on_operator_left`
  right away. A peer that crashes or loses its network is only noticed when
  the LiveKit server times it out, about 20 seconds later. Until then the
  pointer still names it and the robot executes nothing, so the safe default
  above matters for that case too.

## Next steps

- [Data recording](03-data-recording.md). What's in the archive and how it
  becomes a training dataset.
- [Portal API: the active operator](05-portal-api.md#the-active-operator).
  Control methods and callbacks.
- [Portal API: action subscription](05-portal-api.md#operator-side-action-subscription).
  `"none"`, `"active"` and `"all"` in detail.
