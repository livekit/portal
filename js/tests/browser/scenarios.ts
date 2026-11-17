// Copyright 2026 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Scenarios run in one page by run.mjs: every peer is a real Portal over a
// real LiveKit server. Each scenario throws on failure.
import {
  type ConnectOptions,
  Observer,
  ObserverConfig,
  Operator,
  OperatorConfig,
  type Portal,
  type PortalSpec,
  Robot,
  RobotConfig,
  RpcError,
} from '@livekit/portal';


declare global {
  interface Window {
    scenarios: Record<string, (url: string) => Promise<void>>;
    mintToken(identity: string, room: string, grants?: object): Promise<string>;
  }
}

const SPEC: PortalSpec = {
  fps: 30,
  videos: [
    { name: 'cam', codec: 'mjpeg', quality: 80 },
    { name: 'depth', codec: 'png' },
    { name: 'raw', codec: 'raw' },
  ],
  state: [
    { name: 'j1', dtype: 'f32' },
    { name: 'gripper', dtype: 'bool' },
    { name: 'mode', dtype: 'i8' },
  ],
  action: [
    { name: 'j1', dtype: 'f32' },
    { name: 'gripper', dtype: 'bool' },
  ],
  telemetry: [{ name: 'battery/v', dtype: 'f32' }],
};
const W = 64;
const H = 48;

let url = '';
const peers: Portal[] = [];
let roomSeq = 0;
const newRoom = () => `portal-test-${Date.now()}-${roomSeq++}`;

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(`assertion failed: ${msg}`);
}

