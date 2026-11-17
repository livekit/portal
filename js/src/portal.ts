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

// The browser driver. Every decision lives in `livekit-portal-core` (via
// wasm); this file only maps LiveKit room events to core inputs with the
// local time, and carries out the core's outputs as LiveKit calls. It
// mirrors `livekit-portal/src/portal.rs` and `driver.rs`.

import {
  ExternalE2EEKeyProvider,
  type Participant,
  type RemoteTrack,
  type RemoteTrackPublication,
  type RemoteVideoTrack,
  Room,
  RoomEvent,
  type RoomOptions,
} from 'livekit-client';

import { ObserverConfig, OperatorConfig, type PortalConfig, RobotConfig } from './config';
import { FrameStore, VideoReceiver } from './frames';
import type {
  Action,
  Keypoint,
  Observation,
  PortalMetrics,
  RpcHandler,
  State,
  Telemetry,
  Values,
  VideoFrameData,
} from './types';
import init, { Config, Core, wireNames } from './generated/livekit_portal_wasm';

export interface ConnectOptions {
  /**
   * Worker that reads Portal's frame timestamps off received video. Defaults
   * to `livekit-client/frame-metadata-worker`.
   */
  frameMetadataWorker?: Worker;
  /** Required when the config has an E2EE key: `livekit-client/e2ee-worker`. */
  e2eeWorker?: Worker;
  /** How long `connect` waits for the first clock sync with the robot. */
  timeSyncTimeoutMs?: number;
  /** Extra LiveKit room options. */
  roomOptions?: Partial<RoomOptions>;
}

export interface RpcOptions {
  /** Defaults to the obvious counterpart: the robot, or the active operator. */
  destination?: string;
  responseTimeoutMs?: number;
}

// Shapes the wasm core emits (js/wasm/src/js.rs).
interface JsFrame {
  handle: number | null;
  data: Uint8Array | null;
  width: number;
  height: number;
  timestampUs: number;
  source: VideoFrameData['source'];
}
interface RpcResult {
  ok: string | null;
  error: { code: number; message: string; data: string | null } | null;
}
type CoreEvent =
  | ({ kind: 'action' } & Action)
  | ({ kind: 'state' } & State)
  | ({ kind: 'telemetry' } & Telemetry)
  | { kind: 'videoFrame'; track: string; frame: JsFrame }
  | ({ kind: 'observation'; frames: Record<string, JsFrame> } & Omit<Observation, 'frames'>)
  | { kind: 'drops'; states: Values[] }
  | ({ kind: 'keypoint' } & Keypoint)
  | { kind: 'timeSynced' }
  | { kind: 'operatorJoined' | 'operatorLeft'; identity: string }
  | { kind: 'activeOperatorChanged'; identity: string | null }
  | { kind: 'rpcResult' | 'rpcRequest' };
type Output =
  | { type: 'publish'; topic: string; payload: Uint8Array<ArrayBuffer>; reliable: boolean; to: string | null }
  | { type: 'setAttributes'; id: number | null; attributes: Record<string, string> }
  | { type: 'rpc'; id: number; to: string; method: string; payload: string; timeoutMs: number | null }
  | { type: 'rpcReply'; id: number; result: RpcResult }
  | { type: 'event'; event: CoreEvent }
  | { type: 'wake'; atUs: number | null };
interface ConfigInfo {
  role: string;
  slack: number;
  videos: { name: string; webrtc: boolean }[];
  timeSyncSource: 'portal' | 'system';
  clockSkewUs: number;
}

const TIME_SYNC_TIMEOUT_MS = 10_000;
/** Floor on a wake-up's delay, so rounding never spins the loop. */
const MIN_WAKE_DELAY_MS = 1;
const BUILTIN_RPC = 1500;

let wasmReady: Promise<unknown> | undefined;
const loadWasm = () => (wasmReady ??= init());
const wire = () => wireNames() as { frameVideoTopic: string; setActiveOperatorRpc: string };

class Callbacks {
  action?: (a: Action) => void;
  state?: (s: State) => void;
  telemetry?: (t: Telemetry) => void;
  observation?: (o: Observation) => void;
  video = new Map<string, (track: string, f: VideoFrameData) => void>();
  drop?: (states: Values[]) => void;
  keypoint?: (k: Keypoint) => void;
  timeSynced?: () => void;
  operatorJoined?: (identity: string) => void;
  operatorLeft?: (identity: string) => void;
  activeOperatorChanged?: (identity: string | null) => void;
}

