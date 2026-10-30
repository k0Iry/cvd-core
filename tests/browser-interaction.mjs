#!/usr/bin/env node
// Actual-input interaction harness for the default chart (static/).
//
//   node tests/browser-interaction.mjs
//
// Environment:
//   CVD_STATIC_ROOT   static/ directory to serve (default: this repo's static/)
//   CVD_INTERACTION_OUT  output directory for results JSON (default: ./artifacts/interaction-results)
//   CVD_CHROMIUM      Chromium binary (default /usr/bin/chromium)
//   CVD_REPS          repetitions per scenario (default 5)
//   CVD_PROFILE       synthetic data profile: fragmented | continuous (default fragmented)
//   CVD_SCENARIOS     comma-separated subset (default: all)
//
//   CVD_LOCAL_WASM        comma-separated public plugin .wasm paths (required)
//
// Everything is loopback: a synthetic /meta, /history and /ws fixture plus the
// served static files and the pinned vendored Lightweight Charts. Indicator
// results are REAL: the browser computes them from the imported public .wasm;
// the harness's independent NUMERICAL ORACLE runs the same .wasm through the
// shipped trusted plugin-worker.js + cvd_browser_host.wasm in Node
// (tests/wasm-oracle-client.mjs; successor of the retired native runner).
// Oracle envelopes are never displayed. No production
// host, database or market feed is contacted. Input is real CDP pointer/wheel
// input (trusted DOM events), not programmatic range setters; setup-only
// positioning uses the chart API and is excluded from measurements.
//
// RAF intervals are a main-thread frame-cadence PROXY in headless Chromium.
// They are not input-to-photon latency, FPS certification or Safari evidence.
// A failed check makes the process exit non-zero; scenarios are never skipped.
import fs from 'node:fs'
import path from 'node:path'
import http from 'node:http'
import crypto from 'node:crypto'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { WasmOracleClient, inputBar } from './wasm-oracle-client.mjs'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const SRC = path.resolve(process.env.CVD_STATIC_ROOT || path.join(HERE, '../static'))
const OUT = path.resolve(process.env.CVD_INTERACTION_OUT || 'artifacts/interaction-results')
const CHROMIUM = process.env.CVD_CHROMIUM || '/usr/bin/chromium'
const REPS = Number(process.env.CVD_REPS || 5)
if (!Number.isInteger(REPS) || REPS < 1) throw new Error(`invalid CVD_REPS: ${process.env.CVD_REPS}`)
const PROFILE = process.env.CVD_PROFILE || 'fragmented'
if (!['fragmented', 'continuous'].includes(PROFILE)) throw new Error(`unknown CVD_PROFILE: ${PROFILE}`)
const ONLY = process.env.CVD_SCENARIOS ? new Set(process.env.CVD_SCENARIOS.split(',')) : null
const VIEW = { width: 1440, height: 900, deviceScaleFactor: 2 }
fs.mkdirSync(OUT, { recursive: true })

// ---------------------------------------------------------------- fixture data
const STEP = 60
const T0 = 1_700_000_040
const TOTAL = 40_000
function makeBar(i) {
    const fragmented = PROFILE === 'fragmented'
    const price = 30000 + 200 * Math.sin(i / 300) + 50 * Math.sin(i / 37)
    const prev = 30000 + 200 * Math.sin((i - 1) / 300) + 50 * Math.sin((i - 1) / 37)
    // Fragmented: Buy/Sell does not reconcile at i%80 === 0, so the real Flow
    // library emits gaps there (exercises the gap path of a real plugin).
    const flowGap = fragmented && i % 80 === 0
    const cvd = 1e6 * Math.sin(i / 500) + i * 10
    const buy = 5000 + 1000 * Math.sin(i / 7), sell = 5000
    return {
        time: T0 + i * STEP, price_close: price, po: prev, ph: Math.max(price, prev) + 5, pl: Math.min(price, prev) - 5,
        apc: price + 10, apo: prev + 10, aph: Math.max(price, prev) + 15, apl: Math.min(price, prev) + 5,
        cvd_usdt: cvd, delta_usdt: buy - sell + (flowGap ? 17 : 0), buy_volume_usdt: buy, sell_volume_usdt: sell, // delta = buy - sell
        ac: 2 * cvd, ad: 2 * (buy - sell), agb: 2 * buy, ags: 2 * sell, // aggregate tuple, same identity
        bvb: buy / price, svb: sell / price, abvb: 2 * buy / price, asvb: 2 * sell / price,
        trades: 10,
    }
}
let dataset = []
function resetDataset() { dataset = Array.from({ length: TOTAL }, (_, i) => makeBar(i)) }
// assetFault: test-only fault on ONE served static asset (indicator_menu reload
// controls): { path, delayMs } delays it, { path, status } fails it.
const control = { beforeDelayMs: 0, requests: [], discovery: null, wsUpgrades: 0, assetFault: null, livePushes: 0 }

// ------------------------------------------------------------ real indicators
const ORACLE_WASM = (process.env.CVD_LOCAL_WASM || '').split(',').filter(Boolean)
if (!ORACLE_WASM.length) throw new Error('CVD_LOCAL_WASM is required (comma-separated public plugin .wasm paths)')
const runners = []
for (const lib of ORACLE_WASM) runners.push(await WasmOracleClient.spawn(lib))
const discovery = { api_version: 1, indicators: runners.map(r => ({ id: r.descriptor.id, status: 'ready', modes: ['source', 'aggregate'],
    version: r.descriptor.version, descriptor: JSON.parse(r.descriptorJson), error: null, restarts: 0 })) }
// Coherent scope identity: the scope of the most recent request (exchange /
// market / symbol / tf), never a hard-coded tf.
const currentScope = { exchange: 'binance', market: 'spot', symbol: 'BTC', tf: '1m' }
function noteScope(params) {
    for (const k of ['exchange', 'market', 'symbol', 'tf']) if (params.get(k)) currentScope[k] = params.get(k)
}
const scopeOf = (mode, scope = currentScope) => ({ ...scope, mode })
// Base-bar revisions per time (host shard rev semantics: increments on each
// base frame for this source); indicator frames carry as_of_rev = rev of the
// exact snapshot they were computed from.
let baseRev = 0
// Host behaviour: warm-up rows (declared context) are used, then trimmed so
// output.times == page bars exactly. The bar window is frozen (copied) and
// the scope fixed BEFORE deriving outputs from that exact snapshot.
async function indicatorsFor(start, end, snapshot = dataset.slice(), scope = { ...currentScope }) {
    const out = {}
    for (const r of runners) {
        const ctx = Math.min(start, r.descriptor.context?.bars ?? 0)
        const bars = snapshot.slice(start - ctx, end).map(inputBar)
        const modes = {}
        for (const mode of ['source', 'aggregate']) {
            const res = await r.history(scopeOf(mode, scope), ctx, bars)
            modes[mode] = res.ok ? { status: 'ok', output: res.output } : { status: 'unavailable', error: { code: res.error.code, message: res.error.message } }
        }
        out[r.descriptor.id] = { version: r.descriptor.version, modes }
    }
    return out
}
const liveIndicator = { generation: Date.now(), session: 1, seq: new Map() }

async function historyPage(params) {
    const limit = Math.min(5000, Number(params.get('limit') || 1600))
    const before = params.has('before') ? Number(params.get('before')) : null
    const after = params.has('after') ? Number(params.get('after')) : null
    let rows, moreBefore, moreAfter
    if (after != null) {
        const start = dataset.findIndex(b => b.time > after)
        rows = start < 0 ? [] : dataset.slice(start, start + limit)
        moreBefore = start > 0
        moreAfter = start >= 0 && start + limit < dataset.length
    } else {
        let end = dataset.length
        if (before != null) { end = dataset.findIndex(b => b.time >= before); if (end < 0) end = dataset.length }
        const start = Math.max(0, end - limit)
        rows = dataset.slice(start, end)
        moreBefore = start > 0
        moreAfter = end < dataset.length
    }
    const first = rows.length ? dataset.indexOf(rows[0]) : 0
    return { series: 'spot-v1', bars: rows, has_more_before: moreBefore, has_more_after: moreAfter,
        oldest_time: rows[0]?.time ?? null, newest_time: rows.at(-1)?.time ?? null, realtime_enabled: true,
        indicator_api_version: 1, indicators: rows.length ? await indicatorsFor(first, first + rows.length) : {} }
}

// ------------------------------------------------------------ test bridge
// Injected only into the served response, never into the repo assets.
const BRIDGE_CHART = `
const __calls = {}
const __series = new Set()
function __count(name) { __calls[name] = (__calls[name] || 0) + 1 }
function __wrapSeries(s) { __series.add(s); for (const n of ['setData', 'update', 'applyOptions']) { const real = s[n].bind(s); s[n] = (...a) => { __count(n); return real(...a) } } return s }
for (const n of ['addSeries', 'addCustomSeries']) { const real = chart[n].bind(chart); chart[n] = (...a) => { __count(n); return __wrapSeries(real(...a)) } }
{ const real = chart.removeSeries.bind(chart); chart.removeSeries = s => { __count('removeSeries'); __series.delete(s); return real(s) } }
{ const ts = chart.timeScale(); const real = ts.setVisibleLogicalRange.bind(ts); ts.setVisibleLogicalRange = r => { __count('setVisibleLogicalRange'); return real(r) } }
`
const BRIDGE_EXPOSE = `
{
const __setBars = setBars
const __setBarsMs = []
setBars = (...a) => { const t = performance.now(); try { return __setBars(...a) } finally { __setBarsMs.push(performance.now() - t) } }
const __longTasks = []
try { new PerformanceObserver(list => { for (const e of list.getEntries()) __longTasks.push({ start: e.startTime, duration: e.duration }) }).observe({ type: 'longtask' }) } catch {}
const __input = { mousedown: 0, mousemove: 0, wheel: 0, trusted: 0 }
for (const type of ['mousedown', 'mousemove', 'wheel']) chartEl.addEventListener(type, e => { __input[type]++; if (e.isTrusted) __input.trusted++ }, { capture: true, passive: true })
let __rec = null
function __frame(now) {
    if (!__rec) return
    if (__rec.last != null) __rec.intervals.push(now - __rec.last)
    __rec.last = now
    const range = currentChartRange()
    const vis = chart.timeScale().getVisibleRange()
    const n = page.bars.length
    if (range && n) {
        const from = Math.max(0, Math.ceil(range.from)), to = Math.min(n - 1, Math.floor(range.to))
        const covered = renderedWindow.start <= from && renderedWindow.start + renderedWindow.bars.length - 1 >= to
        if (!covered && !chartNeedsRender) __rec.coverageMisses++
        __rec.samples.push({ t: now, from: range.from, to: range.to, timeFrom: vis?.from ?? null, timeTo: vis?.to ?? null, start: renderedWindow.start, len: renderedWindow.bars.length, cached: n })
    }
    requestAnimationFrame(__frame)
}
let __hidden = null
Object.defineProperty(document, 'hidden', { configurable: true, get: () => __hidden ?? false })
globalThis.__cvd = {
    page, chart, elements,
    get window() { return { start: renderedWindow.start, length: renderedWindow.bars.length } },
    range: () => currentChartRange(),
    timeRange: () => chart.timeScale().getVisibleRange(),
    seriesCount: () => __series.size,
    loadMoreBefore: () => loadMoreBefore(),
    setRange: r => chart.timeScale().setVisibleLogicalRange(chartRangeForCache(r)),
    snapshot: () => ({ calls: { ...__calls }, setBarsMs: __setBarsMs.slice(), longTasks: __longTasks.slice(), input: { ...__input } }),
    reset() { for (const k of Object.keys(__calls)) delete __calls[k]; __setBarsMs.length = 0; __longTasks.length = 0; for (const k of Object.keys(__input)) __input[k] = 0 },
    startRecording() { __rec = { intervals: [], samples: [], coverageMisses: 0, last: null }; requestAnimationFrame(__frame) },
    stopRecording() { const r = __rec; __rec = null; return r },
    setHidden(value) { __hidden = value; document.dispatchEvent(new Event('visibilitychange')) },
    seriesData: () => [...__series].map(s => ({ type: s.seriesType(), visible: s.options().visible !== false, data: s.data() })),
    title: () => document.title,
    seriesHandle: key => renderers.get(key)?.seriesHandle?.(),
    segmentedInfo: () => [...renderers.values()].filter(r => r.output.kind === "segmented_lines").map(r => ({ key: r.output.key, count: r.seriesCount(), entries: r.entries().map(e => ({ kind: e.kind, marker: e.marker, width: e.lineWidth, color: e.color, points: e.series.data().length })) })),
    value: (key, mode, time) => localValues.get(key + "|" + mode)?.get(time),
    settle: () => new Promise(r => { const t = () => localCompute && localCompute.idle() ? requestAnimationFrame(() => requestAnimationFrame(r)) : setTimeout(t, 20); t() }),
    localHost: () => localHost, localRecords: () => localRecords, localValues: () => localValues, localCompute: () => localCompute,
    faults: () => Object.fromEntries([...localRecords.keys()].map(id => [id, localCompute?.fault(id)]).filter(([, f]) => f)),
    registry: () => ({ plugins: registry.plugins.map(p => p.id), pending: registry.pending.map(p => ({ id: p.id, status: p.status })) }),
    resync: async () => {}, // server indicator resync retired (browser computes locally)
    epoch: () => indicatorStore.epoch,
    loadIndicators: async () => {}, // server discovery retired: rows come only from local imports
    // Actual LWC series state per plugin (not app-level prefs).
    pluginSeries: id => [...renderers.values()].filter(r => r.output.pluginId === id).map(r => { const s = r.seriesHandle?.(); return { key: r.output.key, visible: !!s && s.options().visible !== false, points: s ? s.data().length : 0 } }),
}
}
`

