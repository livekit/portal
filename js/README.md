# @livekit/portal (browser)

LiveKit Portal for web pages. Teleoperate a robot, watch a session, or run a
policy from a browser, in the same room as Python and Rust peers.

It is the same Portal: `livekit-portal-core` compiled to WebAssembly
([`wasm/`](wasm)) makes every decision (sync, clock, control handoff, wire
format), and a thin TypeScript driver ([`src/portal.ts`](src/portal.ts))
connects it to [`livekit-client`](https://github.com/livekit/client-sdk-js).
See [transports](../docs/transports.md#in-the-browser) for how it maps.

## Install

Not on npm yet. Build it from this repo and add it to your app:

```bash
# Rust with the wasm target, and the wasm-bindgen CLI matching js/wasm/Cargo.toml
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked

cd portal/js
pnpm install
pnpm build                      # wasm + TypeScript into dist/

cd your-app
pnpm add /path/to/portal/js livekit-client   # or npm install / yarn add
```

`livekit-client` (2.24.1 or newer) is a peer dependency. The package is ESM and
loads its `.wasm` and the frame-metadata worker with `new URL(..., import.meta.url)`,
which Vite, webpack 5, Rollup and esbuild-based bundlers resolve on their own.

Tokens must allow `canUpdateOwnMetadata` (Portal announces each peer's role in
its attributes). Mint them on your server, never in the page.

## Use

```ts
import { Operator, OperatorConfig } from '@livekit/portal';
import contract from './portal.yaml?raw'; // the same file every role loads

const op = new Operator(OperatorConfig.fromYaml(contract, 'my-room'));

op.onObservation((obs) => {
  // obs.frames.cam1.frame is a VideoFrame, obs.state the matched state
  op.sendAction(policy(obs), { inReplyToTsUs: obs.timestampUs });
});

await op.connect(url, token); // token from your server
op.videoTrack('cam1')?.attach(videoElement); // show the camera
await op.setActiveOperator(op.localIdentity!);
```

The API mirrors Python's, in camelCase: `Robot`, `Operator` and `Observer`,
`send*`, `on*` callbacks and `get*` latest values, `setActiveOperator`,
`sendKeypoint`, `performRpc` / `registerRpcMethod`, `metrics()`. A config can
also be built from an object with the YAML file's fields:
`new OperatorConfig('my-room', { fps: 30, videos: [...], state: [...] })`.

**Frames.** WebRTC tracks deliver browser `VideoFrame`s. Portal closes each
one a few frames later; `clone()` it to keep it. RAW, PNG and MJPEG tracks
deliver RGB24 bytes in `data`.

**Limits.** A browser robot can't publish WebRTC video yet: declare its
tracks as RAW, PNG or MJPEG. With an E2EE key, pass
`connect(url, token, { e2eeWorker })` with `livekit-client/e2ee-worker`.
Video is read from the decoder with `MediaStreamTrackProcessor` (Chromium),
or from a hidden `<video>` elsewhere, which adds about 20 ms.

## Develop

```bash
pnpm build      # wasm + TypeScript into dist/
pnpm typecheck
pnpm test       # the wasm binding, in Node
pnpm exec playwright install chromium
pnpm test:browser   # real peers in Chromium against `livekit-server --dev`
```
