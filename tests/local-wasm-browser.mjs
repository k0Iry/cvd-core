#!/usr/bin/env node
// Real-Chromium gate for the browser-LOCAL WASM indicator path (lane B).
//
//   CVD_WASM_DIR=<wasm32 wasm-release dir> node tests/local-wasm-browser.mjs
//
// Env: CVD_WASM_DIR must contain cvd_browser_host.wasm, sample_indicator.wasm,
// hostile_wasm_plugin.wasm and the PUBLIC cvd_indicator_cvd.wasm /
// cvd_indicator_flow.wasm (lane C). CVD_LOCAL_OUT: screenshots/results dir.
// CVD_CHROMIUM: default /usr/bin/chromium.
//
// Loopback only: synthetic valid canonical bars on /meta, /history, /ws plus
// the repo static/ app. The server ALSO sends bogus legacy `indicators`
// blocks / typed indicator frames (value 999999) which must never be shown.
// Modules are imported through the real <input type=file> with
// DOM.setFileInputFiles; enable/disable/remove use real CDP mouse/keyboard
// input. Outputs are computed by the real WASM modules in Workers and compared
// against values derived here from the same bars (CVD projection, sample SMA).
// Every check asserts; any failure exits non-zero. No private artifacts.
import fs from 'node:fs'
import path from 'node:path'
import http from 'node:http'
import crypto from 'node:crypto'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { teardown } from './harness-teardown.mjs'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const SRC = path.resolve(path.join(HERE, '../static'))
const WASM = process.env.CVD_WASM_DIR
if (!WASM) throw new Error('CVD_WASM_DIR is required')
const OUT = path.resolve(process.env.CVD_LOCAL_OUT || 'artifacts/local-wasm')
fs.mkdirSync(OUT, { recursive: true })
const mod = name => { const f = path.join(WASM, name); if (!fs.existsSync(f)) throw new Error('missing ' + f); return f }
const FILES = { cvd: mod('cvd_indicator_cvd.wasm'), flow: mod('cvd_indicator_flow.wasm'), sample: mod('sample_indicator.wasm'), hostile: mod('hostile_wasm_plugin.wasm'), stateful: mod('stateful_indicator.wasm'), host: mod('cvd_browser_host.wasm'),
  prefix1: path.join(process.env.CVD_PREFIX_DIR || WASM, 'prefix_v1.wasm'), prefix2: path.join(process.env.CVD_PREFIX_DIR || WASM, 'prefix_v2.wasm') }
// Owned invalid inputs (no secrets): a text file renamed .wasm, a non-.wasm name.
const BAD = path.join(OUT, 'not-a-module.wasm'); fs.writeFileSync(BAD, 'hello, not wasm')
const NOTWASM = path.join(OUT, 'plugin.js'); fs.writeFileSync(NOTWASM, 'alert(1)')