async function until(pred: () => boolean, msg: string, ms = 8000): Promise<void> {
  const deadline = performance.now() + ms;
  while (!pred()) {
    if (performance.now() > deadline) throw new Error(`timed out: ${msg}`);
    await sleep(20);
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function rejects(p: Promise<unknown>, pattern: RegExp, msg: string): Promise<unknown> {
  try {
    await p;
  } catch (e) {
    assert(pattern.test(String((e as Error)?.message ?? e)), `${msg}: got "${(e as Error)?.message ?? e}"`);
    return e;
  }
  throw new Error(`expected rejection: ${msg}`);
}

function throws(f: () => unknown, pattern: RegExp, msg: string): void {
  try {
    f();
  } catch (e) {
    assert(pattern.test(String((e as Error)?.message ?? e)), `${msg}: got "${(e as Error)?.message ?? e}"`);
    return;
  }
  throw new Error(`expected throw: ${msg}`);
}

async function connect<P extends Portal>(p: P, identity: string, room: string, opts?: ConnectOptions): Promise<P> {
  peers.push(p);
  await p.connect(url, await window.mintToken(identity, room), opts);
  return p;
}

const robot = (room: string, spec: PortalSpec | string = SPEC) =>
  connect(new Robot(new RobotConfig(room, spec)), 'robot', room);
const operator = (room: string, identity: string, spec: PortalSpec = SPEC) =>
  connect(new Operator(new OperatorConfig(room, spec)), identity, room);
const observer = (room: string, identity = 'observer') =>
  connect(new Observer(new ObserverConfig(room, SPEC)), identity, room);

/** Robot loop at 30 fps: one timestamp per tick for state and every track. */
function drive(r: Robot): () => void {
  let i = 0;
  const rgb = new Uint8Array(W * H * 3);
  const timer = setInterval(() => {
    const ts = r.nowUs();
    rgb.fill(i % 256);
    for (const track of ['cam', 'depth', 'raw']) r.sendVideoFrame(track, rgb, { width: W, height: H }, ts);
    r.sendState({ j1: i / 10, gripper: i % 2 === 0, mode: i % 3 }, ts);
    i++;
  }, 1000 / 30);
  return () => clearInterval(timer);
}

async function cleanup(): Promise<void> {
  await Promise.all(peers.splice(0).map((p) => p.disconnect()));
}

const scenarios: Record<string, () => Promise<void>> = {
  async 'observations carry every byte-frame track and actions reach the robot'() {
    const room = newRoom();
    const r = await robot(room);
    const op = await operator(room, 'op');
    const stop = drive(r);
    let observations = 0;
    op.onObservation((obs) => {
      observations++;
      for (const track of ['cam', 'depth', 'raw']) {
        const f = obs.frames[track];
        assert(f && f.width === W && f.height === H && f.data?.length === W * H * 3, `${track} frame shape`);
        assert(f.timestampUs === obs.timestampUs, `${track} frame matched to its state`);
      }
      assert(typeof obs.state.gripper === 'boolean' && Number.isInteger(obs.state.mode), 'typed state');
      op.sendAction({ j1: obs.state.j1!, gripper: obs.state.gripper! }, { inReplyToTsUs: obs.timestampUs });
    });
    await op.setActiveOperator('op');
    await until(() => observations > 30 && r.getAction()?.sender === 'op', 'observations and actions');
    stop();
    const action = r.getAction()!;
    assert(action.active && typeof action.values.gripper === 'boolean', 'action from the active operator');
    const m = r.metrics()!;
    assert(m.policy.e2eUsP95 !== null && m.transport.actionsReceived > 0, 'robot metrics');
    assert((op.metrics()!.transport.bytesReceived.cam ?? 0) > 0, 'operator frame bytes counted');
    assert(op.getObservation() !== undefined && op.getVideoFrame('depth') !== undefined, 'latest values');
    assert(Math.abs(op.nowUs() - r.nowUs()) < 50_000, 'operator clock on the robot timeline');
  },

  async 'control hands off between operators, an observer and the robot'() {
    const room = newRoom();
    const r = await robot(room);
    const changes: (string | null)[] = [];
    r.onActiveOperatorChanged((who) => changes.push(who));
    const a = await operator(room, 'op-a');
    const b = await operator(room, 'op-b');
    const obs = await observer(room);
    await until(() => r.operators.length === 2 && r.observers.length === 1, 'rosters');
    assert(r.activeOperator === undefined, 'no one in control at start');

    const send = () => {
      a.sendAction({ j1: 1, gripper: true });
      b.sendAction({ j1: 2, gripper: false });
    };
    await a.setActiveOperator('op-a');
    assert(r.activeOperator === 'op-a', 'robot pointer');
    await until(() => (send(), r.getAction()?.sender === 'op-a'), 'op-a actions executed');
    await until(() => b.activeOperator === 'op-a', 'other operators see the pointer');

    await obs.setActiveOperator('op-b');
    await until(() => (send(), r.getAction()?.sender === 'op-b'), 'observer hands control to op-b');
    assert(r.getAction()!.active, 'executed action is active');

    await r.setActiveOperator(null);
    assert(r.activeOperator === undefined, 'robot clears the pointer');
    assert(changes.join(',') === 'op-a,op-b,', `pointer changes: ${changes.join(',')}`);

    const left: string[] = [];
    r.onOperatorLeft((id) => left.push(id));
    await b.disconnect();
    await until(() => left.includes('op-b') && r.operators.length === 1, 'operator left');
  },

  async 'observer receives state, active actions, telemetry and keypoints'() {
    const room = newRoom();
    const r = await robot(room);
    const op = await operator(room, 'op');
    const obs = await observer(room);
    const actions: string[] = [];
    const keypoints: string[] = [];
    const telemetry: string[] = [];
    obs.onAction((a) => actions.push(`${a.sender}:${a.active}`));
    obs.onKeypoint((k) => keypoints.push(`${k.sender}:${k.type}`));
    obs.onTelemetry((t) => telemetry.push(t.sender));
    let states = 0;
    obs.onState(() => states++);
    const stop = drive(r);
    await op.setActiveOperator('op');
    await until(() => (op.sendAction({ j1: 1, gripper: true }), actions.includes('op:true')), 'active actions');
    await until(() => states > 10, 'states');
    throws(() => obs.onObservation(() => {}), /observation sync is off/, 'observer has no observations');
    throws(() => obs.getObservation(), /observation sync is off/, 'observer has no latest observation');

    r.sendTelemetry({ 'battery/v': 12.5 });
    op.sendTelemetry({ 'battery/v': 3.7 });
    await until(() => telemetry.includes('robot') && telemetry.includes('op'), 'telemetry from every role');
    assert(obs.getTelemetry('robot')?.values['battery/v'] === 12.5, 'latest telemetry per sender');

    const own: string[] = [];
    op.onKeypoint((k) => own.push(k.type));
    const recorders = await op.sendKeypoint('recording', { task: 'pick' }, op.nowUs());
    assert(recorders.length === 1 && recorders[0] === 'observer', `keypoint recorders: ${recorders}`);
    await until(() => keypoints.includes('op:recording') && own.includes('recording'), 'keypoint everywhere');
    stop();

    throws(() => obs.sendState({ j1: 0, gripper: true, mode: 0 }), /role/i, 'observer cannot send state');
    throws(() => op.sendState({ j1: 0, gripper: true, mode: 0 }), /role/i, 'operator cannot send state');
    throws(() => r.sendAction({ j1: 0, gripper: true }), /role/i, 'robot cannot send actions');
  },

  async 'rpc: user methods, application errors and unknown methods'() {
    const room = newRoom();
    const r = new Robot(new RobotConfig(room, SPEC));
    // Registered before connect: applied on connect.
    r.registerRpcMethod('echo', async (d) => `${d.callerIdentity}:${d.payload}`);
    r.registerRpcMethod('fail', async () => {
      throw new RpcError(2001, 'nope', 'details');
    });
    await connect(r, 'robot', room);
    const op = await operator(room, 'op');
    assert((await op.performRpc('echo', 'hi')) === 'op:hi', 'default destination is the robot');
    const e = (await rejects(op.performRpc('fail', ''), /nope/, 'application error')) as RpcError;
    assert(e.code === 2001 && e.data === 'details', `error code and data: ${e.code} ${e.data}`);
    await rejects(op.performRpc('missing', ''), /./, 'unknown method');
    r.unregisterRpcMethod('echo');
    await rejects(op.performRpc('echo', ''), /./, 'unregistered method');
    // Robot to operator, after a handoff names the counterpart.
    op.registerRpcMethod('ping', async () => 'pong');
    await op.setActiveOperator('op');
    assert((await r.performRpc('ping', '')) === 'pong', 'robot reaches the active operator');
  },

  async 'a portal reconnects after disconnect and stays usable'() {
    const room = newRoom();
    const r = await robot(room);
    const stop = drive(r);
    const op = new Operator(new OperatorConfig(room, SPEC));
    let observations = 0;
    op.onObservation(() => observations++);
    for (let round = 0; round < 3; round++) {
      await connect(op, 'op', room);
      const before = observations;
      await until(() => observations > before + 10, `observations in round ${round}`);
      await op.setActiveOperator('op');
      await until(() => r.activeOperator === 'op', `handoff in round ${round}`);
      await op.disconnect();
      assert(op.getObservation() === undefined, 'latest values cleared on disconnect');
    }
    stop();
  },

  async 'operators recover after the robot restarts'() {
    const room = newRoom();
    const r1 = await robot(room);
    let stop = drive(r1);
    const op = await operator(room, 'op');
    let observations = 0;
    op.onObservation(() => observations++);
    await until(() => observations > 10, 'observations from the first robot');
    stop();
    await r1.disconnect();
    const r2 = new Robot(new RobotConfig(room, SPEC));
    await connect(r2, 'robot', room);
    stop = drive(r2);
    const before = observations;
    await until(() => observations > before + 10, `observations from the restarted robot`);
    await op.setActiveOperator('op');
    await until(() => r2.activeOperator === 'op', 'handoff on the new robot');
    stop();
  },

  async 'a network blip reconnects and the loop carries on'() {
    const room = newRoom();
    const r = await robot(room);
    const stop = drive(r);
    const op = await operator(room, 'op');
    await op.setActiveOperator('op');
    let observations = 0;
    op.onObservation((obs) => {
      observations++;
      op.sendAction({ j1: obs.state.j1!, gripper: true }, { inReplyToTsUs: obs.timestampUs });
    });
    await until(() => observations > 10, 'observations before the blip');
    for (const scenario of ['signal-reconnect', 'resume-reconnect', 'full-reconnect'] as const) {
      await op.livekitRoom!.simulateScenario(scenario);
      const before = observations;
      await until(() => observations > before + 20, `observations after ${scenario}`, 15_000);
    }
    await r.livekitRoom!.simulateScenario('full-reconnect');
    const before = r.metrics()!.transport.actionsReceived;
    await until(() => r.metrics()!.transport.actionsReceived > before + 20 && r.activeOperator === 'op', 'robot keeps executing after its own reconnect', 15_000);
    stop();
  },

  async 'connect fails loudly on bad setups'() {
    const room = newRoom();
    const noMetadata = new Operator(new OperatorConfig(room, SPEC));
    await rejects(
      noMetadata.connect(url, await window.mintToken('x', room, { canUpdateOwnMetadata: false })),
      /canUpdateOwnMetadata/,
      'token without canUpdateOwnMetadata',
    );
    const alone = new Operator(new OperatorConfig(room, SPEC));
    await rejects(
      alone.connect(url, await window.mintToken('alone', room), { timeSyncTimeoutMs: 1500 }),
      /no clock sync/,
      'no robot in the room',
    );
    const bad = new Operator(new OperatorConfig(room, { fps: 0 }));
    await rejects(bad.connect(url, await window.mintToken('bad', room)), /fps/, 'invalid contract');
    const webrtc = new Robot(new RobotConfig(room, { videos: [{ name: 'cam', codec: 'h264' }] }));
    await rejects(webrtc.connect(url, await window.mintToken('robot', room)), /WebRTC video/, 'browser robot with h264');
    const keyedConfig = new RobotConfig(room, SPEC);
    keyedConfig.setE2eeKey('k');
    const keyed = new Robot(keyedConfig);
    await rejects(keyed.connect(url, await window.mintToken('robot', room)), /e2eeWorker/, 'E2EE key without a worker');

    const r = await robot(room);
    await rejects(r.connect(url, await window.mintToken('robot2', room)), /already connected/, 'double connect');
    const idle = new Operator(new OperatorConfig(room, SPEC));
    throws(() => idle.sendAction({ j1: 0, gripper: true }), /not connected/, 'send before connect');
    await rejects(idle.setActiveOperator('x'), /not connected/, 'handoff before connect');
    throws(() => r.sendState({ j1: 0, gripper: true, mode: 'x' as never }), /./, 'wrong value type');
    throws(() => r.sendVideoFrame('cam', new Uint8Array(3), { width: W, height: H }), /./, 'wrong frame size');
    throws(() => r.sendVideoFrame('nope', new Uint8Array(W * H * 3), { width: W, height: H }), /unknown video track/, 'unknown track');
  },

  async 'e2ee: a shared key connects, a wrong key never syncs'() {
    const worker = () => new Worker(new URL('livekit-client/e2ee-worker', import.meta.url), { type: 'module' });
    const room = newRoom();
    const rc = new RobotConfig(room, SPEC);
    rc.setE2eeKey('shared-secret');
    const r = await connect(new Robot(rc), 'robot', room, { e2eeWorker: worker() });
    const stop = drive(r);
    const oc = new OperatorConfig(room, SPEC);
    oc.setE2eeKey('shared-secret');
    const op = await connect(new Operator(oc), 'op', room, { e2eeWorker: worker() });
    let observations = 0;
    op.onObservation(() => observations++);
    await until(() => observations > 10, 'encrypted observations');
    const wc = new OperatorConfig(room, SPEC);
    wc.setE2eeKey('wrong');
    const wrong = new Operator(wc);
    peers.push(wrong);
    await rejects(
      wrong.connect(url, await window.mintToken('intruder', room), { e2eeWorker: worker(), timeSyncTimeoutMs: 2000 }),
      /no clock sync/,
      'wrong key',
    );
    stop();
  },

  async 'many operators and a long run keep matching'() {
    const room = newRoom();
    const r = await robot(room);
    const stop = drive(r);
    const ops = await Promise.all([1, 2, 3, 4].map((i) => operator(room, `op-${i}`)));
    const counts = ops.map(() => 0);
    ops.forEach((op, i) => op.onObservation(() => counts[i]!++));
    await sleep(10_000);
    stop();
    for (const [i, op] of ops.entries()) {
      const m = op.metrics()!;
      assert(counts[i]! > 250, `op-${i + 1} observations: ${counts[i]}`);
      assert(m.sync.statesDropped < 10, `op-${i + 1} dropped ${m.sync.statesDropped} states`);
    }
  },
};

window.scenarios = Object.fromEntries(
  Object.entries(scenarios).map(([name, run]) => [
    name,
    async (u: string) => {
      url = u;
      try {
        await run();
      } finally {
        await cleanup();
      }
    },
  ]),
);
