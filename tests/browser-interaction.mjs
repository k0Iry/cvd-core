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
//   CVD_INDICATOR_RUNNER  real cvd-indicator-runner binary (required)
//   CVD_INDICATOR_LIBS    comma-separated operator-owned cdylib paths (required)
//
// Everything is loopback: a synthetic /meta, /history and /ws fixture plus the
// served static files and the pinned vendored Lightweight Charts. Indicator
// results are REAL: every /history page and live tick is computed by the real
// public cdylibs under the real runner and served in the host envelopes
// (/indicators, history `indicators`, typed /ws `indicator` frames). Live
// frames are produced by stateless history requests over a warm-up window
// (the runner live-session path is covered by cvd-indicators Rust tests). No production
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
import { RunnerClient, inputBar } from './runner-client.mjs'

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
const control = { beforeDelayMs: 0, requests: [], discovery: null }

// ------------------------------------------------------------ real indicators
const RUNNER = process.env.CVD_INDICATOR_RUNNER
const LIBS = (process.env.CVD_INDICATOR_LIBS || '').split(',').filter(Boolean)
if (!RUNNER || !LIBS.length) throw new Error('CVD_INDICATOR_RUNNER and CVD_INDICATOR_LIBS are required (real runner + public cdylibs)')
const runners = []
for (const lib of LIBS) runners.push(await RunnerClient.spawn(RUNNER, lib))
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
    value: (key, mode, time) => indicatorStore.get(key, mode, time),
    faults: () => JSON.parse(JSON.stringify(pluginStatus)),
    registry: () => ({ plugins: registry.plugins.map(p => p.id), pending: registry.pending.map(p => ({ id: p.id, status: p.status })) }),
    resync: () => resyncIndicators(),
    epoch: () => indicatorStore.epoch,
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
const CACHE_TARGET = 16000
// Price + one pane per separate (indicator, pane_group) from real descriptors.
const EXPECTED_PANES = 1 + new Set(runners.flatMap(r => r.descriptor.outputs.filter(o => o.pane === 'separate' && !o.readout_only).map(o => `${r.descriptor.id}:${o.pane_group ?? o.key}`))).size
async function openPage() {
    resetDataset()
    control.beforeDelayMs = 0
    control.discovery = null
    control.requests.length = 0
    await call('Page.navigate', { url: `http://127.0.0.1:${PORT}/` })
    await waitFor(`!!globalThis.__cvd && __cvd.page.bars.length > 0 && document.getElementById('status').textContent === 'live'`)
    // Setup only: grow the cache through the real older-page path.
    for (let i = 0; i < 40 && (await evaluate('__cvd.page.bars.length')) < CACHE_TARGET; i++) {
        await sleep(320)
        await evaluate('__cvd.loadMoreBefore()')
    }
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
            await evaluate(`(() => { const e = __cvd.elements.aggToggle; e.checked = true; e.onchange(); return new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))) })()`)
            const agg = { ...(await flowState()), cvd: await evaluate(`__cvd.chart.panes()[1].getSeries()[0].data().at(-1).value`) }
            await evaluate(`(() => { const e = __cvd.elements.aggToggle; e.checked = false; e.onchange(); return new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))) })()`)
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
        // by the real public cdylib under the real runner for the exact bar.
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
        // cvd_usdt. Values come from the real cdylib under the real runner via
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
    await call('Page.addScriptToEvaluateOnNewDocument', { source: `localStorage.setItem('cvd_prefs_v1', JSON.stringify({ exchange: 'binance', symbol: 'BTC', market: 'spot', tf: '1m', showRd: true, showKd: true, showFd: true, showHd: true, showLevels: true, showAgg: false, flowMode: 'delta' }))` })
    const unknown = [...(ONLY ?? [])].filter(name => !(name in scenarios))
    if (unknown.length) throw new Error(`unknown CVD_SCENARIOS: ${unknown.join(',')}`)
    for (const [name, run] of Object.entries(scenarios)) {
        if (ONLY && !ONLY.has(name)) continue
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