class Latest {
  action?: Action;
  state?: State;
  observation?: Observation;
  telemetry = new Map<string, Telemetry>();
  video = new Map<string, VideoFrameData>();
}

/** One LiveKit Portal peer. Use the `Robot`, `Operator` or `Observer` facade. */
export class Portal {
  private core?: Core;
  private info?: ConfigInfo;
  private room?: Room;
  private frames?: FrameStore;
  private timer?: ReturnType<typeof setTimeout>;
  private readonly receivers = new Map<string, VideoReceiver>();
  private readonly rpcHandlers = new Map<string, RpcHandler>();
  private readonly rpcWaiters = new Map<number, (r: RpcResult) => void>();
  private nextRpc = 0;
  private onSynced?: () => void;
  private readonly frameSenders = new Map<string, FrameSender>();
  private readonly cb = new Callbacks();
  private latest = new Latest();

  protected constructor(protected readonly config: PortalConfig) {}

  /** The underlying LiveKit room while connected. */
  get livekitRoom(): Room | undefined {
    return this.room;
  }

  async connect(url: string, token: string, options: ConnectOptions = {}): Promise<void> {
    if (this.room) throw new Error('portal is already connected');
    const core = await this.ensureCore();
    const info = this.info!;
    if (info.role === 'robot' && info.videos.some((v) => v.webrtc)) {
      throw new Error(
        'a browser robot cannot publish WebRTC video tracks yet; declare them as raw, png or mjpeg',
      );
    }
    const room = new Room({
      adaptiveStream: false,
      dynacast: false,
      ...options.roomOptions,
      frameMetadata: { worker: options.frameMetadataWorker ?? defaultFrameMetadataWorker() },
      encryption: await this.encryption(options),
    });
    this.room = room;
    this.frames = new FrameStore(info.slack + 2);
    try {
      this.listen(room);
      await room.connect(url, token);
      if (this.config.e2eeKey) await room.setE2EEEnabled(true);
      for (const [method, handler] of this.rpcHandlers) room.registerRpcMethod(method, handler);
      if (info.role === 'robot') room.registerRpcMethod(wire().setActiveOperatorRpc, this.relayRpc);
      const lp = room.localParticipant;
      await lp.setAttributes(core.roleAttributes()).catch((e: unknown) => {
        throw new Error(
          `failed to publish role attribute (token may be missing canUpdateOwnMetadata): ${e}`,
        );
      });
      this.execute(
        core.connect({
          localIdentity: lp.identity,
          localAttributes: lp.attributes,
          peers: [...room.remoteParticipants.values()].map(peer),
          nowUs: this.rawNowUs(),
        }),
      );
      room.remoteParticipants.forEach((p) =>
        p.videoTrackPublications.forEach((pub) => pub.track && this.subscribed(pub.track, pub)),
      );
      await this.waitForSync(options.timeSyncTimeoutMs ?? TIME_SYNC_TIMEOUT_MS);
    } catch (e) {
      await this.disconnect();
      throw e;
    }
  }

  async disconnect(): Promise<void> {
    const room = this.room;
    this.room = undefined;
    if (this.core) this.execute(this.core.disconnect());
    clearTimeout(this.timer);
    this.receivers.forEach((r) => r.stop());
    this.receivers.clear();
    this.frameSenders.clear();
    this.frames?.clear();
    this.latest = new Latest();
    await room?.disconnect();
  }

  // --- Send -----------------------------------------------------------------

  /** Robot only. `timestampUs` defaults to `nowUs()`. */
  sendState(values: Values, timestampUs?: number): void {
    this.step((c, nowUs) => c.sendState({ values, timestampUs, nowUs }));
  }

  /**
   * Operator only. Pass the observation's `timestampUs` as `inReplyToTsUs`
   * to give the robot end-to-end policy latency.
   */
  sendAction(values: Values, opts: { timestampUs?: number; inReplyToTsUs?: number } = {}): void {
    this.step((c, nowUs) => c.sendAction({ values, ...opts, nowUs }));
  }

  /** Any role. Lossy and never bundled into observations. */
  sendTelemetry(values: Values, timestampUs?: number): void {
    this.step((c, nowUs) => c.sendTelemetry({ values, timestampUs, nowUs }));
  }