function loadApp() {
    const source = fs.readFileSync(path.join(SRC, 'js/app.js'), 'utf8')
    const marker = 'const chart = mkChart(chartEl)'
    if (!source.includes(marker) || !/\ninit\(\)\s*$/.test(source)) throw new Error('app.js bridge anchors not found')
    return source.replace(marker, marker + '\n' + BRIDGE_CHART).replace(/\ninit\(\)\s*$/, '\n' + BRIDGE_EXPOSE + '\ninit()\n')
}

// ------------------------------------------------------------ loopback server
const sockets = new Set()
function wsSend(socket, text) {
    const payload = Buffer.from(text)
    let header
    if (payload.length < 126) header = Buffer.from([0x81, payload.length])
    else if (payload.length < 65536) { header = Buffer.alloc(4); header[0] = 0x81; header[1] = 126; header.writeUInt16BE(payload.length, 2) }
    else { header = Buffer.alloc(10); header[0] = 0x81; header[1] = 127; header.writeBigUInt64BE(BigInt(payload.length), 2) }
    socket.write(Buffer.concat([header, payload]))
}
function pushLive(bar) {
    control.livePushes++ // quiescence witness for indicator_menu reload controls
    const index = dataset.findIndex(b => b.time === bar.time)
    if (index >= 0) dataset[index] = bar
    else dataset.push(bar)
    const rev = ++baseRev
    for (const socket of sockets) wsSend(socket, JSON.stringify({ series: 'spot-v1', action: 'append', rev, bar }))
    // Separate, non-blocking indicator frames (as the host's live hub does).
    const at = index >= 0 ? index : dataset.length - 1
    const snapshot = dataset.slice(), scope = { ...currentScope }
    liveFrames = liveFrames.then(() => indicatorsFor(at, at + 1, snapshot, scope)).then(results => {
        for (const [id, entry] of Object.entries(results)) {
            const output = entry.modes.source.output
            if (!output) continue
            const seq = (liveIndicator.seq.get(id) ?? -1) + 1
            liveIndicator.seq.set(id, seq)
            const frame = JSON.stringify({ type: 'indicator', api_version: 1, id, version: entry.version, mode: 'source',
                generation: liveIndicator.generation, session: liveIndicator.session, seq, as_of_rev: rev, output })
            for (const socket of sockets) wsSend(socket, frame)
        }
    }).catch(error => console.warn('live indicator', error))
}
let liveFrames = Promise.resolve()

const html = fs.readFileSync(path.join(SRC, 'index.html'), 'utf8').replace(/<link[^>]*fonts\.(googleapis|gstatic)[^>]*>/g, '')
const appSource = loadApp()
const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1')
    if (url.pathname === '/meta') {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ contract_series: 'contracts-drb-v1', exchanges: [{ exchange: 'binance', markets: ['spot'], symbols: ['BTC'] }] }))
        return
    }
    if (url.pathname === '/indicators') {
        if (control.discovery === 'error') { res.writeHead(503); res.end(); return }
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify(control.discovery ?? discovery))
        return
    }
    if (url.pathname === '/history') {
        const entry = { at: Date.now(), before: url.searchParams.get('before'), after: url.searchParams.get('after'), limit: url.searchParams.get('limit') }
        control.requests.push(entry)
        if (entry.before && control.beforeDelayMs) await new Promise(r => setTimeout(r, control.beforeDelayMs))
        entry.answeredAt = Date.now()
        res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' })
        noteScope(url.searchParams)
        res.end(JSON.stringify(await historyPage(url.searchParams)))
        return
    }
    const fault = control.assetFault
    if (fault && url.pathname === fault.path) {
        if (fault.status) { res.writeHead(fault.status); res.end(); return }
        if (fault.delayMs) await new Promise(r => setTimeout(r, fault.delayMs))
    }
    let data, mime
    if (url.pathname === '/') { data = html; mime = 'text/html' }
    else if (url.pathname === '/js/app.js') { data = appSource; mime = 'text/javascript' }
    else {
        const file = path.resolve(SRC, '.' + decodeURIComponent(url.pathname))
        if (!file.startsWith(SRC + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) { res.writeHead(404); res.end(); return }
        data = fs.readFileSync(file)
        mime = file.endsWith('.css') ? 'text/css' : /\.m?js$/.test(file) ? 'text/javascript' : 'application/octet-stream'
    }
    res.writeHead(200, { 'Content-Type': mime, 'Cache-Control': 'no-store' })
    res.end(data)
})
server.on('upgrade', (req, socket) => {
    const key = req.headers['sec-websocket-key']
    const accept = crypto.createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64')
    control.wsUpgrades++
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
    sockets.add(socket)
    socket.on('data', () => {}) // client pings are ignored
    socket.on('close', () => sockets.delete(socket))
    socket.on('error', () => sockets.delete(socket))
})
await new Promise(r => server.listen(0, '127.0.0.1', r))
const PORT = server.address().port

// ------------------------------------------------------------ CDP
const profileDir = fs.mkdtempSync(path.join(process.env.TMPDIR || (() => { throw new Error('TMPDIR must be set') })(), 'cvd-interaction-'))
const chrome = spawn(CHROMIUM, ['--headless=new', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-first-run',
    '--disable-background-networking', '--disable-background-timer-throttling', '--disable-renderer-backgrounding',
    `--window-size=${VIEW.width},${VIEW.height}`, '--remote-debugging-port=0', '--user-data-dir=' + profileDir, 'about:blank'],
{ stdio: ['ignore', 'ignore', 'pipe'] })
let socket
const errors = []
const sleep = ms => new Promise(r => setTimeout(r, ms))
let seq = 0
const pending = new Map()
function call(method, params = {}) {
    return new Promise((resolve, reject) => {
        const id = ++seq
        const timer = setTimeout(() => { pending.delete(id); reject(new Error(method + ' timeout')) }, 60000)
        pending.set(id, { resolve, reject, timer })
        socket.send(JSON.stringify({ id, method, params }))
    })
}
async function evaluate(expression) {
    const r = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 2000))
    return r.result.value
}
const frames = n => evaluate(`new Promise(r => { let k = ${n}; const f = () => --k <= 0 ? r(true) : requestAnimationFrame(f); requestAnimationFrame(f) })`)
async function waitFor(expression, timeout = 15000) {
    const deadline = Date.now() + timeout
    while (!(await evaluate(expression))) {
        if (Date.now() > deadline) throw new Error('timeout waiting for ' + expression)
        await sleep(25)
    }
}

// ------------------------------------------------------------ input helpers
let paneBox = null
async function drag(dx, steps = 30, stepMs = 16, between = null) {
    const y = paneBox.y + paneBox.height * 0.25
    let x = paneBox.x + paneBox.width / 2 - dx / 2
    await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y })
    await call('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1 })
    for (let i = 1; i <= steps; i++) {
        x += dx / steps
        await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y, button: 'left', buttons: 1 })
        if (between) await between(i)
        await sleep(stepMs)
    }
    await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1 })
}
async function wheel(deltaY, count, stepMs = 16) {
    const x = paneBox.x + paneBox.width * 0.6, y = paneBox.y + paneBox.height * 0.25
    for (let i = 0; i < count; i++) {
        await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x, y, deltaX: 0, deltaY })
        await sleep(stepMs)
    }
}

// ------------------------------------------------------------ scenario setup
// Browser-local WASM (lane B): the app computes indicators itself from
// locally imported modules; the oracle envelopes above (shipped worker+host in Node) remain only as
// the native NUMERICAL ORACLE (and must never be displayed). The REAL public
// CVD/Flow .wasm are imported once per profile through the trusted file input
// (DOM.setFileInputFiles) and enabled with trusted clicks; IndexedDB keeps
// them across navigations exactly like a user's browser.
const LOCAL_WASM = (process.env.CVD_LOCAL_WASM || '').split(',').filter(Boolean)
if (!LOCAL_WASM.length) throw new Error('CVD_LOCAL_WASM is required (comma-separated public cvd/flow .wasm paths)')
async function ensureLocalPlugins() {
    await waitFor(`!!__cvd.localHost && !!__cvd.localHost()`)
    const ids = runners.map(r => r.descriptor.id)
    if (!(await evaluate(`${JSON.stringify(ids)}.every(id => __cvd.localRecords().has(id))`))) {
        const { root } = await call('DOM.getDocument', { depth: -1 })
        for (const file of LOCAL_WASM) {
            const { nodeId } = await call('DOM.querySelector', { nodeId: root.nodeId, selector: '.plugin-import-input' })
            const before = await evaluate('__cvd.localRecords().size')
            await call('DOM.setFileInputFiles', { nodeId, files: [file] })
            await waitFor(`__cvd.localRecords().size > ${before} || /already/.test(document.querySelector('.plugin-menu-notice')?.textContent ?? '')`)
        }
    }
    for (const id of ids) {
        if (await evaluate(`__cvd.localRecords().get(${JSON.stringify(id)}).enabled`)) continue
        if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`)
        const p = await evaluate(`(() => { const b = document.querySelector('.plugin-menu-row[data-plugin=${JSON.stringify(id)}] input').getBoundingClientRect(); return { x: b.left + b.width / 2, y: b.top + b.height / 2 } })()`)
        await call('Input.dispatchMouseEvent', { type: 'mousePressed', ...p, button: 'left', buttons: 1, clickCount: 1 })
        await call('Input.dispatchMouseEvent', { type: 'mouseReleased', ...p, button: 'left', buttons: 0, clickCount: 1 })
    }
    if (!(await evaluate(`document.getElementById('plugin-menu').hidden`))) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`)
    await waitFor(`${JSON.stringify(ids)}.every(id => [...__cvd.localValues().keys()].some(k => k.startsWith(id + '/'))) && __cvd.localCompute().idle()`, 30000)
}
const CACHE_TARGET = 16000
// Price + one pane per separate (indicator, pane_group) from real descriptors.
const EXPECTED_PANES = 1 + new Set(runners.flatMap(r => r.descriptor.outputs.filter(o => o.pane === 'separate' && !o.readout_only).map(o => `${r.descriptor.id}:${o.pane_group ?? o.key}`))).size
async function openPage() {
    resetDataset()
    control.beforeDelayMs = 0
    control.discovery = null
    control.assetFault = null
    control.requests.length = 0
    await call('Page.navigate', { url: `http://127.0.0.1:${PORT}/` })
    await waitFor(`!!globalThis.__cvd && __cvd.page.bars.length > 0 && document.getElementById('status').textContent === 'live'`)
    await ensureLocalPlugins()
    // Setup only: grow the cache through the real older-page path.
    for (let i = 0; i < 40 && (await evaluate('__cvd.page.bars.length')) < CACHE_TARGET; i++) {
        await sleep(320)
        await evaluate('__cvd.loadMoreBefore()')
    }
    await evaluate("__cvd.settle()") // setup: let browser-local recomputes of the grown cache land before measuring
    await frames(3)
    paneBox = await evaluate(`(() => { const r = __cvd.elements.chartEl.getBoundingClientRect(); const h = __cvd.chart.panes()[0].getHeight(); return { x: r.left, y: r.top, width: r.width - 70, height: h } })()`)
}
async function position(from, width = 180) {
    // Setup only. A far jump is clamped by LWC to the rendered window, so
    // repeat until the window has moved and the range is exact.
    for (let i = 0; i < 16; i++) {
        await evaluate(`__cvd.setRange({ from: ${from}, to: ${from + width} })`)
        await frames(4)
        await sleep(50)
        const range = await evaluate('__cvd.range()')
        if (Math.abs(range.from - from) < 0.5 && Math.abs(range.to - from - width) < 0.5) return
    }
    throw new Error(`could not position view at ${from}`)
}
async function measure(body) {
    await evaluate('__cvd.reset(); __cvd.startRecording()')
    const requestsBefore = control.requests.length
    const t = Date.now()
    const extra = await body()
    await frames(6)
    const rec = await evaluate('__cvd.stopRecording()')
    const snap = await evaluate('__cvd.snapshot()')
    return { wallMs: Date.now() - t, requests: control.requests.slice(requestsBefore), ...snap, frames: rec.intervals,
        coverageMisses: rec.coverageMisses, samples: rec.samples, extra }
}
const setDataCount = m => (m.calls.setData || 0)
const writeCount = m => ['setData', 'update', 'addSeries', 'addCustomSeries', 'removeSeries'].reduce((n, k) => n + (m.calls[k] || 0), 0)

