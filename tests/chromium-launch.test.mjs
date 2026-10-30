// Startup classification for tests/chromium-launch.mjs using owned fake
// binaries (deterministic) plus one real Chromium readiness check when present.
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import http from 'node:http'
import { launchChromium, STDERR_CAP, byteTail } from './chromium-launch.mjs'
import { teardown } from './harness-teardown.mjs'

const mk = () => fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'cvd-launch-'))
function fake(dir, body) { const f = path.join(dir, 'fake-chromium'); fs.writeFileSync(f, '#!/bin/sh\n' + body + '\n', { mode: 0o755 }); return f }
async function run(bin, budgetMs, dir, pollMs) {
  const profile = path.join(dir, 'p'); fs.mkdirSync(profile)
  const t0 = Date.now(), { child, promise } = launchChromium({ bin, profile, budgetMs, ...(pollMs ? { pollMs } : {}) })
  let err, ok; try { ok = await promise } catch (e) { err = e }
  const td = await teardown({ child, profileDir: profile, opts: { graceMs: 1000, killMs: 2000 } })
  return { err, ok, td, ms: Date.now() - t0, budgetMs }
}
// Owned loopback fake CDP endpoint: sends headers, then the JSON body after bodyDelayMs.
async function fakeCdp(bodyDelayMs) {
  const srv = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' }); res.flushHeaders()
    setTimeout(() => res.end(JSON.stringify({ Browser: 'Fake/1', webSocketDebuggerUrl: 'ws://127.0.0.1:1/devtools/browser/fake' })), bodyDelayMs)
  })
  await new Promise(r => srv.listen(0, '127.0.0.1', r))
  return { port: srv.address().port, close: () => { srv.closeAllConnections(); return new Promise(r => srv.close(r)) } }
}
const portFileAfter = (sec, port) => `sleep ${sec}; printf '%s\\n/devtools/browser/fake\\n' ${port} > "$(dirname "$0")/p/DevToolsActivePort"; exec sleep 60`

test('missing binary fails promptly as spawn-error with clean teardown', async () => {
  const d = mk(); const r = await run(path.join(d, 'nope'), 10000, d)
  assert.equal(r.err.kind, 'spawn-error'); assert.match(r.err.message, /^chromium did not start/)
  assert.ok(r.ms < 3000, `${r.ms}ms`); assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})

test('early exit fails promptly and keeps capped stderr tail', async () => {
  const d = mk(); const r = await run(fake(d, `head -c 9000 /dev/zero | tr '\\0' x >&2; echo LAST-LINE >&2; exit 7`), 10000, d)
  assert.equal(r.err.kind, 'exited-early'); assert.match(r.err.message, /code=7/)
  assert.ok(r.err.stderrTail.length <= STDERR_CAP && r.err.stderrTail.includes('LAST-LINE'))
  assert.ok(r.ms < 3000, `${r.ms}ms`); assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})

test('invalid UTF-8 tail stays within byte cap after replacement expansion', () => {
  const t = byteTail(Buffer.alloc(STDERR_CAP, 255))
  assert.ok(Buffer.byteLength(t, 'utf8') <= STDERR_CAP, `${Buffer.byteLength(t, 'utf8')} bytes`)
  const mixed = byteTail(Buffer.concat([Buffer.alloc(STDERR_CAP, 255), Buffer.from('LAST-LINE\n')]))
  assert.ok(Buffer.byteLength(mixed, 'utf8') <= STDERR_CAP && mixed.endsWith('LAST-LINE\n'))
  assert.equal(byteTail(Buffer.from('€'.repeat(3000))).length * 3, Buffer.byteLength(byteTail(Buffer.from('€'.repeat(3000))), 'utf8'))
})

test('multibyte stderr tail is capped in UTF-8 bytes and stays valid text', async () => {
  const d = mk(); const r = await run(fake(d, `i=0; while [ $i -lt 3000 ]; do printf '€' >&2; i=$((i+1)); done; echo LAST-LINE >&2; exit 3`), 10000, d)
  assert.equal(r.err.kind, 'exited-early'); const t = r.err.stderrTail
  assert.ok(Buffer.byteLength(t, 'utf8') <= STDERR_CAP, `${Buffer.byteLength(t, 'utf8')} bytes`)
  assert.ok(t.endsWith('LAST-LINE\n') && !t.includes('\uFFFD') && t.startsWith('€'))
  assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})

test('late CDP answer finishing after the absolute budget is rejected as timeout', async () => {
  // Endpoint appears <=40ms before the deadline; body completes 80ms later
  // (inside the old 100ms probe floor, past the absolute deadline).
  const s = await fakeCdp(80); const d = mk()
  try {
    const r = await run(fake(d, portFileAfter(0.76, s.port)), 800, d, 5)
    assert.equal(r.ok, undefined, `accepted late at ${r.ok?.ms}ms > budget ${r.budgetMs}ms`)
    assert.equal(r.err.kind, 'timeout'); assert.ok(r.ms < 3000, `${r.ms}ms`); assert.equal(r.td.ok, true)
  } finally { await s.close(); fs.rmSync(d, { recursive: true }) }
})

test('control: same fake endpoint answering within budget is accepted', async () => {
  const s = await fakeCdp(120); const d = mk()
  try {
    const r = await run(fake(d, portFileAfter(0.1, s.port)), 3000, d, 5)
    assert.ifError(r.err); assert.equal(r.ok.via, 'DevToolsActivePort'); assert.ok(r.ok.ms <= 3000)
    assert.equal(r.ok.httpBase, `http://127.0.0.1:${s.port}`); assert.equal(r.td.ok, true)
  } finally { await s.close(); fs.rmSync(d, { recursive: true }) }
})

test('invalid budget/poll options are rejected before spawning', () => {
  for (const o of [{ budgetMs: 0 }, { budgetMs: -1 }, { budgetMs: NaN }, { budgetMs: Infinity }, { pollMs: 0 }, { budgetMs: '20000' }])
    assert.throws(() => launchChromium({ bin: '/nonexistent', profile: '/nonexistent', spawnFn: () => assert.fail('spawned'), ...o }), TypeError)
})

test('stalled binary hits the absolute budget, then is reaped', async () => {
  const d = mk(); const r = await run(fake(d, 'echo stalling >&2; exec sleep 60'), 800, d)
  assert.equal(r.err.kind, 'timeout'); assert.match(r.err.stderrTail, /stalling/)
  assert.ok(r.ms < 5000, `${r.ms}ms`); assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})

test('banner without a real CDP endpoint is not accepted as ready', async () => {
  const d = mk(); const r = await run(fake(d, 'echo "DevTools listening on ws://127.0.0.1:9/devtools/browser/x" >&2; exec sleep 60'), 1200, d)
  assert.equal(r.err.kind, 'timeout'); assert.match(r.err.message, /CDP probe/)
  assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})

const real = process.env.CVD_CHROMIUM || '/usr/bin/chromium'
test('real Chromium reaches CDP readiness', { skip: !fs.existsSync(real) && 'no chromium' }, async () => {
  const d = mk(); const r = await run(real, 20000, d)
  assert.ifError(r.err); assert.match(r.ok.wsUrl, /^ws:\/\/127\.0\.0\.1:\d+\/devtools\/browser\//)
  assert.equal(r.ok.via, 'DevToolsActivePort'); assert.equal(r.td.ok, true); fs.rmSync(d, { recursive: true })
})