  /** Robot only, RAW/PNG/MJPEG tracks: one RGB24 frame. */
  sendVideoFrame(track: string, rgb: Uint8Array, size: { width: number; height: number }, timestampUs?: number): void {
    const route = this.connected().sendVideoFrame({ track, rgb, ...size, timestampUs, nowUs: this.rawNowUs() }) as {
      payload: Uint8Array | null;
    };
    if (!route.payload) return;
    let sender = this.frameSenders.get(track);
    if (!sender) {
      const lp = this.room!.localParticipant;
      sender = new FrameSender((p) => lp.sendBytes(p, { topic: wire().frameVideoTopic }).then(() => {}));
      this.frameSenders.set(track, sender);
    }
    sender.send(route.payload);
  }

  /**
   * Send an annotation to everyone, this peer's own `onKeypoint` included.
   * Returns the observers present, so an empty list means nobody recorded it.
   */
  async sendKeypoint(type: string, payload: Record<string, unknown> = {}, timestampUs?: number): Promise<string[]> {
    const core = this.connected();
    const out = core.sendKeypoint({ type, payload, timestampUs, nowUs: this.rawNowUs() }) as Output[];
    for (const o of out) {
      if (o.type === 'publish') await this.publish(o);
      else this.execute([o]);
    }
    return core.observers();
  }

  // --- Receive --------------------------------------------------------------

  onAction(cb: (a: Action) => void): void {
    this.cb.action = cb;
  }
  onState(cb: (s: State) => void): void {
    this.cb.state = cb;
  }
  onTelemetry(cb: (t: Telemetry) => void): void {
    this.cb.telemetry = cb;
  }
  onObservation(cb: (o: Observation) => void): void {
    this.cb.observation = cb;
  }
  onVideoFrame(track: string, cb: (track: string, f: VideoFrameData) => void): void {
    this.cb.video.set(track, cb);
  }
  /** States that could not be matched to video. */
  onDrop(cb: (states: Values[]) => void): void {
    this.cb.drop = cb;
  }
  onKeypoint(cb: (k: Keypoint) => void): void {
    this.cb.keypoint = cb;
  }
  /** Fires on the first sync with the robot's clock, and after each resync. */
  onTimeSynced(cb: () => void): void {
    this.cb.timeSynced = cb;
  }
  onOperatorJoined(cb: (identity: string) => void): void {
    this.cb.operatorJoined = cb;
  }
  onOperatorLeft(cb: (identity: string) => void): void {
    this.cb.operatorLeft = cb;
  }
  onActiveOperatorChanged(cb: (identity: string | null) => void): void {
    this.cb.activeOperatorChanged = cb;
  }

  getAction(): Action | undefined {
    return this.latest.action;
  }
  getState(): State | undefined {
    return this.latest.state;
  }
  getObservation(): Observation | undefined {
    return this.latest.observation;
  }
  getTelemetry(sender: string): Telemetry | undefined {
    return this.latest.telemetry.get(sender);
  }
  getVideoFrame(track: string): VideoFrameData | undefined {
    return this.latest.video.get(track);
  }

  /** The subscribed WebRTC track, to show it in a `<video>` with `attach()`. */
  videoTrack(name: string): RemoteVideoTrack | undefined {
    for (const p of this.room?.remoteParticipants.values() ?? []) {
      const track = p.getTrackPublicationByName(name)?.videoTrack;
      if (track) return track as RemoteVideoTrack;
    }
    return undefined;
  }

  // --- Control plane --------------------------------------------------------

  get localIdentity(): string | undefined {
    return this.core?.localIdentity();
  }
  get activeOperator(): string | undefined {
    return this.core?.activeOperator();
  }
  get operators(): string[] {
    return this.core?.operators() ?? [];
  }
  get observers(): string[] {
    return this.core?.observers() ?? [];
  }
  get robotIdentity(): string | undefined {
    return this.core?.robotIdentity();
  }