// ---------------------------------------------------------------- data
const STEP = 60, T0 = 1_700_000_040, TOTAL = 4000
function makeBar(i) {
  const price = 30000 + 200 * Math.sin(i / 300) + 50 * Math.sin(i / 37)
  const prev = 30000 + 200 * Math.sin((i - 1) / 300) + 50 * Math.sin((i - 1) / 37)
  const cvd = i % 97 === 0 ? 0 : 1e6 * Math.sin(i / 500) + i * 10 // true zeros included
  const buy = 5000 + 1000 * Math.sin(i / 7), sell = 5000
  return { time: T0 + i * STEP + (i >= 2000 ? 600 : 0), // a 10-bar time gap in the middle
    price_close: price, po: prev, ph: Math.max(price, prev) + 5, pl: Math.min(price, prev) - 5,
    apc: price + 10, apo: prev + 10, aph: Math.max(price, prev) + 15, apl: Math.min(price, prev) + 5,
    cvd_usdt: cvd, delta_usdt: buy - sell, buy_volume_usdt: buy, sell_volume_usdt: sell,
    ac: 2 * cvd, ad: 2 * (buy - sell), agb: 2 * buy, ags: 2 * sell,
    bvb: buy / price, svb: sell / price, abvb: 2 * buy / price, asvb: 2 * sell / price, trades: 10 }
}
let dataset = Array.from({ length: TOTAL }, (_, i) => makeBar(i))
const control = { history: [], other: [], wsUpgrades: 0, uploads: [] }
const BOGUS = 999999
function historyPage(params) {
  const limit = Math.min(control.pageCap || 5000, Number(params.get('limit') || 1600))
  const before = params.has('before') ? Number(params.get('before')) : null
  const after = params.has('after') ? Number(params.get('after')) : null
  let rows
  if (after != null) { const s = dataset.findIndex(b => b.time > after); rows = s < 0 ? [] : dataset.slice(s, s + limit) }
  else { let end = dataset.length; if (before != null) { end = dataset.findIndex(b => b.time >= before); if (end < 0) end = dataset.length } rows = dataset.slice(Math.max(0, end - limit), end) }
  // Producer semantics (load_history_page + overlay_live_validity): `limit`
  // counts SCANNED rows; invalid rows are filtered and reported within the
  // truthful scan coverage; live revision marks are reported with their rev.
  const scanned = rows
  const inv = control.invalid ?? new Set()
  rows = scanned.filter(b => !inv.has(b.time))
  const hasMoreAfter = after != null && dataset.some(b => b.time > (scanned.at(-1)?.time ?? Infinity))
  const hasMoreBefore = scanned.length > 0 && (control.hideStart || scanned[0].time > dataset[0].time)
  const lastInvalid = scanned.filter(b => inv.has(b.time)).map(b => b.time)
  let coverageFrom, coverageTo
  if (after != null) { coverageFrom = after + 1; coverageTo = hasMoreAfter ? (rows.at(-1)?.time ?? lastInvalid.at(-1) ?? null) : null }
  else { coverageFrom = hasMoreBefore ? (rows[0]?.time ?? lastInvalid[0] ?? null) : null; coverageTo = before != null ? before - 1 : null }
  const inCov = t => (coverageFrom == null || t >= coverageFrom) && (coverageTo == null || t <= coverageTo)
  const invalidTimes = lastInvalid.filter(inCov)
  const validity = [...(control.marks ?? new Map())].filter(([t]) => inCov(t)).map(([time, m]) => ({ time, rev: m.rev, valid: m.valid }))
  const times = rows.map(b => b.time)
  return { series: 'spot-v1', bars: rows, invalid_times: invalidTimes, coverage_from: coverageFrom, coverage_to: coverageTo, has_more_after: hasMoreAfter, epoch: control.epoch ?? 7, validity, oldest_time: times[0] ?? null, newest_time: times.at(-1) ?? null, realtime_enabled: true, has_more_before: hasMoreBefore,
    indicator_api_version: 1, indicators: { cvd: { version: '9', modes: { source: { status: 'ok', output: { times, series: { cvd: { type: 'line', values: times.map(() => BOGUS) } } } } } } } }
}
const sockets = new Set()
function wsSend(socket, text) {
  const payload = Buffer.from(text); let header
  if (payload.length < 126) header = Buffer.from([0x81, payload.length])
  else if (payload.length < 65536) { header = Buffer.alloc(4); header[0] = 0x81; header[1] = 126; header.writeUInt16BE(payload.length, 2) }
  else { header = Buffer.alloc(10); header[0] = 0x81; header[1] = 127; header.writeBigUInt64BE(BigInt(payload.length), 2) }
  socket.write(Buffer.concat([header, payload]))
}
let baseRev = 0
function pushLive(bar) {
  const i = dataset.findIndex(b => b.time === bar.time)
  if (i >= 0) dataset[i] = bar; else dataset.push(bar)
  const rev = ++baseRev
  for (const s of sockets) {
    wsSend(s, JSON.stringify({ series: 'spot-v1', action: 'append', rev, bar, epoch: control.epoch ?? 7 }))
    wsSend(s, JSON.stringify({ type: 'indicator', api_version: 1, id: 'cvd', version: '9', mode: 'source', generation: 1, session: 0, seq: rev, as_of_rev: rev, output: { times: [bar.time], series: { cvd: { type: 'line', values: [BOGUS] } } } }))
  }
}
// Raw producer frame (validity suite): sent verbatim on every open socket.
function pushRaw(frame) { const text = JSON.stringify(frame); for (const s of sockets) wsSend(s, text) }
function nextRev() { return ++baseRev }
function dropSockets() { for (const s of sockets) s.destroy(); sockets.clear() }
function releaseHeld() { const h = control.held ?? []; control.held = []; for (const x of h) x.send(); return h.map(x => x.search) }
const BRIDGE = `\nwindow.__cvd = { page, renderers: () => renderers, chart, loadMoreBefore, values: () => localValues, compute: () => localCompute, host: () => localHost, records: () => localRecords, notice: () => importNotice, revision: () => dataRevision, historyRevision: () => historyRevision, noteDataChanged, olderPages: () => localOlderPages, coverage: () => localCoverage, registryFromRecords, renderIndicatorMenu, pluginState: () => pluginStateEl && pluginStateEl.title, fmtReadout: (key, v) => { const o = registry.panes.flatMap(p => p.outputs).find(o => o.key === key); return o ? readoutText(o, v) : null }, setMode: m => { aggToggle.checked = m === 'aggregate'; aggToggle.onchange() }, db: () => localDb, toggleReceipts: () => (typeof toggleReceipts === 'undefined' ? null : toggleReceipts), poison: () => [...invalidPoison.keys()], epoch: () => producerEpoch, fetchSkip: (l, b, a) => fetchHistorySkippingInvalid(l, b, a), validAuth: t => validAuth.get(t) ?? null, refresh: (ids, sc, touched) => refreshPluginOutputs(ids, sc, touched), window: () => renderedWindow.bars }\n`
const appSource = fs.readFileSync(process.env.CVD_APP_OVERRIDE || path.join(SRC, 'js/app.js'), 'utf8').replace(/\ninit\(\)\s*$/, BRIDGE + '\ninit()\n')
const html = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8').replace(/<link[^>]*fonts\.(googleapis|gstatic)[^>]*>/g, '')
const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1')
  if (req.method !== 'GET' && req.method !== 'HEAD') control.uploads.push(`${req.method} ${url.pathname}`)
  if (url.pathname === '/meta') { res.writeHead(200, { 'Content-Type': 'application/json' }); return res.end(JSON.stringify({ contract_series: 'contracts-drb-v1', exchanges: [{ exchange: 'binance', markets: ['spot'], symbols: ['BTC'] }] })) }
  if (url.pathname === '/history') {
    control.history.push(url.search)
    // Validity suite: scripted verbatim bodies (real producer pages) or a
    // request-time snapshot HELD to model a delayed/overlapping observation.
    const body = control.script?.length ? control.script.shift() : historyPage(url.searchParams)
    const send = () => { res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(body)) }
    if (control.holdNext) { control.holdNext--; (control.held ??= []).push({ search: url.search, send }); return }
    return send()
  }
  let data, mime
  if (url.pathname === '/') { data = html; mime = 'text/html' }
  else if (url.pathname === '/js/app.js') { data = appSource; mime = 'text/javascript' }
  else if (url.pathname === '/wasm-host/cvd_browser_host.wasm') { if (control.hostMissing) { res.writeHead(404); return res.end() } data = fs.readFileSync(FILES.host); mime = 'application/wasm' }
  else {
    const file = path.resolve(SRC, '.' + decodeURIComponent(url.pathname))
    if (!file.startsWith(SRC + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) { control.other.push(url.pathname); res.writeHead(404); return res.end() }
    data = fs.readFileSync(file)
    mime = file.endsWith('.css') ? 'text/css' : /\.m?js$/.test(file) ? 'text/javascript' : file.endsWith('.wasm') ? 'application/wasm' : 'application/octet-stream'
  }
  res.writeHead(200, { 'Content-Type': mime, 'Cache-Control': 'no-store' }); res.end(data)
})
server.on('upgrade', (req, socket) => {
  const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64')
  control.wsUpgrades++
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
  sockets.add(socket); socket.on('data', () => {}); socket.on('close', () => sockets.delete(socket)); socket.on('error', () => sockets.delete(socket))
})
await new Promise(r => server.listen(0, '127.0.0.1', r))
const PORT = server.address().port

