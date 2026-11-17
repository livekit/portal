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

// Browser integration tests: every scenario in scenarios.ts, in Chromium,
// against a LiveKit server (default: `livekit-server --dev`).
//   pnpm test:browser [name-filter]
import { fileURLToPath } from 'node:url';
import { AccessToken } from 'livekit-server-sdk';
import { chromium } from 'playwright';
import { createServer } from 'vite';

const url = process.env.LIVEKIT_URL ?? 'ws://localhost:7880';
const key = process.env.LIVEKIT_API_KEY ?? 'devkey';
const secret = process.env.LIVEKIT_API_SECRET ?? 'secret';
const filter = process.argv[2] ?? '';
const root = fileURLToPath(new URL('.', import.meta.url));

const server = await createServer({
  root,
  logLevel: 'warn',
  resolve: { alias: { '@livekit/portal': fileURLToPath(new URL('../../src/index.ts', import.meta.url)) } },
  server: { port: 0 },
});
await server.listen();
// `PORTAL_TEST_BROWSER_CHANNEL=chrome` uses an installed Chrome instead of
// Playwright's Chromium (CI runners ship one).
const browser = await chromium.launch({ channel: process.env.PORTAL_TEST_BROWSER_CHANNEL });
let failed = 0;
try {
  const page = await browser.newPage();
  const logs = [];
  const verbose = !!process.env.PORTAL_TEST_VERBOSE;
  page.on('console', (m) => {
    const line = `[${m.type()}] ${m.text()}`;
    if (verbose && (line.includes('livekit-portal') || line.includes('[dbg]'))) console.log(line);
    logs.push(line);
  });
  page.on('pageerror', (e) => logs.push(`[pageerror] ${e.stack ?? e}`));
  await page.exposeFunction('mintToken', async (identity, room, grants = {}) => {
    const at = new AccessToken(key, secret, { identity });
    at.addGrant({ roomJoin: true, room, canPublish: true, canSubscribe: true, canUpdateOwnMetadata: true, ...grants });
    return at.toJwt();
  });
  await page.goto(server.resolvedUrls.local[0]);
  await page.waitForFunction(() => window.scenarios);
  const names = (await page.evaluate(() => Object.keys(window.scenarios))).filter((n) => n.includes(filter));
  for (const name of names) {
    logs.length = 0;
    const started = Date.now();
    const error = await page.evaluate(
      ([name, url]) => window.scenarios[name](url).then(() => null, (e) => String(e?.stack ?? e)),
      [name, url],
    );
    const ms = Date.now() - started;
    if (error) {
      failed++;
      console.log(`FAIL ${name} (${ms} ms)\n${error}\n${logs.slice(-15).join('\n')}`);
    } else {
      console.log(`ok   ${name} (${ms} ms)`);
    }
  }
} finally {
  await browser.close();
  await server.close();
}
process.exit(failed ? 1 : 0);