  /**
   * Choose whose actions the robot executes; `null` clears it. A robot sets
   * it directly, anyone else asks the robot.
   */
  async setActiveOperator(identity: string | null): Promise<void> {
    const core = this.connected();
    if (this.info!.role !== 'robot') await this.resolveRobot();
    const out = core.setActiveOperator({ identity, nowUs: this.rawNowUs() }) as Output[];
    for (const o of out) {
      if (o.type === 'setAttributes' && o.id !== null) {
        const error = await this.room!.localParticipant.setAttributes(o.attributes).then(
          () => null,
          (e: unknown) => String(e),
        );
        this.step((c, nowUs) => c.presenceSet({ id: o.id, error, nowUs }));
        if (error) throw new Error(error);
      } else if (o.type === 'rpc') {
        const result = await this.performRpc(o.method, o.payload, { destination: o.to }).then(
          (ok): RpcResult => ({ ok, error: null }),
          (e: { code?: number; message: string; data?: string }): RpcResult => ({
            ok: null,
            error: { code: e.code ?? BUILTIN_RPC, message: e.message, data: e.data ?? null },
          }),
        );
        this.step((c, nowUs) => c.rpcResponse({ id: o.id, result, nowUs }));
        if (result.error) throw Object.assign(new Error(result.error.message), result.error);
      } else {
        this.execute([o]);
      }
    }
  }

  registerRpcMethod(method: string, handler: RpcHandler): void {
    this.rpcHandlers.set(method, handler);
    this.room?.unregisterRpcMethod(method);
    this.room?.registerRpcMethod(method, handler);
  }

  unregisterRpcMethod(method: string): void {
    this.rpcHandlers.delete(method);
    this.room?.unregisterRpcMethod(method);
  }

  async performRpc(method: string, payload: string, opts: RpcOptions = {}): Promise<string> {
    const room = this.room;
    if (!room) throw new Error('portal is not connected');
    const destinationIdentity = opts.destination ?? this.resolvePeer(room);
    return room.localParticipant.performRpc({
      destinationIdentity,
      method,
      payload,
      responseTimeout: opts.responseTimeoutMs,
    });
  }

  // --- Clock and metrics ----------------------------------------------------

  /** Now, in microseconds on the robot's clock. */
  nowUs(): number {
    return this.core ? this.core.nowUs(this.rawNowUs()) : this.rawNowUs();
  }

  metrics(): PortalMetrics | undefined {
    return this.core?.metrics();
  }

  resetMetrics(): void {
    this.core?.resetMetrics();
  }

  // --- Driver ---------------------------------------------------------------

  private async ensureCore(): Promise<Core> {
    if (this.core) return this.core;
    await loadWasm();
    const config = Config.fromYaml(this.config.yaml, this.config.session, this.config.role);
    if (this.config.e2eeKey) config.setE2eeKey(this.config.e2eeKey);
    config.setClockSkewUs(this.config.clockSkewUs);
    this.info = config.describe();
    this.core = new Core(config);
    config.free();
    return this.core;
  }

  private connected(): Core {
    if (!this.core || !this.room) throw new Error('portal is not connected');
    return this.core;
  }

  private async encryption(options: ConnectOptions): Promise<RoomOptions['encryption']> {
    const key = this.config.e2eeKey;
    if (!key) return undefined;
    if (!options.e2eeWorker) throw new Error('an E2EE key is set: pass ConnectOptions.e2eeWorker');
    const keyProvider = new ExternalE2EEKeyProvider();
    await keyProvider.setKey(key.slice().buffer);
    return { keyProvider, worker: options.e2eeWorker };
  }

  /** Raw local time: wall time advanced monotonically, or the host clock as is. */
  private rawNowUs(): number {
    const ms =
      this.info?.timeSyncSource === 'system' ? Date.now() : performance.timeOrigin + performance.now();
    return Math.floor(ms * 1000) + (this.info?.clockSkewUs ?? 0);
  }

  /** Step the core with the local time and carry out what it returns. */
  private step(f: (core: Core, nowUs: number) => unknown): void {
    const core = this.core;
    if (!core) throw new Error('portal is not connected');
    this.execute(f(core, this.rawNowUs()) as Output[]);
  }

  private execute(out: Output[]): void {
    for (const o of out) {
      switch (o.type) {
        case 'publish':
          this.publish(o).catch((e) => console.warn(`[livekit-portal] [publish-failed] ${o.topic}: ${e}`));
          break;
        case 'setAttributes':
          this.setAttributes(o);
          break;
        case 'rpc':
          // Only `setActiveOperator` starts one, and it performs the call.
          this.step((c, nowUs) =>
            c.rpcResponse({ id: o.id, result: { ok: null, error: { code: BUILTIN_RPC, message: 'unexpected rpc', data: null } }, nowUs }),
          );
          break;
        case 'rpcReply':
          this.rpcWaiters.get(o.id)?.(o.result);
          this.rpcWaiters.delete(o.id);
          break;
        case 'event':
          this.dispatch(o.event);
          break;
        case 'wake':
          clearTimeout(this.timer);
          if (o.atUs !== null) {
            const delay = Math.max(MIN_WAKE_DELAY_MS, (o.atUs - this.rawNowUs()) / 1000);
            this.timer = setTimeout(() => this.step((c, nowUs) => c.wake(nowUs)), delay);
          }
          break;
      }
    }
  }