// ------------------------------------------------------------ scenarios
const scenarios = {
    async source_reset_new_grid() {
        // Hidden series participate in LWC's unified time axis. Exercise real
        // reloads onto different synthetic grids, not only mock series arrays.
        const m = await measure(async () => {
            const flow = async mode => {
                await evaluate(`(() => { const e = document.querySelector('select[data-group="flow:flow"]'); e.value = '${mode}'; e.dispatchEvent(new Event('change')) })()`)
                await frames(3)
            }
            await flow('flow/buy_sell')
            await flow('flow/delta') // both representations have held the old grid
            const resets = []
            for (const [hidden, tf, step, mode] of [[false, '5m', 300, 'flow/delta'], [true, '15m', 900, 'flow/buy_sell']]) {
                await flow(mode)
                dataset = dataset.filter(b => b.time % step === 0)
                if (hidden) await evaluate('__cvd.setHidden(true)')
                const before = await evaluate('__cvd.snapshot().calls')
                await evaluate(`__cvd.elements.tfSel.value = '${tf}'; __cvd.elements.tfSel.onchange()`)
                const after = await evaluate('__cvd.snapshot().calls')
                if (hidden) await evaluate('__cvd.setHidden(false)')
                await frames(8)
                const state = await evaluate(`(() => {
                    const cache = new Set(__cvd.page.bars.map(b => b.time))
                    const all = __cvd.chart.panes().flatMap(p => p.getSeries())
                    const flow = __cvd.chart.panes().map(p => p.getSeries()).find(ss => ss.includes(__cvd.seriesHandle('flow/buy_sell')))
                    const inactive = flow.find(s => s.seriesType() === '${mode === 'flow/delta' ? 'Custom' : 'Histogram'}')
                    const last = __cvd.page.bars.at(-1)
                    const x = __cvd.chart.timeScale().timeToCoordinate(last.time)
                    return { panes: __cvd.chart.panes().length, cached: cache.size,
                        wrongGrid: [...cache].filter(t => t % ${step} !== 0),
                        foreignTimes: [...new Set(all.flatMap(s => s.data().map(p => p.time)).filter(t => !cache.has(t)))],
                        inactiveRows: inactive.data().length,
                        logicalLast: __cvd.chart.timeScale().coordinateToLogical(x), expectedLast: cache.size - 1 }
                })()`)
                resets.push({ hidden, tf, state, hiddenCallsUnchanged: !hidden || JSON.stringify(before) === JSON.stringify(after) })
            }
            return resets
        })
        return { m, checks: {
            different_grids: m.extra.every(x => x.state.cached > 0 && x.state.wrongGrid.length === 0),
            no_hidden_old_source_points: m.extra.every(x => x.state.foreignTimes.length === 0 && x.state.inactiveRows === 0),
            native_axis_matches_cache: m.extra.every(x => Number.isFinite(x.state.logicalLast) && Math.abs(x.state.logicalLast - x.state.expectedLast) < 1e-6),
            hidden_zero_chart_calls: m.extra.every(x => x.hiddenCallsUnchanged),
            panes_retained: m.extra.every(x => x.state.panes === EXPECTED_PANES),
        } }
    },
    async pane_contract() {
        // Legacy stored prefs carried showRd/showKd/showFd/showHd; they must
        // neither resurrect a Strength pane nor survive the next save.
        const m = await measure(async () => {
            const layout = await evaluate(`(() => ({
                panes: __cvd.chart.panes().length,
                strengthDom: document.querySelectorAll('#toggle-rd,#toggle-kd,#toggle-fd,#toggle-hd,#signal-state').length,
                labels: [...document.querySelectorAll('.pane-label')].length,
                bandSeries: __cvd.chart.panes()[0].getSeries().length,
                paneTypes: __cvd.chart.panes().map(p => p.getSeries().map(s => s.seriesType())),
                prefs: JSON.parse(localStorage.getItem('cvd_prefs_v1')),
                quality: document.getElementById('quality-state').textContent,
            }))()`)
            const flowSelect = value => evaluate(`(() => { const e = document.querySelector('select[data-group="flow:flow"]'); e.value = '${value}'; e.dispatchEvent(new Event('change')); return new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))) })()`)
            const flowState = () => evaluate(`(() => { const s = __cvd.chart.panes().find(p => p.getSeries().includes(__cvd.seriesHandle('flow/buy_sell'))).getSeries(); const custom = s.find(x => x.seriesType() === 'Custom'); const hist = s.find(x => x.seriesType() === 'Histogram'); return { custom: custom?.options().visible, hist: hist?.options().visible, last: custom?.data().at(-1)?.values ?? null, delta: hist?.data().at(-1)?.value ?? null, summary: document.querySelector('[data-pane-summary="flow:flow"]').textContent } })()`)
            await flowSelect('flow/buy_sell')
            const buySell = await flowState()
            await evaluate(`(() => { const e = __cvd.elements.aggToggle; e.checked = true; e.onchange(); return __cvd.settle() })()`)
            const agg = { ...(await flowState()), cvd: await evaluate(`__cvd.chart.panes()[1].getSeries()[0].data().at(-1).value`) }
            await evaluate(`(() => { const e = __cvd.elements.aggToggle; e.checked = false; e.onchange(); return __cvd.settle() })()`)
            await flowSelect('flow/delta')
            const delta = await flowState()
            const labelGeometry = await evaluate(`(() => { const top = document.querySelector('.plugin-pane-label[data-pane="flow:flow"]').getBoundingClientRect().top - __cvd.elements.chartEl.getBoundingClientRect().top; const panes = __cvd.chart.panes(); const i = panes.findIndex(p => p.getSeries().includes(__cvd.seriesHandle('flow/buy_sell'))); return { top, expected: panes.slice(0, i).reduce((a, p) => a + p.getHeight(), 0) + i } })()`)
            const savedPrefs = await evaluate(`JSON.parse(localStorage.getItem('cvd_prefs_v1'))`)
            return { layout, buySell, agg, delta, labelGeometry, savedPrefs }
        })
        const x = m.extra
        const last = dataset.at(-1)
        return { m, checks: {
            panes_from_descriptors: x.layout.panes === EXPECTED_PANES && x.layout.labels === EXPECTED_PANES,
            no_strength_dom: x.layout.strengthDom === 0,
            // One-time legacy migration: old LEVELS/Flow choices carried into plugin prefs.
            prefs_migrated: ['showRd', 'showKd', 'showFd', 'showHd', 'showLevels', 'flowMode'].every(k => !(k in x.layout.prefs)) && x.layout.prefs.plugins?.enabled?.levels === true && x.layout.prefs.plugins?.modes?.['flow:flow'] === 'flow/delta' && x.layout.prefs.tf === '1m',
            total_readout_both_modes: /· Total \d/.test(x.buySell.summary) && /· Total \d/.test(x.delta.summary),
            quality_badge: typeof x.layout.quality === 'string' && x.layout.quality.length > 0,
            buy_sell_visible: x.buySell.custom === true && x.buySell.hist === false && JSON.stringify(x.buySell.last) === JSON.stringify([last.buy_volume_usdt, 0, -last.sell_volume_usdt, 0]),
            agg_flow_and_cvd: JSON.stringify(x.agg.last) === JSON.stringify([last.agb, 0, -last.ags, 0]) && x.agg.cvd === last.ac,
            delta_restored: x.delta.hist === true && x.delta.custom === false && Math.abs(x.delta.delta - last.delta_usdt) < 1e-9,
            flow_label_on_third_pane: Math.abs(x.labelGeometry.top - x.labelGeometry.expected) < 16,
            prefs_saved_without_strength: ['showRd', 'showKd', 'showFd', 'showHd'].every(k => !(k in x.savedPrefs)) && x.savedPrefs.plugins.modes['flow:flow'] === 'flow/delta',
        } }
    },
    async cached_drag() {
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 2000)
        const before = await evaluate('__cvd.window')
        const m = await measure(() => drag(600))
        const after = await evaluate('__cvd.window')
        return { m, checks: {
            trusted_input: m.input.trusted > 20,
            moved: Math.abs(m.samples.at(-1).from - m.samples[0].from) > 50,
            no_setData: setDataCount(m) === 0,
            no_request: m.requests.length === 0,
            window_unchanged: before.start === after.start && before.length === after.length,
            coverage: m.coverageMisses === 0,
        } }
    },
    async cached_zoom() {
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 2000, 300)
        const before = await evaluate('__cvd.window')
        const m = await measure(async () => { await wheel(-100, 8); await wheel(100, 8) })
        const after = await evaluate('__cvd.window')
        const widths = m.samples.map(s => s.to - s.from)
        return { m, checks: {
            trusted_input: m.input.wheel >= 16 && m.input.trusted >= 16,
            zoomed: Math.max(...widths) - Math.min(...widths) > 30,
            no_setData: setDataCount(m) === 0,
            no_request: m.requests.length === 0,
            window_unchanged: before.start === after.start && before.length === after.length,
            coverage: m.coverageMisses === 0,
        } }
    },
    async crossing_buffer() {
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 2000)
        const w = await evaluate('__cvd.window')
        // Start just inside the render window's left margin, then drag older
        // far enough for one boundary crossing (not to the cache start).
        await position(w.start + 300)
        const before = await evaluate('({ window: __cvd.window, range: __cvd.range(), spacing: __cvd.chart.timeScale().width() / (__cvd.range().to - __cvd.range().from) })')
        const dx = 420
        if (process.env.CVD_CPU_PROFILE) { await call('Profiler.enable'); await call('Profiler.setSamplingInterval', { interval: 500 }); await call('Profiler.start') }
        const m = await measure(() => drag(dx, 20))
        if (process.env.CVD_CPU_PROFILE) { const { profile } = await call('Profiler.stop'); fs.writeFileSync(path.join(OUT, `crossing_buffer-${Date.now()}.cpuprofile`), JSON.stringify(profile)) }
        const after = await evaluate('({ window: __cvd.window, range: __cvd.range() })')
        let maxJump = 0
        for (let i = 1; i < m.samples.length; i++) maxJump = Math.max(maxJump, Math.abs(m.samples[i].from - m.samples[i - 1].from))
        // Dragging right by dx pixels moves the view dx / barSpacing bars older;
        // a re-render that lost the anchor would show up as extra shift.
        const expectedShift = -dx / before.spacing
        const actualShift = after.range.from - before.range.from
        m.extra = { maxPerFrameShiftBars: maxJump, expectedShift, actualShift, windowBefore: before.window, windowAfter: after.window }
        return { m, checks: {
            trusted_input: m.input.trusted > 15,
            window_changed: before.window.start !== after.window.start,
            no_request: m.requests.length === 0,
            coverage: m.coverageMisses === 0,
            // LWC starts a drag on the first pressed move (and may need a
            // small threshold), so up to two of the 20 steps are not scrolled.
            anchor_kept: Math.abs(actualShift - expectedShift) <= 2 * (dx / 20) / before.spacing + 1,
        } }
    },
    async crossing_newer() {
        // Mirror of crossing_buffer: start near the render window's right
        // margin and drag newer (left) across the boundary.
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 6000)
        const w = await evaluate('__cvd.window')
        await position(w.start + w.length - 480)
        const before = await evaluate('({ window: __cvd.window, range: __cvd.range(), spacing: __cvd.chart.timeScale().width() / (__cvd.range().to - __cvd.range().from) })')
        const dx = -420
        const m = await measure(() => drag(dx, 20))
        const after = await evaluate('({ window: __cvd.window, range: __cvd.range() })')
        const expectedShift = -dx / before.spacing
        const actualShift = after.range.from - before.range.from
        m.extra = { expectedShift, actualShift, windowBefore: before.window, windowAfter: after.window }
        return { m, checks: {
            trusted_input: m.input.trusted > 15,
            window_changed: before.window.start !== after.window.start,
            no_request: m.requests.length === 0,
            coverage: m.coverageMisses === 0,
            anchor_kept: Math.abs(actualShift - expectedShift) <= 2 * (Math.abs(dx) / 20) / before.spacing + 1,
        } }
    },
    async long_drag() {
        // Several consecutive older drags crossing multiple windows; each
        // gesture's displacement must match its pixel distance and the
        // visible time range must stay consistent with cache indices.
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 2000)
        const spacing = await evaluate('__cvd.chart.timeScale().width() / (__cvd.range().to - __cvd.range().from)')
        const dx = 1000, reps = 12
        const startWindow = await evaluate('__cvd.window')
        const shifts = []
        const m = await measure(async () => {
            for (let i = 0; i < reps; i++) {
                const r0 = await evaluate('__cvd.range().from')
                await drag(dx, 20)
                await frames(2)
                shifts.push((await evaluate('__cvd.range().from')) - r0)
            }
        })
        const endWindow = await evaluate('__cvd.window')
        // Visible time range must map to the same cache bars as the logical
        // range (global cache indices); bracket fractional edges by floor/ceil.
        const timeCheck = await evaluate(`(() => { const r = __cvd.range(), t = __cvd.timeRange(), b = __cvd.page.bars, n = b.length - 1; const at = i => b[Math.max(0, Math.min(n, i))].time; return { r, t, ok: !!t && t.from >= at(Math.floor(r.from)) && t.from <= at(Math.ceil(r.from)) && t.to >= at(Math.floor(r.to)) && t.to <= at(Math.ceil(r.to)) } })()`)
        const timeOk = timeCheck.ok
        const expected = -dx / spacing, tol = 2 * (dx / 20) / spacing + 1
        m.extra = { shifts, expected, startWindow, endWindow, timeCheck }
        return { m, checks: {
            trusted_input: m.input.trusted > 15 * reps,
            windows_moved: startWindow.start !== endWindow.start,
            no_request: m.requests.length === 0,
            coverage: m.coverageMisses === 0,
            anchor_kept_each: shifts.every(s => Math.abs(s - expected) <= tol),
            time_consistent: timeOk,
        } }
    },
    async wide_zoom_return() {
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 4000, 300)
        const before = await evaluate('__cvd.range()')
        const m = await measure(async () => { await wheel(300, 40); await frames(3); await wheel(-300, 40) })
        const widths = m.samples.map(s => s.to - s.from)
        m.extra = { maxWidth: Math.max(...widths), maxWindow: Math.max(...m.samples.map(s => s.len)) }
        return { m, checks: {
            trusted_input: m.input.wheel >= 80,
            window_enlarged: m.extra.maxWindow > 3200,
            coverage: m.coverageMisses === 0,
            // Zoom-out saturates at minBarSpacing, so the same number of
            // zoom-in events ends narrower than the start; require narrow again.
            zoomed_back_in: widths.at(-1) < 600,
        } }
    },
    async history_prepend_delayed() {
        // Not at reachedStart: the cache start is a real older-page boundary.
        await position(45, 180)
        control.beforeDelayMs = 1200
        const cachedBefore = await evaluate('__cvd.page.bars.length')
        let pendingDuringDrag = false
        const m = await measure(async () => {
            await drag(240, 15)
            // Keep dragging (cached) while the older page is delayed.
            await drag(-120, 15, 16, async () => { if (control.requests.some(r => r.before && !r.answeredAt)) pendingDuringDrag = true })
            await sleep(300)
            const settled = await evaluate('({ time: __cvd.timeRange(), length: __cvd.page.bars.length })')
            await waitFor(`__cvd.page.bars.length > ${cachedBefore}`, 6000)
            await frames(4)
            const afterPrepend = await evaluate('({ time: __cvd.timeRange(), length: __cvd.page.bars.length })')
            return { settled, afterPrepend }
        })
        control.beforeDelayMs = 0
        const { settled, afterPrepend } = m.extra
        return { m, checks: {
            trusted_input: m.input.trusted > 20,
            delayed_request: m.requests.some(r => r.before && r.answeredAt - r.at >= 1100),
            dragged_while_pending: pendingDuringDrag,
            not_loaded_before_answer: settled.length === cachedBefore,
            prepended: afterPrepend.length > cachedBefore,
            time_anchor_kept: settled.time.from === afterPrepend.time.from && settled.time.to === afterPrepend.time.to,
            coverage: m.coverageMisses === 0,
        } }
    },
    async realtime_while_history() {
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 3000)
        const range = await evaluate('__cvd.timeRange()')
        const last = dataset.at(-1)
        const pushed = []
        const m = await measure(async () => {
            await drag(200, 20, 16, async i => {
                if (i % 4) return
                const bar = { ...makeBar(TOTAL + pushed.length), time: last.time + STEP * (pushed.length + 1) }
                pushed.push(bar); pushLive(bar)
                pushLive({ ...bar, price_close: bar.price_close + 1 }) // same-time correction
            })
            await sleep(200)
        })
        const after = await evaluate('({ time: __cvd.timeRange(), newest: __cvd.page.newestTime, last: __cvd.page.bars.at(-1).price_close })')
        // Then go to live (setup-style) and verify the tail was not lost.
        await position(cached + pushed.length - 150, 160)
        const tail = await evaluate(`__cvd.seriesData()[0].data.slice(-3).map(p => ({ time: p.time, close: p.close }))`)
        const expect = pushed.at(-1)
        return { m, checks: {
            trusted_input: m.input.trusted > 15,
            cache_updated: after.newest === expect.time && after.last === expect.price_close + 1,
            not_pulled_to_live: after.time.to < last.time - STEP * 1000 && after.time.from < range.from,
            no_request: m.requests.every(r => !r.before),
            coverage: m.coverageMisses === 0,
            tail_rendered: tail.at(-1).time === expect.time && tail.at(-1).close === expect.price_close + 1,
        } }
    },
    async hidden_resize_resume() {
        const cached = await evaluate('__cvd.page.bars.length')
        await position(cached - 170, 160)
        const last = dataset.at(-1)
        const m = await measure(async () => {
            const anchor = await evaluate('__cvd.timeRange()')
            await evaluate('__cvd.setHidden(true)')
            const titleBefore = await evaluate('__cvd.title()')
            await evaluate('__cvd.reset()')
            for (let i = 1; i <= 5; i++) { pushLive({ ...makeBar(TOTAL + i), time: last.time + STEP * i }); await sleep(30) }
            await call('Emulation.setDeviceMetricsOverride', { width: 1100, height: 800, deviceScaleFactor: VIEW.deviceScaleFactor, mobile: false })
            await sleep(200)
            await waitFor(`__cvd.page.newestTime === ${last.time + STEP * 5}`)
            const hiddenSnap = await evaluate('__cvd.snapshot()')
            const titleHidden = await evaluate('__cvd.title()')
            await evaluate('__cvd.reset(); __cvd.setHidden(false)')
            await frames(6)
            const resumeSnap = await evaluate('__cvd.snapshot()')
            await call('Emulation.setDeviceMetricsOverride', { width: VIEW.width, height: VIEW.height, deviceScaleFactor: VIEW.deviceScaleFactor, mobile: false })
            await frames(4)
            const tail = await evaluate(`__cvd.seriesData()[0].data.at(-1).time`)
            return { anchor, titleBefore, titleHidden, hiddenWrites: writeCount(hiddenSnap), resumeSetBars: resumeSnap.setBarsMs.length, resumeSetBarsMs: resumeSnap.setBarsMs, tail }
        })
        const x = m.extra
        return { m, checks: {
            no_chart_writes_hidden: x.hiddenWrites === 0,
            title_refreshed_hidden: x.titleHidden !== x.titleBefore,
            one_resume_render: x.resumeSetBars === 1,
            tail_rendered: x.tail === last.time + STEP * 5,
        } }
    },
    async lifecycle_fences() {
        // Real-browser availability / fencing gate. Frames are hand-sequenced
        // (generation/session/seq/as_of_rev) but every output VALUE is computed
        // by the WASM oracle (same public .wasm, shipped host) for the exact bar.
        const plugin = runners[0].descriptor
        const outKey = plugin.outputs.find(o => !o.readout_only).key
        const key = `${plugin.id}/${outKey}`
        const G = liveIndicator.generation
        const send = obj => { for (const s of sockets) wsSend(s, JSON.stringify(obj)) }
        const realOutput = async (at, snapshot = dataset.slice()) => (await indicatorsFor(at, at + 1, snapshot))[plugin.id].modes.source.output
        const frame = (session, seq, rev, output) => send({ type: 'indicator', api_version: 1, id: plugin.id, version: plugin.version, mode: 'source', generation: G, session, seq, as_of_rev: rev, output })
        const status = (session, ok) => send({ type: 'indicator_status', id: plugin.id, mode: 'source', generation: G, session, status: ok ? 'ok' : 'unavailable', ...(ok ? {} : { error: { code: 'crashed', message: 'x' } }) })
        const base = (bar, rev) => send({ series: 'spot-v1', action: 'append', rev, bar })
        const val = time => evaluate(`__cvd.value(${JSON.stringify(key)}, 'source', ${time})`)
        const settle = () => sleep(150).then(() => frames(3))
        const m = await measure(async () => {
            const x = {}
            const at = dataset.length - 1, last = dataset[at]
            const out = await realOutput(at)
            const v = out.series[outKey].values[0]
            const R = ++baseRev
            base(last, R); await settle()
            x.history_pending_on_newer_base = (await val(last.time)) === undefined
            frame(10, 0, R - 1, out); await settle()
            x.stale_rev_pending = (await val(last.time)) === undefined
            frame(10, 1, R, out); await settle()
            x.current = await val(last.time); x.expected = v
            // Real HTTP history overlap (same tuple) must not erase the live value.
            await evaluate('__cvd.resync()'); await settle()
            x.after_overlap = await val(last.time)
            // Changed tuple in a late overlapping response: live stays current.
            const altered = { ...out, series: { ...out.series, [outKey]: { ...out.series[outKey], values: [v + 12345] } } }
            // Terminal error, then stale sessions must not revive.
            status(10, false); await settle()
            x.err_cleared = (await val(last.time)) === undefined
            x.err_fault = (await evaluate('__cvd.faults()'))[`${plugin.id}:source`]?.state ?? null
            frame(9, 99, R, altered); await settle()
            x.old_session_rejected = (await val(last.time)) === undefined
            frame(10, 2, R, altered); await settle()
            x.failed_session_rejected = (await val(last.time)) === undefined
            // Recovery on a newer session.
            status(11, true); frame(11, 0, R, out); await settle()
            x.recovered = await val(last.time)
            x.fault_cleared = !(await evaluate('__cvd.faults()'))[`${plugin.id}:source`]
            frame(11, 1, R - 1, altered); await settle()
            x.older_rev_ignored = await val(last.time)
            // Output-before-bar: indicator result for a bar the host lacks.
            const nb = { ...makeBar(dataset.length), time: last.time + STEP }
            const snap = [...dataset, nb]
            const nout = await realOutput(snap.length - 1, snap)
            const before = await evaluate('({ n: __cvd.page.bars.length, last: __cvd.page.bars.at(-1).time, foreign: __cvd.seriesData().some(s => s.data.some(p => p.time === ' + nb.time + ')) })')
            frame(11, 2, R + 1, nout); await settle()
            x.buffer_axis = await evaluate('({ n: __cvd.page.bars.length, last: __cvd.page.bars.at(-1).time, foreign: __cvd.seriesData().some(s => s.data.some(p => p.time === ' + nb.time + ')) })')
            x.buffer_before = before
            dataset.push(nb); const R2 = ++baseRev; base(nb, R2); await settle()
            x.buffer_merged = await val(nb.time); x.buffer_expected = nout.series[outKey].values[0]
            // Hidden: live frames are stored but no chart writes happen.
            await evaluate('__cvd.setHidden(true)')
            const hb = await evaluate('JSON.stringify(__cvd.snapshot().calls)')
            frame(11, 3, R2, nout); await settle()
            x.hidden_zero_writes = hb === await evaluate('JSON.stringify(__cvd.snapshot().calls)')
            await evaluate('__cvd.setHidden(false)'); await settle()
            // Whole-server restart: drop sockets; counters restart low.
            const epoch0 = await evaluate('__cvd.epoch()')
            for (const s of [...sockets]) s.destroy()
            await waitFor('__cvd.epoch() > ' + epoch0 + " && document.getElementById('status').textContent === 'live'", 20000)
            await sleep(300)
            send({ series: 'spot-v1', action: 'append', rev: 1, bar: nb }); await settle()
            send({ type: 'indicator', api_version: 1, id: plugin.id, version: plugin.version, mode: 'source', generation: 1, session: 0, seq: 0, as_of_rev: 1, output: nout }); await settle()
            x.reconnect_value = await val(nb.time)
            return x
        })
        const x = m.extra
        return { m, checks: {
            history_pending_on_newer_base: x.history_pending_on_newer_base,
            stale_rev_pending: x.stale_rev_pending,
            current_real_value: x.current === x.expected && Number.isFinite(x.expected),
            http_overlap_keeps_live: x.after_overlap === x.expected,
            error_clears: x.err_cleared && x.err_fault === 'unavailable',
            old_session_cannot_revive: x.old_session_rejected,
            failed_session_terminal: x.failed_session_rejected,
            newer_session_recovers: x.recovered === x.expected && x.fault_cleared,
            older_rev_ignored: x.older_rev_ignored === x.expected,
            output_before_bar_no_axis_growth: x.buffer_axis.n === x.buffer_before.n && x.buffer_axis.last === x.buffer_before.last && !x.buffer_axis.foreign,
            buffered_merge_on_bar: x.buffer_merged === x.buffer_expected,
            hidden_zero_writes: x.hidden_zero_writes,
            reconnect_counters_restart: x.reconnect_value === x.buffer_expected,
        } }
    },
    async history_coherence() {
        // Base bar and derived result are ONE admission decision: whenever the
        // public CVD result is shown it must equal the DISPLAYED canonical
        // cvd_usdt. Values come from the WASM oracle (same public .wasm) via
        // the real /history path (resync) and real typed WS frames.
        const cvd = runners.find(r => r.descriptor.id === 'cvd')
        if (!cvd) return { m: null, checks: { cvd_library_loaded: false } }
        const G = liveIndicator.generation
        const send = obj => { for (const s of sockets) wsSend(s, JSON.stringify(obj)) }
        const realOutput = async (at, snapshot) => (await indicatorsFor(at, at + 1, snapshot))[cvd.descriptor.id].modes.source.output
        let seq = 0
        const SESSION = 50
        const frame = (rev, output) => send({ type: 'indicator', api_version: 1, id: 'cvd', version: cvd.descriptor.version, mode: 'source', generation: G, session: SESSION, seq: seq++, as_of_rev: rev, output })
        const base = (bar, rev) => send({ series: 'spot-v1', action: 'append', rev, bar })
        const settle = () => sleep(150).then(() => frames(3))
        const state = time => evaluate(`(() => { const b = __cvd.page.bars.find(b => b.time === ${time}); const s = __cvd.seriesData().flatMap(x => x.data).filter(p => p.time === ${time}); return { base: b?.cvd_usdt, indicator: __cvd.value('cvd/cvd', 'source', ${time}) } })()`)
        const coherent = s => s.indicator === undefined || s.indicator === s.base
        const m = await measure(async () => {
            const x = {}
            const at = dataset.length - 1, orig = { ...dataset[at] }, t = orig.time
            // live proof at rev R
            const R = ++baseRev
            base(orig, R); frame(R, await realOutput(at, dataset.slice())); await settle()
            x.live = await state(t)
            // same tuple HTTP overlap keeps live
            await evaluate('__cvd.resync()'); await settle()
            x.same = await state(t)
            // revised canonical tuple accepted from history (parent repro)
            dataset[at] = { ...orig, cvd_usdt: orig.cvd_usdt + 100 }
            await evaluate('__cvd.resync()'); await settle()
            x.revised = await state(t); x.revised_expected = orig.cvd_usdt + 100
            // delayed stale live base + result (rev <= superseded) cannot revive
            base(orig, R); frame(R, await realOutput(at, [...dataset.slice(0, at), orig])); await settle()
            x.delayed = await state(t)
            // output-before-base: newer result pending until its base displays
            const newer = { ...orig, cvd_usdt: orig.cvd_usdt + 300 }
            const R2 = ++baseRev
            frame(R2, await realOutput(at, [...dataset.slice(0, at), newer])); await settle()
            x.out_before_base = await state(t)
            base(newer, R2); await settle()
            x.restored = await state(t); x.restored_expected = newer.cvd_usdt
            // history response after newer live (dataset still +100 tuple)
            await evaluate('__cvd.resync()'); await settle()
            x.late_history = await state(t)
            // metadata-only: same-value result before base, then identical base at newer rev
            const shownBar = await evaluate(`({ ...__cvd.page.bars.find(b => b.time === ${t}) })`)
            const R3 = ++baseRev
            frame(R3, await realOutput(at, [...dataset.slice(0, at), shownBar])); await settle()
            const rendered = () => evaluate(`__cvd.seriesHandle('cvd/cvd').data().some(p => p.time === ${t} && p.value === ${shownBar.cvd_usdt})`)
            x.meta_pending = await state(t)
            base(shownBar, R3); await settle()
            x.meta_after = await state(t); x.meta_expected = shownBar.cvd_usdt; x.meta_rendered = await rendered()
            // ORDINARY live -> future result -> base (no historical supersede):
            // as_of_rev above every observed base rev stays pending.
            const shownVal = v => evaluate(`__cvd.seriesData().flatMap(x => x.data).filter(p => p.time === ${t}).map(p => p.value)`)
            const fut = { ...shownBar, cvd_usdt: shownBar.cvd_usdt + 500 }
            const R4 = ++baseRev
            frame(R4, await realOutput(at, [...dataset.slice(0, at), fut])); await settle()
            x.future_pending = await state(t); x.future_rendered = await shownVal(t)
            base(fut, R4); await settle()
            x.future_after = await state(t); x.future_expected = fut.cvd_usdt; x.future_rendered_after = await shownVal(t)
            // same again while HIDDEN: zero-write hide, repaint on restore
            const fut2 = { ...fut, cvd_usdt: fut.cvd_usdt + 50 }
            const R5 = ++baseRev
            await evaluate('__cvd.setHidden(true)')
            frame(R5, await realOutput(at, [...dataset.slice(0, at), fut2])); await settle()
            x.hidden_pending = await state(t)
            base(fut2, R5); await settle()
            await evaluate('__cvd.setHidden(false)'); await settle()
            x.hidden_after = await state(t); x.hidden_expected = fut2.cvd_usdt; x.hidden_rendered = await shownVal(t)
            dataset[at] = orig
            return x
        })
        const x = m.extra
        return { m, checks: {
            live_coherent: x.live.indicator === x.live.base && Number.isFinite(x.live.base),
            same_tuple_keeps_live: x.same.indicator === x.same.base && x.same.base === x.live.base,
            revised_tuple_coherent: x.revised.base === x.revised_expected && x.revised.indicator === x.revised_expected,
            delayed_stale_rejected: x.delayed.base === x.revised_expected && coherent(x.delayed),
            output_before_base_pending: x.out_before_base.base === x.revised_expected && coherent(x.out_before_base),
            matching_base_restores: x.restored.base === x.restored_expected && x.restored.indicator === x.restored_expected,
            late_history_coherent: coherent(x.late_history) && x.late_history.indicator !== undefined,
            future_result_pending: x.future_pending.indicator === undefined && x.future_pending.base === x.meta_expected && !x.future_rendered.includes(x.future_expected),
            future_matching_base_repaints: x.future_after.base === x.future_expected && x.future_after.indicator === x.future_expected && x.future_rendered_after.includes(x.future_expected),
            hidden_future_pending: x.hidden_pending.indicator === undefined,
            hidden_restore_coherent: x.hidden_after.base === x.hidden_expected && x.hidden_after.indicator === x.hidden_expected && x.hidden_rendered.includes(x.hidden_expected),
            metadata_only_base_repaints: x.meta_pending.indicator === undefined && x.meta_after.indicator === x.meta_expected && x.meta_rendered,
        }, extra: x }
    },
    async discovery_states() {
        // starting -> ready, failed (error) and a missing/unknown plugin id.
        const id = runners[0].descriptor.id
        const entry = discovery.indicators.find(i => i.id === id)
        const nav = async () => {
            await call('Page.navigate', { url: `http://127.0.0.1:${PORT}/` })
            await waitFor(`!!globalThis.__cvd && __cvd.page.bars.length > 0 && document.getElementById('status').textContent === 'live'`)
            await frames(3)
        }
        // Navigation replaces the page, so the RAF recorder of measure() cannot
        // span this scenario; collect state directly.
        const m = { calls: {}, requests: [], setBarsMs: [], longTasks: [], frames: [], coverageMisses: 0, wallMs: 0, input: {}, extra: await (async () => {
            const x = {}
            control.discovery = { ...discovery, indicators: discovery.indicators.map(i => i.id === id ? { id, status: 'starting', modes: i.modes, version: null, descriptor: null, error: null, restarts: 0 } : i) }
            await nav()
            x.starting = await evaluate('__cvd.registry()')
            x.startingPanes = await evaluate('__cvd.chart.panes().length')
            control.discovery = null
            await waitFor(`__cvd.registry().plugins.includes(${JSON.stringify(id)})`, 15000)
            await sleep(500); await frames(3)
            x.readyPanes = await evaluate('__cvd.chart.panes().length')
            x.readyValue = await evaluate(`__cvd.value(${JSON.stringify(`${id}/${entry.descriptor.outputs.find(o => !o.readout_only).key}`)}, 'source', __cvd.page.bars.at(-1).time)`)
            control.discovery = { ...discovery, indicators: discovery.indicators.map(i => i.id === id ? { ...i, status: 'failed', descriptor: null, error: { code: 'load_failed', message: 'x' } } : i) }
            await nav()
            x.failed = await evaluate('__cvd.registry()')
            x.failedPanes = await evaluate('__cvd.chart.panes().length')
            control.discovery = { ...discovery, indicators: discovery.indicators.filter(i => i.id !== id) }
            await nav()
            for (const s of sockets) wsSend(s, JSON.stringify({ type: 'indicator', api_version: 1, id: 'not.installed', version: '1', mode: 'source', generation: liveIndicator.generation, session: 1, seq: 0, as_of_rev: 0, output: { times: [], series: {} } }))
            await sleep(300); await frames(3)
            x.missing = await evaluate('__cvd.registry()')
            x.missingStatus = await evaluate(`document.getElementById('status').textContent`)
            control.discovery = null
            return x
        })() }
        const x = m.extra
        return { m, checks: {
            starting_pending: x.starting.pending.some(p => p.id === id && p.status === 'starting') && !x.starting.plugins.includes(id),
            starting_to_ready: x.readyPanes > x.startingPanes && x.readyPanes === EXPECTED_PANES && Number.isFinite(x.readyValue),
            failed_pending_price_kept: x.failed.pending.some(p => p.id === id && p.status === 'failed') && x.failedPanes < EXPECTED_PANES && x.failedPanes >= 1,
            missing_plugin_harmless: !x.missing.plugins.includes(id) && !x.missing.plugins.includes('not.installed') && x.missingStatus === 'live',
        } }
    },
    async indicator_menu() {
        // Compact indicator multiselect, driven ONLY by trusted CDP pointer
        // and key input. Rows for 'cvd'/'flow' etc. come from REAL public
        // plugin descriptors validated by the shipped host; rows named synthetic.* are
        // SYNTHETIC discovery entries (pending/failed/overflow) served by the
        // loopback /indicators fixture and never computed.
        const shots = path.join(OUT, 'indicator-menu')
        fs.mkdirSync(shots, { recursive: true })
        const shot = async name => { const r = await call('Page.captureScreenshot', { format: 'png' }); fs.writeFileSync(path.join(shots, name + '.png'), Buffer.from(r.data, 'base64')); return name + '.png' }
        const failures = []
        // Checks whose fixture was the retired SERVER discovery (reordered/pending/failed/error rows, 40 synthetic
        // descriptor-less rows): no such rows can exist for local packages. Recorded as retired, never as passed.
        const RETIRED_CHECKS = /^(kb_reordered_uses_rendered_order|kb_all_disabled_leaves_trigger|kb_empty_leaves_trigger|pending_failed_rows_disabled|error_empty_understandable|.*_scrollable_bounded)$/
        const check = (name, ok, detail) => { if (RETIRED_CHECKS.test(name)) { (x.details.retired ??= []).push(name); return } x.checks[name] = !!ok; if (!ok) failures.push(name + ' ' + JSON.stringify(detail ?? null).slice(0, 400)) }
        const x = { checks: {}, screenshots: [], details: {} }
        const S = `document.querySelector('#plugin-controls > button')`
        const state = () => evaluate(`(() => { const t = ${S}, p = document.getElementById('plugin-menu'), r = p.getBoundingClientRect()
            return { label: t.textContent.trim(), expanded: t.getAttribute('aria-expanded'), controls: t.getAttribute('aria-controls'), type: t.type,
                open: !p.hidden, rect: { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height },
                vw: innerWidth, vh: innerHeight, active: document.activeElement === t ? 'trigger' : (document.activeElement?.closest?.('[data-plugin]')?.dataset.plugin ?? document.activeElement?.tagName),
                rows: [...p.querySelectorAll('[data-plugin]')].map(l => ({ id: l.dataset.plugin, checked: l.querySelector('input').checked, disabled: l.querySelector('input').disabled, name: l.querySelector('.plugin-menu-name').textContent, meta: l.querySelector('.plugin-menu-meta').textContent, title: l.title })),
                empty: p.querySelector('[data-empty]')?.textContent ?? null, inBar: !!t.closest('.bar'), popupInBar: !!p.closest('.bar') } })()`)
        const center = async selector => evaluate(`(() => { const e = ${selector}; e.scrollIntoView({ block: 'nearest', inline: 'nearest' }); const r = e.getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 } })()`)
        const click = async (selector) => {
            const { x: cx, y: cy } = await center(selector)
            const hit = await evaluate(`(() => { const e = ${selector}, h = document.elementFromPoint(${cx}, ${cy}); return !!h && (e === h || e.contains(h) || (e.labels && [...e.labels].some(l => l.contains(h)))) })()`)
            if (!hit) throw new Error('trusted click target obscured: ' + selector)
            await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: cx, y: cy })
            await call('Input.dispatchMouseEvent', { type: 'mousePressed', x: cx, y: cy, button: 'left', buttons: 1, clickCount: 1 })
            await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x: cx, y: cy, button: 'left', buttons: 0, clickCount: 1 })
            await frames(2)
        }
        const key = async (k, code, vk, text, modifiers = 0) => {
            await call('Input.dispatchKeyEvent', { type: 'keyDown', key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers, ...(text ? { text } : {}) })
            await call('Input.dispatchKeyEvent', { type: 'keyUp', key: k, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers })
            await frames(2)
        }
        const row = id => `document.querySelector('#plugin-menu [data-plugin="${id}"] input')`
        const realIds = discovery.indicators.map(e => e.id)
        const trusted = await evaluate(`(() => { globalThis.__menuTrusted = { n: 0, untrusted: 0 }; for (const t of ['pointerdown', 'click', 'keydown', 'change']) document.addEventListener(t, e => { __menuTrusted.n++; if (!e.isTrusted) __menuTrusted.untrusted++ }, true); return true })()`)

        // 1. default closed + count of enabled REAL plugins
        let s0 = await state()
        const enabled0 = s0.rows.filter(r => r.checked && !r.disabled).length
        check('default_closed', !s0.open && s0.expanded === 'false', s0)
        check('trigger_semantics', s0.type === 'button' && s0.controls === 'plugin-menu' && s0.inBar && !s0.popupInBar, s0)
        check('default_count', s0.label.includes(`Indicators · ${enabled0}`) && enabled0 === realIds.length, { label: s0.label, enabled0, realIds })
        check('real_rows_are_real_plugins', JSON.stringify(s0.rows.map(r => r.id)) === JSON.stringify(realIds) && s0.rows.every(r => r.title.includes(discovery.indicators.find(e => e.id === r.id).descriptor.version)), s0.rows)
        // Disclosure semantics: no menu/haspopup claims; labelled checkbox group.
        const sem = await evaluate(`(() => { const t = ${S}, p = document.getElementById('plugin-menu'), g = p.querySelector('[role="group"]')
            return { haspopup: t.getAttribute('aria-haspopup'), title: t.title, menuRoles: p.querySelectorAll('[role^="menu"]').length + (t.getAttribute('role') ? 1 : 0),
                group: !!g, groupLabel: g && document.getElementById(g.getAttribute('aria-labelledby'))?.textContent, boxes: p.querySelectorAll('input[type=checkbox]').length } })()`)
        check('disclosure_semantics', sem.haspopup === null && sem.menuRoles === 0 && sem.group && sem.groupLabel === 'Indicators' && sem.boxes === realIds.length, sem)
        check('tooltip_says_selected', /indicators selected/.test(sem.title) && !/shown/.test(sem.title), sem.title)
        // NORMAL state with REAL ready plugin rows only (before any synthetic rows).
        for (const scheme of ['dark', 'light']) {
            await call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme }] }); await frames(4)
            await shot(`normal-closed-${scheme}`); x.screenshots.push(`normal-closed-${scheme}.png`)
            await click(S); const no = await state()
            check(`normal_open_real_rows_${scheme}`, no.open && JSON.stringify(no.rows.map(r => r.id)) === JSON.stringify(realIds) && no.rows.every(r => !r.disabled && r.checked), no.rows)
            await shot(`normal-open-${scheme}`); x.screenshots.push(`normal-open-${scheme}.png`)
            await click(S)
        }
        await call('Emulation.setEmulatedMedia', { features: [] }); await frames(2)

        // 2. repeated open/close with trusted clicks
        let cycles = 0
        for (let i = 0; i < 3; i++) {
            await click(S); const a = await state()
            await click(S); const b = await state()
            if (a.open && a.expanded === 'true' && !b.open && b.expanded === 'false') cycles++
        }
        check('repeated_open_close', cycles === 3, cycles)

        // 3. multiple selections while open: no reload/WS/viewport change
        const m = await measure(async () => {
            await click(S)
            const before = { time: await evaluate('__cvd.timeRange()'), logical: await evaluate('__cvd.range()'), requests: control.requests.length, ws: control.wsUpgrades, sockets: sockets.size }
            const visBefore = Object.fromEntries(await Promise.all(realIds.map(async id => [id, await evaluate(`__cvd.pluginSeries(${JSON.stringify(id)})`)])))
            for (const id of realIds) await click(row(id))
            const mid = await state()
            const visOff = Object.fromEntries(await Promise.all(realIds.map(async id => [id, await evaluate(`__cvd.pluginSeries(${JSON.stringify(id)})`)])))
            await click(row(realIds[0]))
            const after = await state()
            await evaluate('__cvd.settle()') // re-enable recomputes in the plugin Worker (async)
            const visOn = await evaluate(`__cvd.pluginSeries(${JSON.stringify(realIds[0])})`)
            await sleep(400); await frames(4)
            const end = { time: await evaluate('__cvd.timeRange()'), logical: await evaluate('__cvd.range()'), requests: control.requests.length, ws: control.wsUpgrades, sockets: sockets.size }
            return { before, end, mid, after, visBefore, visOff, visOn }
        })
        const t = m.extra
        check('stays_open_multi_select', t.mid.open && t.after.open, { mid: t.mid.open, after: t.after.open })
        check('all_unchecked_count_zero', t.mid.rows.every(r => !r.checked) && t.mid.label.includes('Indicators · 0'), t.mid)
        check('recheck_count_one', t.after.label.includes('Indicators · 1') && t.after.rows.find(r => r.id === realIds[0]).checked, t.after.label)
        check('real_series_visible_before', realIds.every(id => t.visBefore[id].some(s => s.visible && s.points > 0)), t.visBefore)
        check('real_series_hidden_after_uncheck', realIds.every(id => t.visOff[id].length && t.visOff[id].every(s => !s.visible)), t.visOff)
        check('real_series_visible_after_recheck', t.visOn.some(s => s.visible && s.points > 0), t.visOn)
        check('time_range_unchanged', JSON.stringify(t.before.time) === JSON.stringify(t.end.time) && JSON.stringify(t.before.logical) === JSON.stringify(t.end.logical) && !(m.calls.setVisibleLogicalRange > 0), { before: t.before, end: t.end, calls: m.calls })
        check('no_history_or_ws_activity', t.end.requests === t.before.requests && t.end.ws === t.before.ws && t.end.sockets === t.before.sockets && m.requests.length === 0, { before: t.before, end: t.end })

        // 4. outside click closes; Escape closes and returns focus
        await click(`document.getElementById('chart')`)
        const outside = await state()
        check('outside_click_closes', !outside.open && outside.expanded === 'false', outside)
        await click(S)
        await click(row(realIds[1] ?? realIds[0])) // focus inside popup (toggles that row; reverted below)
        await key('Escape', 'Escape', 27)
        const esc = await state()
        check('escape_closes_focus_trigger', !esc.open && esc.active === 'trigger', esc)
        // keyboard: Space on trigger opens, Tab enters rows, Space toggles, stays open
        await key(' ', 'Space', 32, ' ')
        const spaceOpen = await state()
        await key('Tab', 'Tab', 9)
        const tabbed = await state()
        await key(' ', 'Space', 32, ' ')
        const spaced = await state()
        check('keyboard_space_tab', spaceOpen.open && tabbed.active === realIds[0] && spaced.open && spaced.rows[0].checked !== tabbed.rows[0].checked, { spaceOpen: spaceOpen.open, tabbed: tabbed.active, spaced: spaced.rows[0], before: tabbed.rows[0] })
        await key(' ', 'Space', 32, ' ') // restore
        await key('Escape', 'Escape', 27)
        await click(S); await click(row(realIds[1] ?? realIds[0])); await key('Escape', 'Escape', 27) // revert focus-click toggle

        // 4b. Keyboard traversal with trusted Tab / Shift+Tab. Neighbours are
        // computed from the live DOM (normal tab sequence around the trigger).
        const ACTIVE = `(() => { const a = document.activeElement; return a === ${S} ? 'trigger' : a?.closest?.('#plugin-menu [data-plugin]')?.dataset.plugin ?? (a?.id || a?.tagName) })()`
        const nbr = await evaluate(`(() => { const all = [...document.querySelectorAll('button, input, select, textarea, a[href], [tabindex]')].filter(e => !e.disabled && e.tabIndex >= 0 && !e.closest('#plugin-menu') && e.getClientRects().length); const i = all.indexOf(${S}); const id = e => e ? (e.id || e.tagName) : null; return { prev: id(all[i - 1]), next: id(all[i + 1]) } })()`)
        const tab = (shift = false) => key('Tab', 'Tab', 9, null, shift ? 8 : 0)
        const focusTrigger = () => evaluate(`${S}.focus()`) // focus only; open/close stays trusted
        const travel = async () => {
            const steps = []
            await focusTrigger(); await key(' ', 'Space', 32, ' ')
            steps.push({ open: (await state()).open, at: await evaluate(ACTIVE) })
            for (let i = 0; i < 12; i++) { await tab(); const st = await state(); steps.push({ open: st.open, at: await evaluate(ACTIVE) }); if (!st.open) break }
            return steps
        }
        const kbm = await measure(async () => {
            const before = { time: await evaluate('__cvd.timeRange()'), logical: await evaluate('__cvd.range()'), requests: control.requests.length, ws: control.wsUpgrades }
            const r = {}
            const enabledIds = (await state()).rows.filter(q => !q.disabled).map(q => q.id)
            r.forward = await travel()
            // backward: into first row, Shift+Tab -> trigger (open), Shift+Tab -> prev control (closed)
            await focusTrigger(); await key(' ', 'Space', 32, ' '); await tab()
            const b0 = await evaluate(ACTIVE); await tab(true)
            const b1 = { at: await evaluate(ACTIVE), open: (await state()).open }; await tab(true)
            const b2 = { at: await evaluate(ACTIVE), open: (await state()).open }
            r.backward = { b0, b1, b2 }
            // pure keyboard traversal ends here: no fetch/WS/range change allowed
            r.end = { time: await evaluate('__cvd.timeRange()'), logical: await evaluate('__cvd.range()'), requests: control.requests.length, ws: control.wsUpgrades }
            // (the discovery fixture swaps below deliberately refetch /indicators; excluded from the no-reload check)
            // reordered discovery (real rows reversed): first/last follow rendered order
            control.discovery = { ...discovery, indicators: [...discovery.indicators].reverse() }
            await evaluate('__cvd.loadIndicators()'); await frames(3)
            r.reorderedRows = (await state()).rows.map(q => q.id)
            r.reordered = await travel()
            // all-disabled (synthetic pending only) and empty (discovery error)
            control.discovery = { api_version: 1, indicators: [{ id: 'synthetic.pending', status: 'starting', descriptor: null, error: null }] }
            await evaluate('__cvd.loadIndicators()'); await frames(3)
            r.allDisabled = await travel()
            control.discovery = 'error'
            await evaluate('__cvd.loadIndicators()'); await frames(3)
            r.empty = await travel()
            control.discovery = null
            await evaluate('__cvd.loadIndicators()'); await frames(3)
            r.enabledIds = enabledIds
            r.afterFixtures = { time: await evaluate('__cvd.timeRange()'), logical: await evaluate('__cvd.range()'), requests: control.requests.length, ws: control.wsUpgrades }
            r.before = before
            return r
        })
        const kb = kbm.extra
        x.details.keyboard = { neighbours: nbr, ...kb }
        const path_ = steps => steps.map(q => q.at)
        check('kb_neighbours_known', !!nbr.prev && !!nbr.next, nbr)
        check('kb_forward_rows_then_next_control', JSON.stringify(path_(kb.forward).filter((v, i, a) => v !== a[i - 1])) === JSON.stringify(['trigger', ...kb.enabledIds, 'BUTTON', nbr.next]) /* each row: checkbox + its Remove/Params actions (same row id), then Import .wasm, then next control */ && kb.forward[0].open && !kb.forward.at(-1).open && kb.forward.slice(0, -1).every(q => q.open), kb.forward)
        check('kb_backward_first_to_trigger_to_prev', kb.backward.b0 === kb.enabledIds[0] && kb.backward.b1.at === 'trigger' && kb.backward.b1.open && kb.backward.b2.at === nbr.prev && !kb.backward.b2.open, kb.backward)
        check('kb_reordered_uses_rendered_order', JSON.stringify(kb.reorderedRows) === JSON.stringify([...realIds].reverse()) && JSON.stringify(path_(kb.reordered)) === JSON.stringify(['trigger', ...[...realIds].reverse(), nbr.next]) && !kb.reordered.at(-1).open, { rows: kb.reorderedRows, path: kb.reordered })
        check('kb_all_disabled_leaves_trigger', JSON.stringify(path_(kb.allDisabled)) === JSON.stringify(['trigger', nbr.next]) && kb.allDisabled[0].open && !kb.allDisabled.at(-1).open, kb.allDisabled)
        check('kb_empty_leaves_trigger', JSON.stringify(path_(kb.empty)) === JSON.stringify(['trigger', nbr.next]) && kb.empty[0].open && !kb.empty.at(-1).open, kb.empty)
        check('kb_no_reload_range_kept', JSON.stringify(kb.before.time) === JSON.stringify(kb.end.time) && JSON.stringify(kb.before.logical) === JSON.stringify(kb.end.logical) && kb.end.requests === kb.before.requests && kb.end.ws === kb.before.ws, { before: kb.before, end: kb.end })
        check('kb_fixture_swaps_keep_range', JSON.stringify(kb.before.time) === JSON.stringify(kb.afterFixtures.time) && !(kbm.calls.setVisibleLogicalRange > 0) && kb.afterFixtures.ws === kb.before.ws, { calls: kbm.calls, afterFixtures: kb.afterFixtures })

        // 5. persistence across reload (prefs as left: only realIds[0] on)
        const expected = (await state()).rows.map(r => [r.id, r.checked])
        await evaluate(`sessionStorage.setItem('cvd_keep_prefs', '1')`)
        // Bars/live do NOT imply the asynchronous local WASM restore (host +
        // coordinator fetch, IndexedDB records, worker load, recompute) has
        // published. Wait semantically, BOUNDED, for genuine output of every
        // persisted-enabled plugin; never re-enable/click (prefs stay as left).
        const onIds = expected.filter(e => e[1]).map(e => e[0])
        const RESTORE_BOUND_MS = 20000
        const seriesNow = async () => Object.fromEntries(await Promise.all(expected.map(async ([id]) => [id, await evaluate(`__cvd.pluginSeries(${JSON.stringify(id)})`)])))
        const reloadKeepingPrefs = async assetFault => {
            control.assetFault = assetFault
            await call('Page.reload', {})
            await waitFor(`!!globalThis.__cvd && __cvd.page.bars.length > 0 && document.getElementById('status').textContent === 'live'`)
            await frames(4)
            const early = await seriesNow()
            const pushes0 = control.livePushes, reqs0 = control.requests.length
            const t0 = Date.now()
            let restored = true
            try { await waitFor(`(() => { const c = __cvd.localCompute(); return !!c && c.idle() && ${JSON.stringify(onIds)}.every(id => __cvd.pluginSeries(id).some(s => s.visible && s.points > 0)) })()`, RESTORE_BOUND_MS) } catch { restored = false }
            const waitedMs = Date.now() - t0
            // No new market data (live push / history request) may rescue the fill.
            const quiet = { livePushes: control.livePushes - pushes0, requests: control.requests.slice(reqs0) }
            await frames(2)
            return { early, restored, waitedMs, quiet, noNewMarket: quiet.livePushes === 0 && quiet.requests.length === 0 }
        }
        const main = await reloadKeepingPrefs(null)
        const reloaded = await state()
        check('prefs_survive_reload', JSON.stringify(reloaded.rows.map(r => [r.id, r.checked])) === JSON.stringify(expected) && !reloaded.open && reloaded.label.includes(`Indicators · ${expected.filter(e => e[1]).length}`), { expected, got: reloaded.rows })
        // Actual series state follows the persisted checkbox state per plugin.
        const afterReload = await seriesNow()
        const seriesMatch = after => expected.every(([id, on]) => on ? after[id].some(s => s.visible && s.points > 0) : after[id].every(s => !s.visible))
        check('reload_series_match_prefs', main.restored && main.noNewMarket && expected.some(e => !e[1]) && expected.every(([id, on]) => on ? afterReload[id].some(s => s.visible && s.points > 0) : afterReload[id].every(s => !s.visible)), { expected, afterReload, restored: main.restored, waitedMs: main.waitedMs, quiet: main.quiet })
        // Regression control (CI run 41013584705): a deterministically DELAYED
        // plugin Worker script (fetched only after bars/live) reproduces the observed live-but-0-points snapshot;
        // the enabled plugin must then fill on the same page with no input.
        const WORKER = '/wasm-host/plugin-worker.js'
        const delayed = await reloadKeepingPrefs({ path: WORKER, delayMs: 2500 })
        const delayedAfter = await seriesNow()
        const delayedRows = (await state()).rows.map(r => [r.id, r.checked])
        check('reload_delayed_restore_fills', onIds.length > 0 && onIds.every(id => delayed.early[id].length > 0 && delayed.early[id].some(s => s.visible) && delayed.early[id].every(s => s.points === 0)) && delayed.restored && delayed.noNewMarket && JSON.stringify(delayedRows) === JSON.stringify(expected) && expected.every(([id, on]) => on ? delayedAfter[id].some(s => s.visible && s.points > 0) : delayedAfter[id].every(s => !s.visible)), { early: delayed.early, after: delayedAfter, restored: delayed.restored, waitedMs: delayed.waitedMs, quiet: delayed.quiet })
        // Negative control: a permanently failed restore must NOT pass the
        // bounded wait (no zero-point acceptance, no unbounded hang).
        const broken = await reloadKeepingPrefs({ path: WORKER, status: 404 })
        check('reload_failed_restore_bounded_red', !broken.restored && broken.waitedMs >= RESTORE_BOUND_MS && broken.waitedMs < RESTORE_BOUND_MS + 5000, { restored: broken.restored, waitedMs: broken.waitedMs, early: broken.early })
        // Healthy reload (same persisted prefs) for the steps below.
        const healed = await reloadKeepingPrefs(null)
        const healedAfter = await seriesNow()
        const healedRows = (await state()).rows.map(r => [r.id, r.checked])
        check('reload_healthy_after_controls', healed.restored && healed.noNewMarket && JSON.stringify(healedRows) === JSON.stringify(expected) && seriesMatch(healedAfter), { waitedMs: healed.waitedMs, rows: healedRows, after: healedAfter, quiet: healed.quiet })
        x.details.reload = { boundMs: RESTORE_BOUND_MS, main, delayed: { ...delayed, after: delayedAfter }, broken, healed: { ...healed, after: healedAfter } }
        await evaluate(`sessionStorage.removeItem('cvd_keep_prefs')`)
        await evaluate(`__menuTrusted = { n: 0, untrusted: 0 }; for (const t of ['pointerdown', 'click', 'keydown', 'change']) document.addEventListener(t, e => { __menuTrusted.n++; if (!e.isTrusted) __menuTrusted.untrusted++ }, true)`)

        // 6. pending / failed / error / recovered while open, focus preserved
        await click(S)
        await click(row(realIds[0])); await click(row(realIds[0])) // focus a real row, state unchanged
        const real = discovery.indicators
        control.discovery = { api_version: 1, indicators: [...real,
            { id: 'synthetic.pending', status: 'starting', descriptor: null, error: null },
            { id: 'synthetic.failed', status: 'failed', descriptor: null, error: { code: 'incompatible', message: '/srv/private/lib.so' } }] }
        await evaluate('__cvd.loadIndicators()'); await frames(3)
        const pend = await state()
        check('pending_failed_rows_disabled', ['synthetic.pending', 'synthetic.failed'].every(id => pend.rows.find(r => r.id === id)?.disabled) && /starting/.test(pend.rows.find(r => r.id === 'synthetic.pending').meta) && /failed incompatible/.test(pend.rows.find(r => r.id === 'synthetic.failed').meta) && !JSON.stringify(pend.rows).includes('/srv/private'), pend.rows)
        check('rerender_keeps_open_and_focus', pend.open && pend.active === realIds[0], { open: pend.open, active: pend.active })
        await shot('pending-failed-open'); x.screenshots.push('pending-failed-open.png')
        control.discovery = 'error'
        await evaluate('__cvd.loadIndicators()'); await frames(3)
        const err = await state()
        check('error_empty_understandable', err.open && err.rows.length === 0 && /No indicators available/.test(err.empty ?? '') && err.label.includes('Indicators · 0'), err)
        await shot('discovery-error-open'); x.screenshots.push('discovery-error-open.png')
        control.discovery = null
        await evaluate('__cvd.loadIndicators()'); await frames(3)
        const rec = await state()
        check('recovered_rows_back_open', rec.open && JSON.stringify(rec.rows.map(r => r.id)) === JSON.stringify(realIds) && rec.empty === null, rec)
        await key('Escape', 'Escape', 27)
        const recClosed = await state()
        check('escape_closes_after_focused_row_removed', !recClosed.open && recClosed.active === 'trigger', recClosed.active)

        // 7. many / long SYNTHETIC rows: bounded + scrollable, per viewport/theme
        control.discovery = { api_version: 1, indicators: [...real, ...Array.from({ length: 40 }, (_, i) => ({ id: `synthetic.overflow-long-descriptor-label-for-wrap-check-${String(i).padStart(2, '0')}`, status: 'starting', descriptor: null, error: null }))] }
        await evaluate('__cvd.loadIndicators()'); await frames(3)
        const views = [['desktop', 1440, 900], ['narrow', 390, 844], ['short', 900, 420]]
        for (const [vname, w, h] of views) {
            await call('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false })
            await frames(4)
            for (const scheme of ['dark', 'light']) {
                await call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme }] })
                await frames(4)
                await click(S)
                const st = await state()
                const geo = await evaluate(`(() => { const p = document.getElementById('plugin-menu'), l = p.querySelector('.plugin-menu-list'), r = p.getBoundingClientRect()
                    const hit = document.elementFromPoint(r.left + r.width / 2, r.top + Math.min(r.height - 4, 30))
                    const lum = c => { const v = c.match(/[\\d.]+/g).slice(0, 3).map(Number).map(n => { n /= 255; return n <= 0.03928 ? n / 12.92 : ((n + 0.055) / 1.055) ** 2.4 }); return 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2] }
                    const fg = getComputedStyle(p.querySelector('.plugin-menu-name')).color, bg = getComputedStyle(p).backgroundColor
                    const [a, b] = [lum(fg), lum(bg)].sort((m, n) => n - m)
                    return { scrollable: l.scrollHeight > l.clientHeight, onTop: p.contains(hit), contrast: (a + 0.05) / (b + 0.05), fg, bg,
                        barOverflow: getComputedStyle(document.querySelector('.bar')).overflow, barMax: getComputedStyle(document.querySelector('.bar')).maxHeight } })()`)
                const inView = st.rect.left >= 0 && st.rect.top >= 0 && st.rect.right <= st.vw && st.rect.bottom <= st.vh && st.rect.height > 60
                // trusted wheel scroll inside the list
                const lc = await evaluate(`(() => { const r = document.querySelector('#plugin-menu .plugin-menu-list').getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 } })()`)
                for (let i = 0; i < 4; i++) await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: lc.x, y: lc.y, deltaX: 0, deltaY: 200 })
                await sleep(150); await frames(3)
                const scrolled = await evaluate(`document.querySelector('#plugin-menu .plugin-menu-list').scrollTop`)
                const name = `${vname}-${scheme}`
                x.details[name] = { rect: st.rect, vw: st.vw, vh: st.vh, ...geo, scrolled, open: st.open }
                check(`${name}_open_in_viewport`, st.open && inView, x.details[name])
                check(`${name}_not_clipped_on_top`, geo.onTop, x.details[name])
                check(`${name}_scrollable_bounded`, geo.scrollable && scrolled > 0, x.details[name])
                check(`${name}_legible_contrast`, geo.contrast >= 4.5, x.details[name])
                check(`${name}_toolbar_overflow_kept`, geo.barOverflow === 'auto' && geo.barMax !== 'none', x.details[name])
                await shot(name); x.screenshots.push(name + '.png')
                await key('Escape', 'Escape', 27)
                const closed = await state()
                check(`${name}_escape_closed`, !closed.open && closed.active === 'trigger', closed.active)
            }
        }
        // short viewport: toolbar scroll repositions the open popup
        await call('Emulation.setDeviceMetricsOverride', { width: 520, height: 360, deviceScaleFactor: 1, mobile: false })
        await call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })
        await frames(4)
        await click(S)
        const pre = await state()
        const scrollRes = await evaluate(`(() => { const b = document.querySelector('.bar'); const t = ${S}; const before = t.getBoundingClientRect().bottom; b.scrollTop = b.scrollHeight; return new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => r({ scrollable: b.scrollHeight > b.clientHeight, moved: t.getBoundingClientRect().bottom - before })))) })()`)
        const post = await state()
        x.details.toolbar_scroll = { pre: pre.rect, post: post.rect, ...scrollRes }
        check('toolbar_scroll_reposition_in_viewport', post.open && post.rect.top >= 0 && post.rect.bottom <= post.vh && (scrollRes.moved === 0 || Math.abs(post.rect.top - pre.rect.top) > 0 || Math.abs(post.rect.bottom - pre.rect.bottom) > 0), x.details.toolbar_scroll)
        await shot('short-toolbar-scrolled'); x.screenshots.push('short-toolbar-scrolled.png')
        await key('Escape', 'Escape', 27)
        // Visual viewport (Chromium pinch-zoom via trusted CDP gesture): the
        // open popup must lie inside the VISIBLE region incl. its offset.
        // Real browser coverage in headless Chromium only (not Safari/iOS).
        await call('Emulation.setDeviceMetricsOverride', { width: 1000, height: 700, deviceScaleFactor: 1, mobile: false })
        control.discovery = null; await evaluate('__cvd.loadIndicators()'); await frames(4)
        await click(S)
        const tr0 = await evaluate(`(() => { const r = ${S}.getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.bottom + 120 } })()`)
        // Zoom the visual viewport (page scale) and pan it with trusted wheel
        // input; headless Chromium ignores synthesized mouse pinch gestures.
        let pinch = null
        try { await call('Emulation.setPageScaleFactor', { pageScaleFactor: 2.2 }) } catch (e) { pinch = String(e) }
        await frames(4)
        for (let i = 0; i < 3; i++) await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: 300, y: 600, deltaX: 120, deltaY: 120 })
        await sleep(300); await frames(4)
        const vvr = await evaluate(`(() => { const v = visualViewport, r = document.getElementById('plugin-menu').getBoundingClientRect(); return { scale: v.scale, ox: v.offsetLeft, oy: v.offsetTop, w: v.width, h: v.height, open: !document.getElementById('plugin-menu').hidden, rect: { left: r.left, top: r.top, right: r.right, bottom: r.bottom } } })()`)
        x.details.visual_viewport = { pinchError: pinch, ...vvr }
        const inVisible = q => q.rect.left >= q.ox - 0.5 && q.rect.top >= q.oy - 0.5 && q.rect.right <= q.ox + q.w + 0.5 && q.rect.bottom <= q.oy + q.h + 0.5
        check('vv_pinch_zoom_applied', !pinch && vvr.scale > 1.5 && (vvr.ox > 0 || vvr.oy > 0), x.details.visual_viewport)
        check('vv_popup_inside_visible_region', vvr.open && inVisible(vvr), x.details.visual_viewport)
        await shot('visual-viewport-zoomed'); x.screenshots.push('visual-viewport-zoomed.png')
        await key('Escape', 'Escape', 27)
        await call('Emulation.resetPageScaleFactor', {}).catch(() => {}); await frames(3)
        await call('Emulation.setDeviceMetricsOverride', { width: VIEW.width, height: VIEW.height, deviceScaleFactor: VIEW.deviceScaleFactor, mobile: false })
        await call('Emulation.setEmulatedMedia', { features: [] })
        control.discovery = null
        const tr = await evaluate('__menuTrusted')
        check('all_menu_input_trusted', trusted && tr.n > 0 && tr.untrusted === 0, tr)
        x.failures = failures
        fs.writeFileSync(path.join(shots, 'indicator-menu-results.json'), JSON.stringify({ label: 'Headless Chromium (not Safari). Rows cvd/flow-style ids come from REAL public .wasm validated by the shipped host; synthetic.* rows are SYNTHETIC loopback discovery entries.', real_ids: realIds, ...x, toggle_measure: { requests: m.requests.length, calls: m.calls, before: t.before, end: t.end } }, null, 1))
        if (failures.length) console.log('indicator_menu failures:\n  ' + failures.join('\n  '))
        return { m, checks: x.checks }
    },
}

