// Real-browser smoke for the cvd-wasm v1 Rust host + trusted Worker bridge.
// Serves repo static/wasm-host and wasm-release artifacts on loopback only,
// drives installed Chromium over CDP, asserts every step and compares the
// browser plugin output to the native C-ABI oracle bit-for-bit.
//   CVD_WASM_DIR   wasm32 wasm-release dir (required)
//   CVD_FIXTURE_DIR owned fixture dir from `native_oracle --fixtures` (request,
//                   oracle, live-script, hostile/*.wasm + index.json)
// Run via scripts/wasm-foundation-gate.sh (builds + generates everything).
//   CVD_CHROMIUM   default /usr/bin/chromium
import http from 'node:http'
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import { spawn } from 'node:child_process'
import assert from 'node:assert/strict'
import { teardown } from './harness-teardown.mjs'

const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..')
const WASM = process.env.CVD_WASM_DIR
const FXD = process.env.CVD_FIXTURE_DIR
assert.ok(WASM && FXD, 'CVD_WASM_DIR and CVD_FIXTURE_DIR are required')
const FIX = path.join(ROOT, 'tests/fixtures/wasm')
const types = { '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.html': 'text/html' }
const routes = (u) => {
  if (u === '/smoke.html') return path.join(FIX, 'smoke.html')
  if (u.startsWith('/static/wasm-host/')) return path.join(ROOT, 'static/wasm-host', path.basename(u))
  if (u.startsWith('/fixtures/hostile/')) return path.join(FXD, 'hostile', path.basename(u))
  if (u.startsWith('/fixtures/')) return path.join(FXD, path.basename(u))
  if (u.startsWith('/artifacts/')) return path.join(WASM, path.basename(u))
  return null
}
const external = []
const server = http.createServer((req, res) => {
  const f = routes(req.url.split('?')[0])
  if (!f || !fs.existsSync(f)) { res.writeHead(404); return res.end() }
  res.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream', 'cache-control': 'no-store' })
  fs.createReadStream(f).pipe(res)
})
const sockets = new Set()
server.on('connection', s => { sockets.add(s); s.on('close', () => sockets.delete(s)) })
await new Promise(r => server.listen(0, '127.0.0.1', r))
const port = server.address().port
const profile = fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'cvd-wasm-smoke-'))
const chrome = spawn(process.env.CVD_CHROMIUM || '/usr/bin/chromium', ['--headless=new', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu',
  '--no-first-run', '--remote-debugging-port=0', '--user-data-dir=' + profile, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] })
let result, ws
// Every wait below is bounded so the finally/teardown path is always reachable.
const bounded = (p, ms, what) => { let t; return Promise.race([p, new Promise((_, rej) => { t = setTimeout(() => rej(new Error(`${what} timed out after ${ms}ms`)), ms) })]).finally(() => clearTimeout(t)) }
try {
  const wsUrl = await new Promise((resolve, reject) => {
    let buf = ''; const t = setTimeout(() => reject(new Error('chromium did not start')), 20000)
    chrome.stderr.on('data', d => { buf += d; const m = buf.match(/DevTools listening on (ws:\/\/\S+)/); if (m) { clearTimeout(t); resolve(m[1]) } })
  })
  const targets = await bounded(fetch(`http://${new URL(wsUrl).host}/json/list`, { signal: AbortSignal.timeout(10000) }).then(r => r.json()), 10000, 'CDP /json/list')
  ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl)
  await bounded(new Promise((r, j) => { ws.addEventListener('open', r, { once: true }); ws.addEventListener('error', () => j(new Error('CDP ws error')), { once: true }) }), 10000, 'CDP open')
  let seq = 0; const pend = new Map()
  ws.addEventListener('message', ev => {
    const m = JSON.parse(ev.data)
    if (m.id && pend.has(m.id)) { pend.get(m.id)(m); pend.delete(m.id) }
    if (m.method === 'Network.requestWillBeSent') {
      const u = new URL(m.params.request.url)
      if (u.protocol.startsWith('http') && u.host !== `127.0.0.1:${port}`) external.push(u.href)
    }
  })
  const cdp = (method, params = {}) => { const id = ++seq; return bounded(new Promise(r => { pend.set(id, r); ws.send(JSON.stringify({ id, method, params })) }), 15000, `CDP ${method}`).finally(() => pend.delete(id)) }
  await cdp('Network.enable'); await cdp('Runtime.enable')
  await cdp('Page.navigate', { url: `http://127.0.0.1:${port}/smoke.html` })
  const deadline = Date.now() + 120000
  while (Date.now() < deadline) {
    const v = await cdp('Runtime.evaluate', { expression: 'JSON.stringify(window.__smoke || null)', returnByValue: true })
    const s = v.result && v.result.result && v.result.result.value
    if (s && s !== 'null') { result = JSON.parse(s); break }
    await new Promise(r => setTimeout(r, 200))
  }
} finally {
  try { ws?.close() } catch (_) {}
  const td = await teardown({ child: chrome, server, sockets, profileDir: profile })
  console.log(`teardown: ${JSON.stringify(td)}`)
  if (!td.ok) { process.exitCode = 1; throw new Error('foundation smoke teardown failed: ' + JSON.stringify(td)) }
}
assert.ok(result, 'smoke page produced no result')
for (const s of result.steps) console.log(`${s.ok ? 'PASS' : 'FAIL'} ${s.name}${s.ms != null ? ` (${s.ms} ms)` : ''} — ${typeof s.detail === 'object' ? JSON.stringify(s.detail) : s.detail}`)
// Bit-exact comparison to the native oracle (JSON numbers re-parsed as f64).
const oracle = JSON.parse(fs.readFileSync(path.join(FXD, 'oracle.json'), 'utf8'))
const browser = JSON.parse(result.output)
const bits = (x) => { const b = Buffer.alloc(8); b.writeDoubleLE(x); return b.toString('hex') }
let compared = 0
const cmp = (a, b, at) => {
  if (Array.isArray(a)) { assert.equal(a.length, b.length, at); a.forEach((v, i) => cmp(v, b[i], `${at}[${i}]`)); return }
  if (a && typeof a === 'object') { assert.deepEqual(Object.keys(a).sort(), Object.keys(b).sort(), at); for (const k of Object.keys(a)) cmp(a[k], b[k], `${at}.${k}`); return }
  if (typeof a === 'number') { assert.equal(bits(a), bits(b), at); compared++; return }
  assert.equal(a, b, at)
}
cmp(oracle, browser, 'output')
console.log(`oracle comparison: ${compared} numeric values bit-identical, ${browser.times.length} bars`)
assert.deepEqual(external, [], 'no non-loopback network requests')
const failed = result.steps.filter(s => !s.ok)
assert.equal(failed.length, 0, `failed steps: ${failed.map(s => s.name).join(', ')}`)
assert.ok(result.steps.length >= 50, 'expected full step coverage')
console.log(`browser smoke: ${result.steps.length} steps passed`)