  private async publish(o: Extract<Output, { type: 'publish' }>): Promise<void> {
    await this.room?.localParticipant.publishData(o.payload, {
      topic: o.topic,
      reliable: o.reliable,
      destinationIdentities: o.to ? [o.to] : undefined,
    });
  }

  private setAttributes(o: Extract<Output, { type: 'setAttributes' }>): void {
    const lp = this.room?.localParticipant;
    const done = (error: string | null) => {
      if (o.id !== null && this.core) this.step((c, nowUs) => c.presenceSet({ id: o.id, error, nowUs }));
    };
    if (!lp) return done('not connected');
    lp.setAttributes(o.attributes).then(
      () => done(null),
      (e: unknown) => done(String(e)),
    );
  }

  private dispatch(e: CoreEvent): void {
    const { cb, latest } = this;
    switch (e.kind) {
      case 'action': {
        const { kind: _, ...a } = e;
        latest.action = a;
        cb.action?.(a);
        break;
      }
      case 'state': {
        const { kind: _, ...s } = e;
        latest.state = s;
        cb.state?.(s);
        break;
      }
      case 'telemetry': {
        const { kind: _, ...t } = e;
        latest.telemetry.set(t.sender, t);
        cb.telemetry?.(t);
        break;
      }
      case 'videoFrame': {
        const frame = this.frame(e.frame);
        latest.video.set(e.track, frame);
        cb.video.get(e.track)?.(e.track, frame);
        break;
      }
      case 'observation': {
        const frames = Object.fromEntries(Object.entries(e.frames).map(([k, f]) => [k, this.frame(f)]));
        const o = { state: e.state, rawState: e.rawState, frames, timestampUs: e.timestampUs };
        latest.observation = o;
        cb.observation?.(o);
        break;
      }
      case 'drops':
        cb.drop?.(e.states);
        break;
      case 'keypoint': {
        const { kind: _, ...k } = e;
        cb.keypoint?.(k);
        break;
      }
      case 'timeSynced':
        this.onSynced?.();
        cb.timeSynced?.();
        break;
      case 'operatorJoined':
        cb.operatorJoined?.(e.identity);
        break;
      case 'operatorLeft':
        cb.operatorLeft?.(e.identity);
        break;
      case 'activeOperatorChanged':
        cb.activeOperatorChanged?.(e.identity);
        break;
      default:
        // RPC polyfill events never fire on LiveKit, which carries RPC natively.
        break;
    }
  }

  private frame(f: JsFrame): VideoFrameData {
    const { handle, data, ...rest } = f;
    return {
      ...rest,
      ...(handle !== null && { frame: this.frames?.get(handle) }),
      ...(data !== null && { data }),
    };
  }

  private listen(room: Room): void {
    const step = this.step.bind(this);
    room
      .on(RoomEvent.DataReceived, (payload, participant, _kind, topic) => {
        if (topic) step((c, nowUs) => c.data({ topic, from: participant?.identity ?? null, payload, nowUs }));
      })
      .on(RoomEvent.ParticipantConnected, (p) => step((c, nowUs) => c.peerJoined({ ...peer(p), nowUs })))
      // Includes this peer's own updates, which the core ignores.
      .on(RoomEvent.ParticipantAttributesChanged, (_changed, p) =>
        step((c, nowUs) => c.attributesChanged({ ...peer(p), nowUs })),
      )
      .on(RoomEvent.ParticipantDisconnected, (p) =>
        step((c, nowUs) => c.peerLeft({ identity: p.identity, nowUs })),
      )
      .on(RoomEvent.TrackSubscribed, (track, pub) => this.subscribed(track, pub))
      .on(RoomEvent.TrackUnsubscribed, (_track, pub) => {
        this.receivers.get(pub.trackName)?.stop();
        this.receivers.delete(pub.trackName);
      })
      .on(RoomEvent.Reconnected, () => {
        // Pre-reconnect data is stale, matching the core's cleared sync buffer.
        this.latest = new Latest();
        step((c, nowUs) => c.linkRestored(nowUs));
      });
    room.registerByteStreamHandler(wire().frameVideoTopic, async (reader) => {
      if (!this.core?.receivesFrameBytes()) return;
      const payload = concat(await reader.readAll());
      step((c, nowUs) => c.frameBytes({ payload, nowUs }));
    });
  }

