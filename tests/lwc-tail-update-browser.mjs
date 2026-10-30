#!/usr/bin/env node
// Real-Chromium regression for plugin renderers driven with the REAL vendored
// Lightweight Charts 5.1.0 + dual-range plugin, the way app.js
// refreshPluginOutputs/updatePluginPoint/syncPluginSeries do. Covers:
//  - incremental result whose first touched bar is the PENULTIMATE rendered
//    bar (live re-close / tail catch-up / history recompute after 15m->30m):
//    LWC's plain update() throws "Cannot update oldest data", which used to
//    escape into local-compute and fault the plugin as `trap`;
//  - live append, value->null and null->value at an older point;
//  - empty render -> update -> clear -> update -> clear occupancy lifecycle;
//  - clear (disable/reset) -> render (re-enable).
// Every step asserts EXACT time keys and values of every LWC series (both band
// edges). Stage of scripts/b-browser-gate.sh (`lwcrender`).
//   node tests/lwc-tail-update-browser.mjs
//   CVD_STATIC_ROOT=<dir>    serve an assembled static tree (core-app-gate)
//   CVD_RENDER_MODULE=<file>  load another plugin-render.mjs (RED control)
//   CVD_CHROMIUM=<binary>     browser (default /usr/bin/chromium)
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import http from 'node:http'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { teardown } from './harness-teardown.mjs'

const SRC = path.resolve(process.env.CVD_STATIC_ROOT || path.join(path.dirname(fileURLToPath(import.meta.url)), '../static'))
const RENDER = process.env.CVD_RENDER_MODULE ? path.resolve(process.env.CVD_RENDER_MODULE) : path.join(SRC, 'js/plugin-render.mjs')
const KINDS = 4, DEADLINE_MS = 60000, MAX_STDOUT = 1 << 20

const PAGE = `<!doctype html><html><body><div id=c style="width:600px;height:400px"></div>
<script type=importmap>{"imports":{"lightweight-charts":"/vendor/lightweight-charts-5.1.0/lightweight-charts.standalone.production.mjs",
"@tradingview/lwc-plugin-dual-range-histogram-series":"/vendor/lwc-plugin-dual-range-histogram-series-1.0.0/dual-range-histogram-series.standalone.js"}}</script>
<script type=module>
import * as LWC from "lightweight-charts"
import { createDualRangeHistogramSeries } from "@tradingview/lwc-plugin-dual-range-histogram-series"
import { createOutputRenderer } from "/render-under-test.mjs"
const out = []
const values = new Map()
const real = LWC.createChart(document.getElementById("c"), { width: 600, height: 400 })
let created = []   // every series the renderer under test creates, in order
const chart = new Proxy(real, { get(t, k) {
  if (k === "addSeries" || k === "addCustomSeries") return (...a) => { const s = t[k](...a); created.push(s); return s }
  const v = t[k]; return typeof v === "function" ? v.bind(t) : v } })
const deps = { LightweightCharts: LWC, outputValue: (bar, key) => values.get(key)?.get(bar.time),
  createDualRangeHistogramSeries: (c, o, p) => { const s = createDualRangeHistogramSeries(real, o, p); created.push(s); return s } }
const style = { color: "#fff", upColor: "#0f0", downColor: "#f00" }
const T = i => 1700000000 + i * 900
const val = (kind, x) => (kind === "dual_range" ? { upper: x, lower: -x } : x)
// Normalised LWC contents: [[time, value|null], ...] per created series.
const snap = () => created.map(s => s.data().map(p => [p.time, p.values ? [p.values[0], p.values[2]] : (p.value ?? null)]))
// LWC data() omits whitespace points, so a missing value must be ABSENT.
function expected(kind, paneId, bars, m) {
  const cell = v => (typeof v === "number" ? v : null)
  const keep = rows => rows.filter(([, v]) => v !== null)
  if (kind === "dual_range" && paneId === "price")   // band: two independent edges
    return [keep(bars.map(b => [b.time, cell(m.get(b.time)?.upper)])), keep(bars.map(b => [b.time, cell(m.get(b.time)?.lower)]))]
  if (kind === "dual_range") return [keep(bars.map(b => { const v = m.get(b.time); return [b.time, cell(v?.upper) !== null && cell(v?.lower) !== null ? [v.upper, v.lower] : null] }))]
  return [keep(bars.map(b => [b.time, cell(m.get(b.time))]))]
}
for (const [kind, paneId, key] of [["line", "main", "l"], ["histogram", "flow", "h"], ["dual_range", "flow", "d"], ["dual_range", "price", "b"]]) {
  const name = kind + "/" + paneId
  let step = "create"
  try {
    const check = (bars, m) => {
      const got = JSON.stringify(snap()), want = JSON.stringify(expected(kind, paneId, bars, m))
      if (got !== want) throw new Error("step " + step + " got " + got + " want " + want)
    }
    created = []
    const r = createOutputRenderer(chart, deps, { kind, paneId, key, style, pluginId: "p" }, paneId === "price" ? 0 : 1)
    const m = new Map(); values.set(key, m)
    let bars = [0, 1, 2, 3, 4].map(i => ({ time: T(i) })); bars.forEach((b, i) => m.set(b.time, val(kind, i + 1)))
    step = "render"; r.render(bars); check(bars, m)
    step = "touch-penultimate+last"; m.set(T(3), val(kind, 30)); m.set(T(4), val(kind, 40))
    for (const b of bars) if (b.time >= T(3)) r.update(b)
    check(bars, m)
    step = "append"; bars = [...bars, { time: T(5) }]; m.set(T(5), val(kind, 50)); r.update(bars[5]); check(bars, m)
    step = "reclose-penultimate"; m.set(T(4), val(kind, 41)); for (const b of bars) if (b.time >= T(4)) r.update(b)
    check(bars, m)
    step = "value-to-null"; m.delete(T(4)); for (const b of bars) if (b.time >= T(4)) r.update(b)
    check(bars, m)
    step = "null-to-value"; m.set(T(4), val(kind, 42)); for (const b of bars) if (b.time >= T(4)) r.update(b)
    check(bars, m)
    step = "one-edge-null"; m.set(T(4), kind === "dual_range" ? { upper: 7, lower: null } : 7); for (const b of bars) if (b.time >= T(4)) r.update(b)
    check(bars, m)
    step = "other-edge-null"; m.set(T(4), kind === "dual_range" ? { upper: null, lower: -8 } : 8); for (const b of bars) if (b.time >= T(4)) r.update(b)
    check(bars, m)
    step = "edges-restored"; m.set(T(4), val(kind, 43)); for (const b of bars) if (b.time >= T(4)) r.update(b)
    check(bars, m)
    step = "last-only-update"; m.set(T(5), val(kind, 51)); r.update(bars[5]); check(bars, m)
    step = "disable-clear"; r.clear(); check([], m)
    step = "re-enable-render"; r.render(bars); check(bars, m)
    step = "empty-render"; r.render([]); check([], m)
    step = "empty-append"; r.update(bars[0]); check([bars[0]], m)
    step = "empty-clear"; r.clear(); check([], m)
    step = "empty-append-again"; r.update(bars[0]); check([bars[0]], m)
    step = "final-clear"; r.clear(); check([], m)
    r.destroy()
    out.push("PASS " + name)
  } catch (e) { out.push("FAIL " + name + " " + String(e?.message ?? e).slice(0, 600)) }
}
document.body.dataset.result = JSON.stringify(out)
</script></body></html>`