// ---------------------------------------------------------------- CDP
const profileDir = fs.mkdtempSync(path.join(process.env.TMPDIR || '/tmp', 'cvd-local-wasm-'))
const chrome = spawn(process.env.CVD_CHROMIUM || '/usr/bin/chromium', ['--headless=new', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-first-run',
  '--disable-background-networking', '--window-size=1440,900', '--remote-debugging-port=0', '--user-data-dir=' + profileDir, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] })
const sleep = ms => new Promise(r => setTimeout(r, ms))
let ws, seq = 0
const pending = new Map(), external = [], consoleErrors = []
const checks = []
function check(name, ok, detail = '') { checks.push({ name, ok: !!ok, detail: ok ? '' : String(detail).slice(0, 400) }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ' :: ' + String(detail).slice(0, 400)}`) }
function call(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++seq; const t = setTimeout(() => { pending.delete(id); reject(new Error(method + ' timeout')) }, 60000)
    pending.set(id, m => { clearTimeout(t); m.error ? reject(new Error(method + ': ' + m.error.message)) : resolve(m.result) }); ws.send(JSON.stringify({ id, method, params }))
  })
}
async function evaluate(expression) {
  const r = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 1500))
  return r.result.value
}
async function waitFor(expression, timeout = 20000) {
  const deadline = Date.now() + timeout
  while (!(await evaluate(expression).catch(() => false))) { if (Date.now() > deadline) throw new Error('timeout waiting for ' + expression); await sleep(40) }
}
async function center(selector) {
  const r = await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return null; el.scrollIntoView({ block: 'nearest' }); const b = el.getBoundingClientRect(); return { x: b.left + b.width / 2, y: b.top + b.height / 2 } })()`)
  if (!r) throw new Error('no element ' + selector)
  return r
}
async function click(selector) {
  const { x, y } = await center(selector)
  await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y })
  await call('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1 })
  await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1 })
}
async function key(k, code = k, keyCode = 0) {
  await call('Input.dispatchKeyEvent', { type: 'keyDown', key: k, code, windowsVirtualKeyCode: keyCode })
  await call('Input.dispatchKeyEvent', { type: 'keyUp', key: k, code, windowsVirtualKeyCode: keyCode })
}
async function openMenu() { if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await click('.plugin-menu-trigger'); await waitFor(`!document.getElementById('plugin-menu').hidden`) }
async function importFile(file) {
  await openMenu()
  // Correlated: wait for the terminal notice of THIS import operation (op type
  // + file name, newer than any notice seen before the action) — a late notice
  // from an earlier queued operation (e.g. removal) cannot satisfy it.
  const before = await evaluate('__cvd.notice().seq')
  const { root } = await call('DOM.getDocument', { depth: -1 })
  const { nodeId } = await call('DOM.querySelector', { nodeId: root.nodeId, selector: '.plugin-import-input' })
  await call('DOM.setFileInputFiles', { nodeId, files: [file] })
  const base = JSON.stringify(path.basename(file))
  await waitFor(`(n => n.seq > ${before} && n.kind !== 'busy' && n.op && n.op.type === 'import' && n.op.file === ${base})(__cvd.notice())`)
  return evaluate('__cvd.notice()')
}
const rowSel = id => `.plugin-menu-row[data-plugin="${id}"]`
async function toggle(id) { await openMenu(); await click(`${rowSel(id)} input[type=checkbox]`) }
async function shot(name) { const { data } = await call('Page.captureScreenshot', { format: 'png' }); fs.writeFileSync(path.join(OUT, name + '.png'), Buffer.from(data, 'base64')) }
const valuesOf = (key, mode = 'source') => evaluate(`Object.fromEntries([...(__cvd.values().get(${JSON.stringify(key + '|' + mode)}) ?? new Map())])`)
const settled = `__cvd.compute() && __cvd.compute().idle()`