  private subscribed(track: RemoteTrack, pub: RemoteTrackPublication): void {
    const name = pub.trackName;
    const declared = this.info?.videos.some((v) => v.webrtc && v.name === name);
    if (this.info?.role === 'robot' || track.kind !== 'video' || !declared || this.receivers.has(name)) return;
    const receiver = new VideoReceiver(track as RemoteVideoTrack, (frame, timestampUs) => {
      const handle = this.frames!.add(name, frame);
      const { displayWidth: width, displayHeight: height } = frame;
      this.step((c, nowUs) => c.videoFrame({ track: name, handle, width, height, timestampUs, nowUs }));
    });
    this.receivers.set(name, receiver);
  }

  /** Robot side: relay `set_active_operator` to the core and wait for its answer. */
  private relayRpc: RpcHandler = (data) => {
    const id = this.nextRpc++;
    const reply = new Promise<RpcResult>((resolve) => this.rpcWaiters.set(id, resolve));
    this.step((c, nowUs) =>
      c.rpcRequest({ id, method: wire().setActiveOperatorRpc, from: data.callerIdentity, payload: data.payload, nowUs }),
    );
    return reply.then((r) => {
      if (r.error) throw Object.assign(new Error(r.error.message), r.error);
      return r.ok ?? '';
    });
  };

  private waitForSync(timeoutMs: number): Promise<void> {
    if (this.core!.timeSynced()) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`no clock sync with the robot within ${timeoutMs} ms; is it in the room, with the same E2EE key and Portal version?`)),
        timeoutMs,
      );
      this.onSynced = () => {
        clearTimeout(timer);
        this.onSynced = undefined;
        resolve();
      };
    });
  }

  /**
   * `setActiveOperator` is usually called right after `connect`, before the
   * robot's attributes have surfaced: wait briefly for them.
   */
  private async resolveRobot(): Promise<void> {
    for (let i = 0; i < 30 && !this.core?.robotIdentity(); i++) {
      this.room?.remoteParticipants.forEach((p) => this.step((c, nowUs) => c.attributesChanged({ ...peer(p), nowUs })));
      if (this.core?.robotIdentity()) return;
      await new Promise((r) => setTimeout(r, 50));
    }
  }

  private resolvePeer(room: Room): string {
    const id = this.core?.defaultPeer();
    if (id) return id;
    const remotes = [...room.remoteParticipants.keys()];
    if (remotes.length === 1) return remotes[0]!;
    throw new Error(
      remotes.length === 0
        ? 'no peer in the room'
        : 'room has multiple remote participants and no peer has been identified yet; pass destination explicitly',
    );
  }
}

/** Sends the newest frame per track; a frame that arrives mid-send replaces the waiting one. */
class FrameSender {
  private pending?: Uint8Array;
  private busy = false;

  constructor(private readonly sendBytes: (payload: Uint8Array) => Promise<void>) {}

  send(payload: Uint8Array): void {
    this.pending = payload;
    if (!this.busy) void this.drain();
  }

  private async drain(): Promise<void> {
    this.busy = true;
    for (let p = this.pending; p; p = this.pending) {
      this.pending = undefined;
      await this.sendBytes(p).catch((e) => console.warn(`[livekit-portal] [publish-failed] frame_video: ${e}`));
    }
    this.busy = false;
  }
}

function peer(p: Participant) {
  return { identity: p.identity, attributes: p.attributes };
}

function concat(chunks: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

function defaultFrameMetadataWorker(): Worker {
  return new Worker(new URL('livekit-client/frame-metadata-worker', import.meta.url), { type: 'module' });
}

export class Robot extends Portal {
  constructor(config: RobotConfig) {
    super(config);
  }
}

export class Operator extends Portal {
  constructor(config: OperatorConfig) {
    super(config);
  }
}

/** Sees everything in the room but never sends state or actions. */
export class Observer extends Portal {
  constructor(config: ObserverConfig) {
    super(config);
  }
}
