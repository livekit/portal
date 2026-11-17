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

// The wasm binding's JS shapes. Portal's logic is tested in Rust; these only
// check what crosses the boundary, with two cores wired back to back the way
// the LiveKit driver wires them.
import { readFileSync } from 'node:fs';
import { beforeAll, describe, expect, it } from 'vitest';

import { Config, Core, initSync, wireNames } from './generated/livekit_portal_wasm';

const CONTRACT = JSON.stringify({
  version: 1,
  fps: 30,
  videos: [{ name: 'cam', codec: 'mjpeg' }],
  state: [
    { name: 'j1', dtype: 'f32' },
    { name: 'gripper', dtype: 'bool' },
  ],
  action: [{ name: 'j1', dtype: 'f32' }],
});
const ROLE = { 'lk.portal.role': 'robot', 'lk.portal.version': '3' };
const T0 = 1_700_000_000_000_000;

type Out = { type: string; [k: string]: any };

beforeAll(() => {
  initSync({ module: readFileSync(new URL('./generated/livekit_portal_wasm_bg.wasm', import.meta.url)) });
});

function core(role: string): Core {
  return new Core(Config.fromYaml(CONTRACT, 'lab', role));
}

/** Deliver one core's publishes to another, as a LiveKit room would. */
function deliver(out: Out[], from: string, to: Core, nowUs: number): Out[] {
  return out
    .filter((o) => o.type === 'publish')
    .flatMap((o) => to.data({ topic: o.topic, from, payload: o.payload, nowUs }) as Out[]);
}

const events = (out: Out[]) => out.filter((o) => o.type === 'event').map((o) => o.event);

describe('wasm core', () => {
  it('rejects an invalid contract with the loader message', () => {
    const bad = JSON.stringify({ version: 1, fps: 0 });
    expect(() => Config.fromYaml(bad, 'lab', 'robot')).toThrow(/fps must be > 0/);
    expect(() => Config.fromYaml(CONTRACT, 'lab', 'pilot')).toThrow(/unknown role/);
  });

  it('describes the config', () => {
    const info = Config.fromYaml(CONTRACT, 'lab', 'operator').describe();
    expect(info).toMatchObject({ role: 'operator', fps: 30, observationSync: true });
    expect(info.videos).toEqual([expect.objectContaining({ name: 'cam', codec: 'mjpeg', webrtc: false })]);
  });

  it('publishes bytes as Uint8Array on LiveKit topics', () => {
    const robot = core('robot');
    robot.connect({ localIdentity: 'robot', localAttributes: {}, peers: [], nowUs: T0 });
    const [publish] = robot.sendState({ values: { j1: 0.5, gripper: true }, nowUs: T0 + 1 }) as Out[];
    expect(publish).toMatchObject({ type: 'publish', topic: 'portal_state', reliable: false, to: null });
    expect(publish!.payload).toBeInstanceOf(Uint8Array);
    const route = robot.sendVideoFrame({ track: 'cam', rgb: new Uint8Array(4 * 4 * 3), width: 4, height: 4, nowUs: T0 });
    expect(route.transport).toBe('bytes');
    expect(route.payload).toBeInstanceOf(Uint8Array);
    expect(typeof route.timestampUs).toBe('number');
  });

  it('carries state, keypoints and clock sync from robot to operator', () => {
    const robot = core('robot');
    const op = core('operator');
    robot.connect({ localIdentity: 'robot', localAttributes: {}, peers: [], nowUs: T0 });
    let out = op.connect({
      localIdentity: 'op',
      localAttributes: {},
      peers: [{ identity: 'robot', attributes: ROLE }],
      nowUs: T0,
    }) as Out[];
    // Ping, pong, and wake when the core asks, until the clock syncs.
    const seen: Out[] = [];
    for (let now = T0; !op.timeSynced() && now < T0 + 10_000_000; ) {
      const wake = out.find((o) => o.type === 'wake')?.atUs ?? now + 100_000;
      const pongs = deliver(out, 'op', robot, now + 1000);
      seen.push(...deliver(pongs, 'robot', op, now + 2000));
      now = Math.max(wake, now + 2000);
      out = op.wake(now) as Out[];
      seen.push(...out);
    }
    expect(events(seen)).toContainEqual({ kind: 'timeSynced' });

    const state = robot.sendState({ values: { j1: 0.5, gripper: true }, timestampUs: T0, nowUs: T0 }) as Out[];
    expect(events(deliver(state, 'robot', op, T0 + 3000))).toContainEqual({
      kind: 'state',
      values: { j1: 0.5, gripper: true },
      rawValues: { j1: 0.5, gripper: 1 },
      timestampUs: T0,
    });

    const mark = robot.sendKeypoint({ type: 'recording', payload: { task: 'pick' }, timestampUs: T0, nowUs: T0 }) as Out[];
    expect(events(deliver(mark, 'robot', op, T0 + 4000))).toContainEqual({
      kind: 'keypoint',
      type: 'recording',
      payload: { task: 'pick' },
      timestampUs: T0,
      sender: 'robot',
    });
  });

  it('decodes frame bytes into RGB frames', () => {
    const robot = core('robot');
    const op = core('operator');
    robot.connect({ localIdentity: 'robot', localAttributes: {}, peers: [], nowUs: T0 });
    op.connect({ localIdentity: 'op', localAttributes: {}, peers: [{ identity: 'robot', attributes: ROLE }], nowUs: T0 });
    const { payload } = robot.sendVideoFrame({ track: 'cam', rgb: new Uint8Array(4 * 4 * 3), width: 4, height: 4, timestampUs: T0, nowUs: T0 });
    const frames = events(op.frameBytes({ payload, nowUs: T0 + 1000 }) as Out[]).filter((e) => e.kind === 'videoFrame');
    expect(frames).toHaveLength(1);
    expect(frames[0].frame).toMatchObject({ handle: null, width: 4, height: 4, timestampUs: T0, source: 'live' });
    expect(frames[0].frame.data).toBeInstanceOf(Uint8Array);
  });

  it('maps the robot pointer to LiveKit attributes', () => {
    const robot = core('robot');
    robot.connect({ localIdentity: 'robot', localAttributes: {}, peers: [], nowUs: T0 });
    const out = robot.setActiveOperator({ identity: 'op', nowUs: T0 }) as Out[];
    expect(out).toContainEqual({ type: 'setAttributes', id: expect.any(Number), attributes: { 'lk.portal.active_operator': 'op' } });
    expect(wireNames()).toEqual({ frameVideoTopic: 'portal_frame_video', setActiveOperatorRpc: 'portal.set_active_operator' });
  });
});