let exitCode = 1
try {
  const wsUrl = await new Promise((resolve, reject) => { let buf = ''; const t = setTimeout(() => reject(new Error('chromium did not start')), 20000)
    chrome.stderr.on('data', d => { buf += d; const m = buf.match(/DevTools listening on (ws:\/\/\S+)/); if (m) { clearTimeout(t); resolve(m[1]) } }) })
  const targets = await (await fetch(`http://${new URL(wsUrl).host}/json/list`)).json()
  ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl)
  await new Promise(r => ws.addEventListener('open', r, { once: true }))
  ws.addEventListener('message', ev => {
    const m = JSON.parse(ev.data)
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) }
    if (m.method === 'Network.requestWillBeSent') {
      const u = new URL(m.params.request.url)
      if (/^(https?|wss?):$/.test(u.protocol) && u.host !== `127.0.0.1:${PORT}`) external.push(u.href)
      if (m.params.request.method !== 'GET' || m.params.request.hasPostData) control.uploads.push(`${m.params.request.method} ${u.pathname}`)
    }
    if (m.method === 'Runtime.exceptionThrown') consoleErrors.push(JSON.stringify(m.params.exceptionDetails).slice(0, 300))
  })
  await call('Network.enable'); await call('Runtime.enable'); await call('DOM.enable'); await call('Page.enable')
  await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
  const open = async () => { await call('Page.navigate', { url: `http://127.0.0.1:${PORT}/` }); await waitFor(`!!globalThis.__cvd && __cvd.page.bars.length > 0 && !!__cvd.host()`) }
  await open()
  if (process.env.CVD_LOCAL_SUITE === 'p2') {
    const { runP2Cases } = await import('./local-p2-cases.inc.mjs')
    await runP2Cases({ evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds: () => dataset, pushLive, STEP, FILES, makeBar, call, click, key, openMenu, rowSel, shot, center,
      setDataset: d => { dataset = d }, reopen: open, external, consoleErrors, OUT })
  } else if (process.env.CVD_LOCAL_SUITE === 'toggle') {
    const { runToggleCases } = await import('./local-toggle-cases.inc.mjs')
    await runToggleCases({ evaluate, waitFor, check, importFile, toggle, sleep, FILES, call, click, openMenu, rowSel, settled, reopen: open })
  } else if (process.env.CVD_LOCAL_SUITE === 'validity') {
    const { runValidityCases } = await import('./local-validity-cases.inc.mjs')
    const frames = process.env.CVD_PRODUCER_VALIDITY_FRAMES ? JSON.parse(fs.readFileSync(process.env.CVD_PRODUCER_VALIDITY_FRAMES, 'utf8')) : null
    await runValidityCases({ evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds: () => dataset, pushLive, pushRaw, nextRev, FILES,
      producerFrames: frames, reopen: open, consoleErrors, external, dropSockets, releaseHeld,
      producerPages: process.env.CVD_PRODUCER_VALIDITY_PAGES ? JSON.parse(fs.readFileSync(process.env.CVD_PRODUCER_VALIDITY_PAGES, 'utf8')) : null })
  } else if (process.env.CVD_LOCAL_SUITE === 'external') {
    // Generic out-of-tree suite (e.g. an operator's PRIVATE whole-app smoke kept
    // outside this repository). Same app/UI/host path; no plugin special case.
    const { runExternalCases } = await import(path.resolve(process.env.CVD_EXTERNAL_SUITE))
    await runExternalCases({ evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds: () => dataset, pushLive, STEP, FILES, makeBar,
      call, click, openMenu, rowSel, shot, reopen: open, external, consoleErrors, OUT, dropSockets })
  } else if (process.env.CVD_LOCAL_SUITE === 'data') {
    const { runDataCases } = await import('./local-data-cases.inc.mjs')
    const pf = process.env.CVD_PRODUCER_FIXTURE ? JSON.parse(fs.readFileSync(process.env.CVD_PRODUCER_FIXTURE, 'utf8')) : null
    await runDataCases({ evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds: () => dataset, pushLive, STEP, FILES, makeBar,
      producerFixture: pf, setDataset: d => { dataset = d }, reopen: open })
  } else {

  // 1. Empty registry: only Import; no rows; no server discovery request.
  await openMenu()
  check('empty_menu_has_import_and_no_rows', await evaluate(`!!document.querySelector('.plugin-import-btn') && document.querySelectorAll('.plugin-menu-row').length === 0 && !!document.querySelector('[data-empty]')`))
  await shot('01-empty-menu')

  // 2. Invalid imports: visible fixed messages, no row, host left clean.
  let n = await importFile(BAD); check('invalid_bytes_rejected_with_message', n.kind === 'error' && /not a WebAssembly module|malformed/.test(n.text), JSON.stringify(n))
  n = await importFile(NOTWASM); check('non_wasm_name_rejected', n.kind === 'error' && /only \.wasm/.test(n.text), JSON.stringify(n))
  const hostileDir = process.env.CVD_HOSTILE_DIR
  if (hostileDir) {
    for (const [name, code] of JSON.parse(fs.readFileSync(path.join(hostileDir, 'index.json'), 'utf8')).slice(0, 4)) {
      n = await importFile(path.join(hostileDir, name + '.wasm'))
      check(`hostile_${name}_rejected_${code}`, n.kind === 'error', JSON.stringify(n))
    }
  }
  check('no_rows_after_invalid', await evaluate(`document.querySelectorAll('.plugin-menu-row').length === 0`))
  check('host_clean_after_invalid', await evaluate(`(() => { const s = __cvd.host().stats(); return s.running === 0 && s.waiters === 0 && s.timers === 0 })()`), await evaluate('JSON.stringify(__cvd.host().stats())'))

  // 3. Real public CVD import (disabled by default: no Worker, no CPU).
  n = await importFile(FILES.cvd); check('cvd_imported', n.kind === 'ok' && /Imported/.test(n.text), JSON.stringify(n))
  check('cvd_row_registered_unchecked', await evaluate(`!!document.querySelector('${rowSel('cvd')}') && !document.querySelector('${rowSel('cvd')} input').checked`))
  check('disabled_import_runs_nothing', await evaluate(`__cvd.host().stats().running === 0`))
  n = await importFile(FILES.cvd); check('duplicate_same_bytes_noop', n.kind === 'info' && /already imported/.test(n.text), JSON.stringify(n))

  // 4. Enable via real click -> computes over the shared cache; exact values.
  const historyBefore = control.history.length
  await toggle('cvd')
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`)
  const bars = await evaluate('__cvd.page.bars.map(b => [b.time, b.cvd_usdt])')
  const cvd = await valuesOf('cvd/cvd')
  let exact = bars.length > 0, bogus = false
  for (const [t, v] of bars) { if (!Object.is(cvd[t], v)) { exact = false } if (cvd[t] === BOGUS) bogus = true }
  check('cvd_equals_authoritative_cumulative_bitwise', exact, `bars=${bars.length}`)
  check('true_zero_cvd_preserved', bars.some(([t, v]) => v === 0 && Object.is(cvd[t], 0)))
  check('server_indicator_outputs_never_used', !bogus && !Object.values(cvd).includes(BOGUS))
  const chartVals = await evaluate(`__cvd.renderers().get('cvd/cvd').seriesHandle().data().filter(p => p.value !== undefined).map(p => [p.time, p.value])`)
  const byTime = new Map(bars)
  check('lwc_series_shows_wasm_values', chartVals.length > 0 && chartVals.every(([t, v]) => Object.is(byTime.get(t), v)), `${chartVals.length}`)
  const summary = await evaluate(`[...document.querySelectorAll('[data-pane-summary]')].map(e => e.textContent).join(' | ')`)
  check('readout_shows_cvd', /CVD/.test(summary) && !/unavailable/.test(summary), summary)
  check('enable_did_not_spawn_feed', control.history.length - historyBefore <= 1 && control.wsUpgrades <= 2, `history+${control.history.length - historyBefore} ws=${control.wsUpgrades}`)

  // 5. Flow + sample (params + warm-up) share the same cache.
  await importFile(FILES.flow); await toggle(await evaluate(`[...__cvd.records().keys()].find(k => k !== 'cvd')`))
  n = await importFile(FILES.sample); check('sample_imported', n.kind === 'ok', JSON.stringify(n))
  await toggle('sample.channel')
  await waitFor(`(__cvd.values().get('sample.channel/sma|source')?.size ?? 0) > 0 && ${settled}`, 30000)
  const len = await evaluate(`__cvd.records().get('sample.channel').params.length`)
  const closes = await evaluate('__cvd.page.bars.map(b => [b.time, b.price_close])')
  const ctx = await evaluate(`__cvd.records().get('sample.channel').descriptor.context.bars`)
  const sma = await valuesOf('sample.channel/sma')
  let smaOk = true, checked = 0
  // Requested range = the displayed page basis (warm-up pages are context only).
  const smaFrom = await evaluate(`__cvd.compute().runtime('sample.channel').lastPlan.range_from_time`)
  for (let i = Math.max(ctx, closes.findIndex(([t]) => t >= smaFrom)); i < closes.length; i++) {
    const expected = i + 1 < len ? null : closes.slice(i + 1 - len, i + 1).reduce((a, [, c]) => a + c, 0) / len
    if (!Object.is(sma[closes[i][0]] ?? null, expected)) { smaOk = false; break }
    checked++
  }
  check('sample_sma_exact_after_warmup', smaOk && checked > 0, `len=${len} ctx=${ctx} checked=${checked}`)
  check('warmup_bars_have_no_output', closes.slice(0, ctx).every(([t]) => !(t in sma)))
  // Param change through the form.
  await openMenu(); await click(`${rowSel('sample.channel')} [data-action=params]`)
  await evaluate(`(() => { const i = document.querySelector('${rowSel('sample.channel')} [data-param=length]'); i.value = '5' })()`)
  await click(`${rowSel('sample.channel')} [data-action=apply]`)
  await waitFor(`__cvd.records().get('sample.channel').params.length === 5 && (__cvd.values().get('sample.channel/sma|source')?.size ?? 0) > 0 && ${settled}`, 30000)
  const sma5 = await valuesOf('sample.channel/sma'); const lastI = closes.length - 1
  check('param_change_recomputes', Object.is(sma5[closes[lastI][0]], closes.slice(lastI - 4).reduce((a, [, c]) => a + c, 0) / 5))
  await evaluate(`(() => { const i = document.querySelector('${rowSel('sample.channel')} [data-param=length]'); i.value = '0' })()`)
  await click(`${rowSel('sample.channel')} [data-action=apply]`)
  check('invalid_param_rejected_visibly', /length must be/.test(await evaluate(`document.querySelector('${rowSel('sample.channel')} .plugin-param-error').textContent`)))
  await shot('02-enabled-normal')

  // 6. Fault isolation: hostile fixture (invalid output) faults alone.
  n = await importFile(FILES.hostile); check('hostile_fixture_imported', n.kind === 'ok', JSON.stringify(n))
  await toggle('hostile.fixture')
  await waitFor(`!!__cvd.compute().fault('hostile.fixture') && ${settled}`)
  check('fault_status_visible', /invalid_output|fault/.test(await evaluate(`document.querySelector('${rowSel('hostile.fixture')} .plugin-menu-meta').textContent`)))
  check('fault_no_auto_restart', !(await evaluate(`__cvd.compute().running().includes('hostile.fixture')`)))
  pushLive({ ...makeBar(TOTAL), time: dataset.at(-1).time + STEP })
  const liveT = dataset.at(-1).time
  await waitFor(`__cvd.values().get('cvd/cvd|source')?.has(${liveT}) && ${settled}`)
  check('others_healthy_after_fault_and_live_append', Object.is((await valuesOf('cvd/cvd'))[liveT], dataset.at(-1).cvd_usdt))
  // Replace forming bar (same time) -> value replaced, never accumulated; legacy frames ignored.
  pushLive({ ...dataset.at(-1), cvd_usdt: 4242.5 })
  await waitFor(`__cvd.values().get('cvd/cvd|source')?.get(${liveT}) === 4242.5 && ${settled}`)
  check('live_replace_applied', true)
  // Caller path: a DIRTY renderer (stale empty contents) receiving a tail
  // result must be filled from the current window, with NO incremental update.
  const dirtyCase = await evaluate(`(() => {
    const r = __cvd.renderers().get('cvd/cvd'); const w = __cvd.window(); let updates = 0
    r.render([]); r.dirty = true; const orig = r.update; r.update = (...a) => { updates++; return orig.apply(r, a) }
    let error = null
    try { __cvd.refresh(new Set([r.output.pluginId]), false, [w[w.length - 2].time]) } catch (e) { error = String(e) } finally { r.update = orig }
    const vals = __cvd.values().get('cvd/cvd|source')
    const want = w.filter(b => typeof vals?.get(b.time) === 'number').map(b => [b.time, vals.get(b.time)])
    const got = r.seriesHandle().data().filter(p => p.value !== undefined).map(p => [p.time, p.value])
    return JSON.stringify({ updates, error, dirty: r.dirty, n: want.length, same: JSON.stringify(got) === JSON.stringify(want) })
  })()`)
  const dc = JSON.parse(dirtyCase)
  check('dirty_renderer_filled_before_incremental', dc.updates === 0 && !dc.error && !dc.dirty && dc.n > 2 && dc.same, dirtyCase)

  // 6b. Actual live semantics at APP level with A's public stateful fixture:
  // History runs on a fresh instance (+1000); ticks run op2/op3 on ONE
  // persistent instance whose call counter becomes visible (+2000, +3000).
  // Quiesce shared warm-up first: a history-basis change (prepend) legitimately
  // supersedes the live overlay with a fresh History, which this case is not about.
  await waitFor(`(!__cvd.records().get('sample.channel')?.enabled || __cvd.compute().runtime('sample.channel')?.coverage === 'ready' || __cvd.page.reachedStart) && !__cvd.page.loadingOlder && ${settled}`, 45000)
  await sleep(300); await waitFor(settled)
  n = await importFile(FILES.stateful); check('stateful_fixture_imported', n.kind === 'ok', JSON.stringify(n))
  await toggle('fixture.stateful')
  await waitFor(`__cvd.compute().ops('fixture.stateful')[2] === 1 && ${settled}`)
  const stT = dataset.at(-1).time
  check('stateful_history_fresh_instance', Object.is((await valuesOf('fixture.stateful/v'))[dataset.at(-2).time], dataset.at(-2).price_close + 1000))
  pushLive({ ...makeBar(TOTAL + 1), time: stT + STEP })
  await waitFor(`__cvd.compute().ops('fixture.stateful')[3] === 1 && ${settled}`)
  check('stateful_live_update_uses_persistent_instance', Object.is((await valuesOf('fixture.stateful/v'))[stT + STEP], dataset.at(-1).price_close + 2000), JSON.stringify({ v: (await valuesOf('fixture.stateful/v'))[stT + STEP], p: dataset.at(-1).price_close, lp: await evaluate(`__cvd.compute().runtime('fixture.stateful').lastPlan`), hr: await evaluate('__cvd.historyRevision()'), n: await evaluate('__cvd.page.bars.length'), ops: await evaluate(`__cvd.compute().ops('fixture.stateful')`) }))
  pushLive({ ...dataset.at(-1), price_close: dataset.at(-1).price_close + 1 })
  await waitFor(`__cvd.compute().ops('fixture.stateful')[3] === 2 && ${settled}`)
  check('stateful_live_replace_op3_seq2', Object.is((await valuesOf('fixture.stateful/v'))[stT + STEP], dataset.at(-1).price_close + 3000))
  check('stateful_no_history_on_ticks', (await evaluate(`__cvd.compute().ops('fixture.stateful')[2]`)) === 1)
  await toggle('fixture.stateful')
  await waitFor(`!__cvd.compute().running().includes('fixture.stateful') && ${settled}`)

  // 7. Disable releases worker; late outputs never paint; row/params retained.
  const runningBefore = await evaluate('__cvd.host().stats().running')
  await toggle('sample.channel')
  check('disable_releases_instance', (await evaluate('__cvd.host().stats().running')) === runningBefore - 1 && !(await evaluate(`__cvd.compute().running().includes('sample.channel')`)))
  check('disable_clears_outputs', !(await evaluate(`__cvd.values().has('sample.channel/sma|source')`)))
  check('disable_retains_row_and_params', await evaluate(`!!document.querySelector('${rowSel('sample.channel')}') && __cvd.records().get('sample.channel').params.length === 5`))
  // Rapid toggles + source change with in-flight computes: final state coherent.
  for (let i = 0; i < 6; i++) await toggle('sample.channel')
  await evaluate(`(() => { const s = document.getElementById('tf'); s.value = '5m'; s.onchange() })()`)
  await evaluate(`(() => { const s = document.getElementById('tf'); s.value = '1m'; s.onchange() })()`)
  await waitFor(`__cvd.page.bars.length > 0 && ${settled}`, 30000)
  await sleep(300); await waitFor(settled)
  const after = await evaluate('__cvd.page.bars.map(b => [b.time, b.cvd_usdt])')
  const cvd2 = await valuesOf('cvd/cvd')
  check('rapid_changes_no_stale_output', after.every(([t, v]) => Object.is(cvd2[t], v)) && !(await evaluate(`__cvd.values().has('sample.channel/sma|source')`)))

  // 8. Aggregate mode uses aggregate fields (ac).
  await click('#toggle-agg'); await waitFor(`(__cvd.values().get('cvd/cvd|aggregate')?.size ?? 0) > 0 && ${settled}`)
  const agg = await valuesOf('cvd/cvd', 'aggregate'); const aggBars = await evaluate('__cvd.page.bars.map(b => [b.time, b.ac])')
  check('aggregate_mode_uses_ac', aggBars.every(([t, v]) => Object.is(agg[t], v)))
  await click('#toggle-agg')

  // 9. Keyboard: Escape closes, Tab enters popup.
  await openMenu(); await key('Escape', 'Escape', 27)
  check('escape_closes_menu', await evaluate(`document.getElementById('plugin-menu').hidden && document.activeElement?.classList.contains('plugin-menu-trigger')`))
  await key(' ', 'Space', 32); await waitFor(`!document.getElementById('plugin-menu').hidden`)
  await key('Tab', 'Tab', 9)
  check('tab_enters_popup', await evaluate(`document.getElementById('plugin-menu').contains(document.activeElement)`))
  await key('Escape', 'Escape', 27)

  // 10. Hidden/resume: no chart writes needed while hidden, values intact on resume.
  await call('Emulation.setFocusEmulationEnabled', { enabled: false })
  await evaluate(`Object.defineProperty(document, 'hidden', { configurable: true, get: () => true }); document.dispatchEvent(new Event('visibilitychange'))`)
  pushLive({ ...makeBar(TOTAL + 1), time: liveT + 2 * STEP })
  await waitFor(`__cvd.values().get('cvd/cvd|source')?.has(${liveT + 2 * STEP}) && ${settled}`)
  await evaluate(`Object.defineProperty(document, 'hidden', { configurable: true, get: () => false }); document.dispatchEvent(new Event('visibilitychange'))`)
  await sleep(300)
  const tail = await evaluate(`__cvd.renderers().get('cvd/cvd').seriesHandle().data().at(-1)`)
  check('hidden_compute_then_resume_paint', tail && tail.time === liveT + 2 * STEP && Object.is(tail.value, dataset.at(-1).cvd_usdt), JSON.stringify(tail))

  // 11. Reload persistence: rows, enable intent, params survive; computes again.
  await open()
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`, 30000)
  check('reload_restores_rows_and_enable', await evaluate(`__cvd.records().size === 5 && __cvd.records().get('cvd').enabled && !__cvd.records().get('sample.channel').enabled && __cvd.records().get('sample.channel').params.length === 5`))

  // 12. Version/replacement safety: same id, different bytes (an extra custom
  // section, which the validator skips) replaces atomically, keeps enable
  // intent, releases the old instance, and values stay exact.
  const orig = fs.readFileSync(FILES.cvd)
  const name = Buffer.from('b-replace-test'), payload = Buffer.from('v2')
  const body = Buffer.concat([Buffer.from([name.length]), name, payload])
  const REPL = path.join(OUT, 'cvd-replaced.wasm'); fs.writeFileSync(REPL, Buffer.concat([orig, Buffer.from([0, body.length]), body]))
  const shaBefore = await evaluate(`__cvd.records().get('cvd').sha256`)
  n = await importFile(REPL); check('replacement_import_replaces', n.kind === 'ok' && /Replaced/.test(n.text), JSON.stringify(n))
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`)
  const bars3 = await evaluate('__cvd.page.bars.map(b => [b.time, b.cvd_usdt])'); const cvd3 = await valuesOf('cvd/cvd')
  check('replacement_keeps_enable_and_exact_values', (await evaluate(`__cvd.records().get('cvd').enabled && __cvd.records().get('cvd').sha256 !== ${JSON.stringify(shaBefore)}`)) && bars3.every(([t, v]) => Object.is(cvd3[t], v)))
  check('replacement_single_instance', (await evaluate(`__cvd.host().stats().running`)) === (await evaluate(`__cvd.compute().running().length`)))
  // Rapid uninstall -> re-import: fresh disabled row with default params.
  await openMenu(); await click(`${rowSel('sample.channel')} [data-action=remove]`)
  n = await importFile(FILES.sample)
  const fresh = await evaluate(`(r => r ? { enabled: r.enabled, len: r.params.length } : null)(__cvd.records().get('sample.channel'))`)
  check('reimport_after_uninstall_is_fresh', n.kind === 'ok' && /Imported/.test(n.text) && fresh !== null && fresh.enabled === false && fresh.len === 20, JSON.stringify({ n, fresh }))
  // Uninstall persists across reload:
  await openMenu(); await click(`${rowSel('hostile.fixture')} [data-action=remove]`)
  await waitFor(`!__cvd.records().has('hostile.fixture') && /Removed/.test(__cvd.notice().text)`)
  await open()
  check('uninstall_persists_across_reload', await evaluate(`!__cvd.records().has('hostile.fixture') && !document.querySelector('${rowSel('hostile.fixture')}')`))

  // 13. Screenshots: normal / narrow / short / light / dark with menu open.
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`, 30000)
  await openMenu(); await shot('03-normal-1440x900-menu')
  for (const [name, w, h] of [['04-narrow-390x844', 390, 844], ['05-short-1280x480', 1280, 480]]) {
    await call('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: w < 500 })
    await sleep(300); await openMenu(); await sleep(100)
    check(`${name}_menu_within_viewport`, await evaluate(`(() => { const b = document.getElementById('plugin-menu').getBoundingClientRect(); return b.left >= 0 && b.right <= innerWidth + 1 && b.top >= 0 && b.bottom <= innerHeight + 1 })()`))
    await shot(name)
  }
  await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
  for (const scheme of ['light', 'dark']) {
    await call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme }] })
    await sleep(300); await openMenu(); await shot(`06-${scheme}`)
  }

  check('no_upload_requests', control.uploads.length === 0, control.uploads.join(','))
  check('no_external_network', external.length === 0, external.join(','))
  check('no_page_exceptions', consoleErrors.length === 0, consoleErrors.join('\n'))
  }
  exitCode = checks.every(c => c.ok) ? 0 : 1
} catch (error) {
  check('harness_completed', false, error.stack)
} finally {
  const results = { checks, history: control.history.length, wsUpgrades: control.wsUpgrades, external, uploads: control.uploads }
  fs.writeFileSync(path.join(OUT, 'results.json'), JSON.stringify(results, null, 2))
  console.log(`${checks.filter(c => c.ok).length}/${checks.length} checks passed`)
  // Cleanup is recorded separately from functional checks; it never changes
  // the check inventory and a cleanup failure cannot mask a functional one.
  let cleanup
  try {
    try { ws?.close() } catch (_) {}
    cleanup = await teardown({ child: chrome, server, sockets, profileDir })
  } catch (error) { cleanup = { ok: false, error: error.stack } }
  fs.writeFileSync(path.join(OUT, 'results.json'), JSON.stringify({ ...results, cleanup }, null, 2))
  if (cleanup.ok) console.log(`CLEANUP OK browser=${cleanup.browser.how} helpers=${cleanup.browser.helpers} server=closed profile_removed attempts=${cleanup.profile.attempts}`)
  else { console.log(`CLEANUP FAIL ${JSON.stringify(cleanup)}`); if (exitCode === 0) exitCode = 2 }
  process.exit(exitCode)
}