// ------------------------------------------------------------ stats
function summary(values) {
    const sorted = values.filter(Number.isFinite).sort((a, b) => a - b)
    const n = sorted.length
    if (!n) return { n: 0 }
    const q = p => sorted[Math.min(n - 1, Math.floor(p * (n - 1)))]
    const out = { n, median: q(0.5), max: sorted[n - 1] }
    // Only report tail percentiles with enough samples to mean something.
    if (n >= 100) out.p95 = q(0.95)
    if (n >= 1000) out.p99 = q(0.99)
    return out
}

let failed = false
const results = { label: 'SYNTHETIC loopback fixture, headless Chromium, real pinned LWC 5.1.0. RAF intervals are a frame-cadence proxy, NOT input-to-photon latency, FPS certification or Safari evidence.',
    static_root: SRC, profile: PROFILE, reps: REPS, viewport: VIEW, scenarios: {} }
const hard = setTimeout(() => { console.error('hard timeout'); chrome.kill('SIGKILL'); process.exit(3) }, Number(process.env.CVD_HARD_TIMEOUT_MIN || 90) * 60 * 1000)
try {
    const endpoint = await new Promise((resolve, reject) => {
        let buf = ''
        const timer = setTimeout(() => reject(new Error('Chromium endpoint timeout')), 15000)
        chrome.stderr.on('data', d => { buf += d; const m = buf.match(/DevTools listening on (ws:\/\/\S+)/); if (m) { clearTimeout(timer); resolve(m[1]) } })
        chrome.on('exit', c => reject(new Error('Chromium exited ' + c)))
    })
    const targets = await (await fetch(`http://127.0.0.1:${new URL(endpoint).port}/json/list`)).json()
    socket = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl)
    await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, { once: true }); socket.addEventListener('error', reject, { once: true }) })
    socket.addEventListener('message', event => {
        const msg = JSON.parse(event.data)
        if (msg.id) {
            const p = pending.get(msg.id)
            if (p) { clearTimeout(p.timer); pending.delete(msg.id); msg.error ? p.reject(new Error(JSON.stringify(msg.error))) : p.resolve(msg.result) }
        } else if (msg.method === 'Runtime.exceptionThrown') errors.push(msg.params.exceptionDetails)
        else if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') errors.push(msg.params.args.map(a => a.value ?? a.description).join(' '))
    })
    await call('Page.enable'); await call('Runtime.enable')
    await call('Emulation.setDeviceMetricsOverride', { width: VIEW.width, height: VIEW.height, deviceScaleFactor: VIEW.deviceScaleFactor, mobile: false })
    await call('Page.addScriptToEvaluateOnNewDocument', { source: `if (!sessionStorage.getItem('cvd_keep_prefs')) localStorage.setItem('cvd_prefs_v1', JSON.stringify({ exchange: 'binance', symbol: 'BTC', market: 'spot', tf: '1m', showRd: true, showKd: true, showFd: true, showHd: true, showLevels: true, showAgg: false, flowMode: 'delta' }))` })
    const unknown = [...(ONLY ?? [])].filter(name => !(name in scenarios))
    if (unknown.length) throw new Error(`unknown CVD_SCENARIOS: ${unknown.join(',')}`)
    // Scenarios whose subject was the SERVER indicator transport (typed WS
    // indicator frames, as_of_rev/session fences, /indicators discovery
    // states). The browser no longer consumes those envelopes, so they are
    // explicitly RETIRED (listed in results, never silently skipped). Their
    // invariants moved to the browser boundary: scope/revision/instance fences,
    // stale-result rejection, fault isolation and no-stale-paint are asserted by
    // tests/local-plugins.test.mjs and tests/local-wasm-browser.mjs.
    const RETIRED = { lifecycle_fences: 'server indicator frame fences', history_coherence: 'server history/live indicator coherence', discovery_states: 'server /indicators discovery states' }
    results.retired = RETIRED
    for (const [name, run] of Object.entries(scenarios)) {
        if (ONLY && !ONLY.has(name)) continue
        if (RETIRED[name]) { console.log(`${name}: RETIRED (${RETIRED[name]}; see B-browser.md)`); continue }
        const reps = []
        for (let rep = 0; rep < REPS; rep++) {
            await openPage()
            let m, checks
            try {
                ({ m, checks } = await run())
            } catch (error) {
                // A scenario that cannot run is a failure, never a skip.
                const state = await evaluate('({ range: __cvd.range(), window: __cvd.window, cached: __cvd.page.bars.length, reachedStart: __cvd.page.reachedStart, loadingOlder: __cvd.page.loadingOlder })').catch(e => String(e))
                m = { calls: {}, requests: control.requests.slice(), setBarsMs: [], longTasks: [], frames: [], coverageMisses: null, wallMs: null, input: {}, extra: { error: String(error), state } }
                checks = { ran: false }
                console.log(name, 'ERROR', String(error), JSON.stringify(state), JSON.stringify(control.requests.slice(-3)))
            }
            if (process.env.CVD_CAPTURE_SCENARIO === name && rep === 0) {
                const shot = await call('Page.captureScreenshot', { format: 'png' })
                fs.writeFileSync(path.join(OUT, `${name}.png`), Buffer.from(shot.data, 'base64'))
                const segmented = await evaluate('__cvd.segmentedInfo()')
                fs.writeFileSync(path.join(OUT, 'segmented-contract.json'), JSON.stringify(segmented, null, 2))
            }
            const ok = Object.values(checks).every(Boolean)
            if (!ok) failed = true
            reps.push({ checks, ok, calls: m.calls, requests: m.requests.length, setBarsMs: m.setBarsMs, longTasks: m.longTasks.map(t => t.duration),
                frames: m.frames, coverageMisses: m.coverageMisses, wallMs: m.wallMs, input: m.input, extra: m.extra })
            console.log(`${name} #${rep + 1}: ${ok ? 'ok' : 'FAIL ' + JSON.stringify(checks)} setData=${m.calls.setData || 0} setBars=${m.setBarsMs.length} longTasks=${m.longTasks.length} maxLong=${Math.max(0, ...m.longTasks.map(t => t.duration)).toFixed(1)}`)
        }
        results.scenarios[name] = {
            all_ok: reps.every(r => r.ok),
            frame_interval_ms: summary(reps.flatMap(r => r.frames)),
            long_task_ms: summary(reps.flatMap(r => r.longTasks)),
            long_tasks_per_rep: reps.map(r => r.longTasks.length),
            setBars_ms: summary(reps.flatMap(r => r.setBarsMs)),
            setData_per_rep: reps.map(r => r.calls.setData || 0),
            series_add_remove_per_rep: reps.map(r => (r.calls.addSeries || 0) + (r.calls.addCustomSeries || 0) + (r.calls.removeSeries || 0)),
            reps,
        }
        fs.writeFileSync(path.join(OUT, 'interaction-results.json'), JSON.stringify({ ...results, errors }, null, 1))
    }
    if (errors.length) { failed = true; console.log('browser errors:', JSON.stringify(errors).slice(0, 2000)) }
    for (const [name, s] of Object.entries(results.scenarios)) {
        console.log(`SUMMARY ${name}: ok=${s.all_ok} frame=${JSON.stringify(s.frame_interval_ms)} long=${JSON.stringify(s.long_task_ms)} setBars=${JSON.stringify(s.setBars_ms)} setData/rep=${s.setData_per_rep.join(',')}`)
    }
    console.log(failed ? 'FAILED' : 'COMPLETE', path.join(OUT, 'interaction-results.json'))
} finally {
    clearTimeout(hard)
    socket?.close()
    chrome.kill('SIGTERM')
    for (const s of sockets) s.destroy()
    server.close()
    for (const r of runners) r.close()
    await new Promise(r => { if (chrome.exitCode !== null) r(); else { chrome.once('exit', r); setTimeout(r, 5000) } })
    try { fs.rmSync(profileDir, { recursive: true, force: true, maxRetries: 5 }) } catch (error) { console.warn('profile cleanup:', error.message) }
}
process.exitCode = failed ? 1 : 0