const types = { '.mjs': 'text/javascript', '.js': 'text/javascript', '.html': 'text/html' }
const sockets = new Set()
const server = http.createServer((req, res) => {
  const p = new URL(req.url, 'http://x').pathname
  if (p === '/') { res.writeHead(200, { 'content-type': 'text/html' }); return res.end(PAGE) }
  const f = p === '/render-under-test.mjs' ? RENDER : path.join(SRC, path.normalize(p))
  if (f !== RENDER && !f.startsWith(SRC + path.sep)) { res.writeHead(404); return res.end() }
  if (!fs.existsSync(f)) { res.writeHead(404); return res.end() }
  res.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream' }); fs.createReadStream(f).pipe(res)
})
server.on('connection', s => { sockets.add(s); s.on('close', () => sockets.delete(s)) })
await new Promise(r => server.listen(0, '127.0.0.1', r))
const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lwc-render-'))
const child = spawn(process.env.CVD_CHROMIUM || '/usr/bin/chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
  `--user-data-dir=${profileDir}`, '--virtual-time-budget=5000', '--dump-dom', `http://127.0.0.1:${server.address().port}/`], { stdio: ['ignore', 'pipe', 'ignore'] })
let code = 1
try {
  // Child must exit 0 by itself within the deadline with untruncated stdout;
  // valid PASS lines in the DOM never override a timeout/overflow/exit failure.
  const run = await new Promise(resolve => {
    let s = '', done = false, overflow = false
    const finish = (status, extra = {}) => { if (!done) { done = true; clearTimeout(timer); resolve({ dom: s, status, overflow, ...extra }) } }
    const timer = setTimeout(() => finish('deadline'), Number(process.env.CVD_LWC_DEADLINE_MS) || DEADLINE_MS)
    child.stdout.on('data', d => { if (s.length + d.length > MAX_STDOUT) overflow = true; else s += d.toString() })
    child.on('close', (exit, signal) => finish(exit === 0 && !signal ? 'ok' : 'exit', { exit, signal }))
    child.on('error', e => finish('spawn', { error: String(e) }))
  })
  const dom = run.dom
  const m = dom.match(/data-result="([^"]*)"/)
  const lines = m ? JSON.parse(m[1].replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&')) : ['FAIL no result']
  if (run.status !== 'ok' || run.overflow) lines.push(`FAIL child ${run.status} exit=${run.exit} signal=${run.signal} overflow=${run.overflow}${run.error ? ' ' + run.error : ''}`)
  for (const l of lines) console.log(l)
  code = lines.length === KINDS && run.status === 'ok' && !run.overflow && lines.every(l => l.startsWith('PASS ')) ? 0 : 1
  console.log(code ? 'RED' : `${KINDS}/${KINDS} renderer kinds GREEN`)
} finally {
  const td = await teardown({ child, server, sockets, profileDir })
  if (!td.ok) { console.log('TEARDOWN FAIL ' + JSON.stringify(td)); code = code || 2 }
}
process.exit(code)
