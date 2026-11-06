#!/usr/bin/env node
// Served order-book chain: REAL cvd-server app_router (/book WS + static +
// history) from the opt-in Rust test `postgres_served_book_chain`
// (crates/cvd-server/src/orderbook_served_tests.rs) -> real Chromium -> the
// shipped book-panel.js + WasmPluginHost + cvd_browser_host.wasm -> the REAL
// public orderbook .wasm -> DOM price-level panel.
//
// CVD_BOOK_FEED=replay (default): the exchange is a SYNTHETIC scripted feed
// driven through the test-only file mailbox; expected rows are computed here
// from the scripted snapshot/events (independent of the plugin). real: the
// hub runs against public Binance USD-M depth (bounded live run).
//
//   CVD_SERVED_CHAIN_DIR / CVD_SERVED_STATIC_DIR  as tests/served-chain-browser.mjs
//   CVD_BOOK_WASM     path to cvd_indicator_orderbook.wasm (candidate)
//   CVD_LEGACY_WASM_DIR dir with caller-supplied bar plugins cvd_indicator_cvd.wasm + cvd_indicator_flow.wasm
//                       (any WASM v1 build; the env name is historical)
//   CVD_BOOK_SHOTS    dir for screenshots
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import { spawn } from 'node:child_process'
import { teardown } from './harness-teardown.mjs'

const DIR = process.env.CVD_SERVED_CHAIN_DIR
const STATIC = process.env.CVD_SERVED_STATIC_DIR
const BOOK = process.env.CVD_BOOK_WASM
const LEGACY = process.env.CVD_LEGACY_WASM_DIR
const SHOTS = process.env.CVD_BOOK_SHOTS
const CHROMIUM = process.env.CVD_CHROMIUM || '/usr/bin/chromium'
const sleep = ms => new Promise(r => setTimeout(r, ms))
const checks = []
const check = (name, ok, detail = '') => { checks.push({ name, ok: !!ok }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ' :: ' + String(detail).slice(0, 400)}`) }
const bounded = (p, ms, what) => Promise.race([p, sleep(ms).then(() => { throw new Error(`timeout ${ms}ms: ${what}`) })])

let exit = 1, chrome, profile, ws, info, seq = 0, id = 0
const pending = new Map(); const exceptions = []; const external = []; const sockets = []
async function mailbox(cmd) {
  const n = seq++
  fs.writeFileSync(path.join(DIR, `cmd.${n}.tmp`), JSON.stringify(cmd)); fs.renameSync(path.join(DIR, `cmd.${n}.tmp`), path.join(DIR, `cmd.${n}`))
  for (let i = 0; i < 500; i++) { const f = path.join(DIR, `ack.${n}`); if (fs.existsSync(f)) { await sleep(20); const t = fs.readFileSync(f, 'utf8'); try { return JSON.parse(t) } catch { return t } } await sleep(20) }
  throw new Error('mailbox timeout')
}
const call = (method, params = {}) => bounded(new Promise((res, rej) => { const i = ++id; pending.set(i, m => m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result)); ws.send(JSON.stringify({ id: i, method, params })) }), 20000, method)
const evaluate = async expr => { const r = await call('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 300)); return r.result.value }
async function waitFor(expr, ms = 30000) { const end = Date.now() + ms; while (Date.now() < end) { try { if (await evaluate(expr)) return true } catch {} await sleep(50) } throw new Error('timeout: ' + expr) }
async function shot(name) { if (!SHOTS) return; const { data } = await call('Page.captureScreenshot', { format: 'png' }); fs.mkdirSync(SHOTS, { recursive: true }); fs.writeFileSync(path.join(SHOTS, name), Buffer.from(data, 'base64')) }

const B = 'window.__cvdLocal.book()'
const bookState = () => evaluate(`${B}.state()`)
const dom = () => evaluate(`(() => { const p = document.getElementById('book-panel'); const rows = [...p.querySelectorAll('.book-row')].map(r => [...r.children].map(c => c.textContent)); return { hidden: p.hidden, status: p.querySelector('.book-status')?.textContent ?? null, venue: p.querySelector('.book-venue')?.textContent ?? null, title: p.querySelector('.book-title')?.textContent ?? null, spread: p.querySelector('.book-spread')?.textContent ?? null, note: p.querySelector('.book-note')?.textContent ?? null, rows } })()`)
const fmt = (v, d) => v.toLocaleString('en-US', { minimumFractionDigits: d, maximumFractionDigits: d })

// Independent expectation for the scripted replay book (rows=20 exact levels, base units).
function expectedRows(mid, tick, levels, overrides) {
  const bids = new Map(), asks = new Map()
  for (let i = 0; i < levels; i++) { bids.set(+(mid - tick * (i + 1)).toFixed(2), +(0.5 + (i % 7)).toFixed(3)); asks.set(+(mid + tick * (i + 1)).toFixed(2), +(0.25 + (i % 5)).toFixed(3)) }
  for (const [side, p, q] of overrides) { const m = side === 'b' ? bids : asks; if (q === 0) m.delete(p); else m.set(p, q) }
  const b = [...bids].sort((x, y) => y[0] - x[0]).slice(0, 20), a = [...asks].sort((x, y) => x[0] - y[0]).slice(0, 20)
  // display digits = max decimals among the shown prices (panel rule)
  const d = Math.max(0, ...[...a, ...b].map(([p]) => (String(p).split('.')[1] ?? '').length))
  return [...a.reverse().map(([p, q]) => ['', fmt(p, d), q]), ...b.map(([p, q]) => [q, fmt(p, d), ''])]
}
const q3 = v => v === '' ? '' : (v >= 1 ? fmt(v, 3) : fmt(v, 4))

try {
  if (!DIR || !STATIC || !BOOK) throw new Error('CVD_SERVED_CHAIN_DIR, CVD_SERVED_STATIC_DIR, CVD_BOOK_WASM required')
  for (let i = 0; i < 600 && !info; i++) { try { info = JSON.parse(fs.readFileSync(path.join(DIR, 'port'), 'utf8')) } catch { await sleep(200) } }
  if (!info) throw new Error('server did not start')
  const BASE = `http://127.0.0.1:${info.port}`
  const real = info.real === true
  console.log(`MODE ${real ? 'real-binance' : 'replay-synthetic'}`)
  // Read-only chart accessor appended to the SERVED COPY of js/app.js in the
  // disposable assembled tree only (same technique as served-chain-browser.mjs).
  const appPath = path.join(STATIC, 'js/app.js')
  const app = fs.readFileSync(appPath, 'utf8'); if (!/\ninit\(\)\s*$/.test(app)) throw new Error('app.js shape changed')
  fs.writeFileSync(appPath, app.replace(/\ninit\(\)\s*$/, `\nwindow.__cvdSrv = { range: () => { const r = chart.timeScale().getVisibleLogicalRange(); return r ? [r.from, r.to] : null }, readouts: () => paneSummaryEls.map(e => e.textContent) }\n\ninit()\n`))

  // Unsupported source is rejected by the server (no substitution).
  for (const q of ['exchange=binance&market=coinm&symbol=SOL', 'exchange=coinbase&market=usdc&symbol=SOL', 'exchange=coinbase&market=usdm_usdc&symbol=ETH', 'exchange=coinbase&market=spot&symbol=SOL', 'exchange=coinbase&market=usdm&symbol=BTC', 'exchange=binance&market=usdm&symbol=SOL']) {
    const r = await new Promise(res => { const s = new WebSocket(`ws://127.0.0.1:${info.port}/book?${q}`); s.onopen = () => { s.close(); res('open') }; s.onerror = () => res('error'); setTimeout(() => res('timeout'), 5000) })
    check(`server_rejects_unsupported_${q.replace(/[=&]/g, '_')}`, r === 'error', r)
  }

  profile = fs.mkdtempSync(path.join(os.tmpdir(), 'book-chain-'))
  chrome = spawn(CHROMIUM, ['--headless=new', '--no-sandbox', '--disable-gpu', `--user-data-dir=${profile}`, '--remote-debugging-port=0', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] })
  const wsUrl = await new Promise((resolve, reject) => { let b = ''; const t = setTimeout(() => reject(new Error('chromium start timeout')), 20000); chrome.stderr.on('data', d => { b += d; const m = b.match(/DevTools listening on (ws:\/\/\S+)/); if (m) { clearTimeout(t); resolve(m[1]) } }) })
  const targets = await (await fetch(`http://${new URL(wsUrl).host}/json/list`)).json()
  ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl)
  await bounded(new Promise(r => ws.addEventListener('open', r, { once: true })), 10000, 'cdp open')
  ws.addEventListener('message', ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) }
    if (m.method === 'Runtime.exceptionThrown') exceptions.push(JSON.stringify(m.params.exceptionDetails).slice(0, 300))
    if (m.method === 'Network.requestWillBeSent') { const u = new URL(m.params.request.url); if (/^(https?|wss?):$/.test(u.protocol) && u.host !== `127.0.0.1:${info.port}`) external.push({ url: u.href, method: m.params.request.method, type: m.params.type }) }
    if (m.method === 'Runtime.consoleAPICalled' && ['warning', 'error'].includes(m.params.type)) console.log('CONSOLE', m.params.type, JSON.stringify(m.params.args.map(a => a.value ?? a.description)).slice(0, 400))
    if (m.method === 'Network.webSocketCreated') sockets.push(m.params.url) })
  await call('Runtime.enable'); await call('Network.enable'); await call('DOM.enable'); await call('Page.enable')
  await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
  await call('Page.navigate', { url: `${BASE}/?exchange=binance&market=usdm&symbol=BTC&tf=1m` })
  await waitFor(`!!window.__cvdLocal && !!window.__cvdLocal.host() && !!window.__cvdLocal.book()`, 30000)
  const L = 'window.__cvdLocal'
  async function importFile(file) {
    if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`)
    const { root } = await call('DOM.getDocument', { depth: -1 }); const { nodeId } = await call('DOM.querySelector', { nodeId: root.nodeId, selector: '.plugin-import-input' })
    const before = await evaluate(`${L}.records().size`)
    await call('DOM.setFileInputFiles', { nodeId, files: [file] })
    await waitFor(`${L}.records().size > ${before} || /already|rejected|error|not/i.test(document.querySelector('.plugin-menu-notice')?.textContent ?? '')`, 20000)
    return evaluate(`${L}.records().size > ${before}`)
  }
  let toggleClicks = 0
  const toggle = async (pid, on) => { if ((await evaluate(`${L}.records().get(${JSON.stringify(pid)})?.enabled === true`)) !== on) { if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`); toggleClicks++; await evaluate(`document.querySelector('.plugin-menu-row[data-plugin=${JSON.stringify(pid)}] input[type=checkbox]').click()`) } }
  const selectSource = async (market, symbol) => evaluate(`(() => { const m = document.getElementById('market'); m.value = ${JSON.stringify(market)}; m.dispatchEvent(new Event('change')); const s = document.getElementById('symbol'); s.value = ${JSON.stringify(symbol)}; s.dispatchEvent(new Event('change')); return [m.value, s.value] })()`)

  const closeMenu = async () => { if (!(await evaluate(`document.getElementById('plugin-menu').hidden`))) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`) }
  const setParams = async (pid, values) => {
    if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`)
    const row = `document.querySelector('.plugin-menu-row[data-plugin=${JSON.stringify(pid)}]')`
    if (await evaluate(`${row}.querySelector('.plugin-menu-params').hidden`)) await evaluate(`${row}.querySelector('[data-action=params]').click()`)
    await evaluate(`(() => { const r = ${row}; for (const [k, v] of Object.entries(${JSON.stringify(values)})) { const i = r.querySelector('[data-param=' + JSON.stringify(k) + ']'); i.value = String(v) } r.querySelector('[data-action=apply]').click(); return true })()`)
  }
  const menuMeta = pid => evaluate(`document.querySelector('.plugin-menu-row[data-plugin=${JSON.stringify(pid)}]')?.textContent ?? ''`)
  // Both sides visible in the initial panel viewport: best ask, best bid, the
  // spread line and >= 3 rows per side, all inside panel AND window.
  const bothSides = () => evaluate(`(() => {
    const p = document.getElementById('book-panel'), pr = p.getBoundingClientRect()
    const vis = e => { if (!e) return false; const r = e.getBoundingClientRect(), s = e.closest('.book-rows')?.getBoundingClientRect() ?? pr
      return r.height > 0 && r.top >= Math.max(pr.top, s.top) - 1 && r.bottom <= Math.min(pr.bottom, s.bottom, innerHeight) + 1 && r.left >= 0 && r.right <= innerWidth + 1 }
    const cells = sel => [...p.querySelectorAll(sel + ' .book-price')]
    const asks = cells('.book-asks'), bids = cells('.book-bids')
    const chart = document.getElementById('chart').getBoundingClientRect()
    return { bestAsk: vis(asks.at(-1)), bestBid: vis(bids[0]), spread: vis(p.querySelector('.book-spread')),
      askRows: asks.filter(vis).length, bidRows: bids.filter(vis).length, panel: [pr.left, pr.top, pr.width, pr.height].map(Math.round),
      chart: [chart.left, chart.top, chart.width, chart.height].map(Math.round), side: pr.left >= chart.right - 1, iw: innerWidth, ih: innerHeight,
      scroll: [...p.querySelectorAll('.book-rows')].map(e => [e.scrollTop, e.scrollHeight - e.clientHeight, Math.round(e.getBoundingClientRect().top), e.clientHeight]), bid0: bids[0] && Math.round(bids[0].getBoundingClientRect().top) }
  })()`)
  const bothOk = v => v.bestAsk && v.bestBid && v.spread && v.askRows >= 3 && v.bidRows >= 3
  // Base-bar CVD/Flow values (fixed SYNTHETIC history), displayed readouts and chart viewport.
  const basis = () => evaluate(`(() => { const v = {}, full = {}; for (const [k, m] of ${L}.values()) if (/^(cvd|delta|flow)\\//.test(k)) { const e = [...m].filter(([, x]) => x != null); v[k] = [m.size, e.slice(-3)]; full[k] = [...m] }
    const r = window.__cvdSrv.range(); return { values: v, full, range: r && r.map(x => +x.toFixed(3)), readouts: window.__cvdSrv.readouts(), tf: document.getElementById('tf').value } })()`)
  // Independent witnesses recomputed from the constants published in
  // orderbook_served_tests.rs (cvd_k = offset + cumulative delta; buy - sell = delta).
  const FIXTURES = {
    'BTC|1m': { t: info.t0, step: 60, off: 250000, p: 5, neg: -30, pos: 10, sell: 5990 },
    'BTC|5m': { t: info.t5, step: 300, off: 500000, p: 3, neg: -40, pos: 25, sell: 18600 },
    'ETH|1m': { t: info.t0, step: 60, off: -120000, p: 4, neg: -15, pos: 7, sell: 2400 },
  }
  const expectedBars = f => { const m = new Map(); let c = f.off; for (let k = 0; k < 400; k++) { const d = k % f.p === 0 ? f.neg : f.pos; c += d; m.set(f.t + k * f.step + f.step - 1, { cvd: c, delta: d, buy: f.sell + d, sell: f.sell }) } return m }
  const fc = v => !Number.isFinite(v) ? 'unavailable' : Math.abs(v) >= 1e9 ? `${(v / 1e9).toFixed(2)}B` : Math.abs(v) >= 1e6 ? `${(v / 1e6).toFixed(2)}M` : Math.abs(v) >= 1e3 ? `${(v / 1e3).toFixed(2)}K` : v.toLocaleString('en-US', { maximumFractionDigits: 2 })
  // Per plugin output: nonempty, every value finite and equal to the fixture
  // witness, nonzero, distinct, latest bar present; plus each plugin's own readout.
  function witness(b, fix) {
    const exp = expectedBars(FIXTURES[fix]), errors = [], stats = {}
    const fin = v => typeof v === 'number' && Number.isFinite(v)
    const outs = { cvd: ['cvd/', e => e.cvd, 50], delta: ['flow/', e => e.delta, 2], total: ['flow/', e => e.buy + e.sell, 2], buy_sell: ['flow/', e => ({ upper: e.buy, lower: -e.sell }), 2] }
    const last = Math.max(...exp.keys())
    for (const [out, [prefix, want, minDistinct]] of Object.entries(outs)) {
      const key = Object.keys(b.full ?? {}).find(k => k.startsWith(prefix) && k.split('|')[0].endsWith('/' + out) && k.endsWith('|source'))
      if (!key) { errors.push(`missing ${prefix}${out}`); continue }
      const es = b.full[key]; let bad = 0, zero = 0; const distinct = new Set()
      for (const [t, v] of es) {
        const e = exp.get(Number(t)), w = e && want(e)
        const ok = !!e && (typeof w === 'number' ? fin(v) && Math.abs(v - w) <= 1e-6 : v && fin(v.upper) && fin(v.lower) && Math.abs(v.upper - w.upper) <= 1e-6 && Math.abs(v.lower - w.lower) <= 1e-6)
        if (!ok) bad++
        if (typeof v === 'number' ? v === 0 : !v || v.upper === 0 || v.lower === 0) zero++
        distinct.add(JSON.stringify(v))
      }
      stats[out] = { n: es.length, bad, zero, distinct: distinct.size }
      if (es.length < 300) errors.push(`${out} n=${es.length}`)
      if (bad) errors.push(`${out} bad=${bad}`)
      if (zero) errors.push(`${out} zero=${zero}`)
      if (distinct.size < minDistinct) errors.push(`${out} distinct=${distinct.size}`)
      if (!es.some(([t]) => Number(t) === last)) errors.push(`${out} latest bar missing`)
    }
    const e = exp.get(last), ro = b.readouts ?? []
    const cvdRo = ro.find(t => t.startsWith('CVD ')), flowRo = ro.find(t => /\bDelta\b/.test(t))
    if (!cvdRo || !cvdRo.startsWith(`CVD ${fc(e.cvd)} `)) errors.push(`cvd readout ${cvdRo} want CVD ${fc(e.cvd)}`)
    if (!flowRo || !flowRo.includes(`Delta ${fc(e.delta)} `) || !flowRo.includes(`Total ${fc(e.buy + e.sell)}`)) errors.push(`flow readout ${flowRo} want Delta ${fc(e.delta)} / Total ${fc(e.buy + e.sell)}`)
    if (b.tf !== fix.split('|')[1]) errors.push(`tf ${b.tf}`)
    return { ok: errors.length === 0, errors, stats }
  }
  const basisOk = (b, fix) => { const ks = Object.keys(b.values); const w = witness(b, fix); if (!w.ok) console.log('WITNESS', fix, JSON.stringify(w).slice(0, 600), JSON.stringify(Object.keys(b.full ?? {})), JSON.stringify(b.readouts))
    return w.ok && ks.some(k => k.startsWith('cvd/')) && ks.some(k => /^(delta|flow)\//.test(k)) && ks.every(k => b.values[k][0] >= 300 && b.values[k][1].length === 3) && b.range }
  // Negative controls: the witness must reject zero/NaN/missing on either plugin and a wrong readout.
  function witnessNegativeControls(b, fix) {
    const keyOf = (x, out) => Object.keys(x.full).find(k => k.split('|')[0].endsWith('/' + out))
    const mut = f => { const x = structuredClone(b); f(x); return witness(x, fix).ok }
    const cases = {
      cvd_zero: x => { x.full[keyOf(x, 'cvd')] = x.full[keyOf(x, 'cvd')].map(([t]) => [t, 0]) },
      cvd_nan: x => { x.full[keyOf(x, 'cvd')].at(-5)[1] = NaN },
      cvd_missing: x => { delete x.full[keyOf(x, 'cvd')] },
      flow_delta_zero: x => { x.full[keyOf(x, 'delta')] = x.full[keyOf(x, 'delta')].map(([t]) => [t, 0]) },
      flow_delta_nan: x => { x.full[keyOf(x, 'delta')].at(-5)[1] = NaN },
      flow_total_null: x => { x.full[keyOf(x, 'total')].at(-1)[1] = null },
      flow_missing: x => { for (const o of ['delta', 'total', 'buy_sell']) delete x.full[keyOf(x, o)] },
      cvd_readout_wrong: x => { x.readouts = x.readouts.map(t => t.startsWith('CVD ') ? 'CVD 0 · x' : t) },
      flow_readout_unavailable: x => { x.readouts = x.readouts.map(t => /\bDelta\b/.test(t) ? 'Delta unavailable · Total unavailable' : t) },
    }
    const accepted = Object.entries(cases).filter(([, f]) => mut(f)).map(([n]) => n)
    return { base: witness(b, fix).ok, accepted, n: Object.keys(cases).length }
  }
  const sameValues = (a, b) => JSON.stringify(a.values) === JSON.stringify(b.values) && JSON.stringify(a.full) === JSON.stringify(b.full) && JSON.stringify(a.readouts) === JSON.stringify(b.readouts)
  const valuesReady = `(() => { let c = 0, f = 0; for (const [k, m] of ${L}.values()) { if (k.startsWith('cvd/') && m.size >= 300) c++; if (/^(delta|flow)\\//.test(k) && m.size >= 300) f++ } return c > 0 && f > 0 })()`
  // Real user pan: mouse drag across the price chart.
  const panChart = async dx => {
    const c = await evaluate(`(() => { const r = document.getElementById('chart').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 3] })()`)
    await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: c[0], y: c[1] })
    await call('Input.dispatchMouseEvent', { type: 'mousePressed', x: c[0], y: c[1], button: 'left', buttons: 1, clickCount: 1 })
    for (let i = 1; i <= 10; i++) await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: c[0] + dx * i / 10, y: c[1], button: 'left', buttons: 1 })
    await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x: c[0] + dx, y: c[1], button: 'left', buttons: 1, clickCount: 1 })
    // Leave the chart so readouts show the latest bar, not a hovered one.
    await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: 1, y: 1 })
    await sleep(300)
  }
  const near = (a, b) => Math.abs(a - b) <= 0.01

  // The page does not take the source from the URL: select USD-M BTC through the real controls.
  await selectSource('usdm', 'BTC')
  await waitFor(`document.getElementById('market').value === 'usdm' && document.getElementById('status').textContent && !/loading/.test(document.getElementById('status').textContent)`, 30000)
  const barsStatus0 = await evaluate(`document.getElementById('status').textContent`)
  check('book_panel_hidden_without_plugins', (await dom()).hidden === true)
  check('no_book_socket_without_demand', !sockets.some(u => u.includes('/book')), sockets.join(','))

  // Caller-supplied CVD/Flow bar plugins coexist with the book plugin: import and compute.
  if (LEGACY) {
    for (const f of ['cvd_indicator_cvd.wasm', 'cvd_indicator_flow.wasm']) check(`legacy_import_${f}`, await importFile(path.join(LEGACY, f)))
    for (const pid of ['cvd', 'flow']) await toggle(pid, true)
    try { await waitFor(`[...${L}.values().keys()].some(k => k.startsWith('cvd/')) && [...${L}.values().keys()].some(k => k.startsWith('delta/') || k.startsWith('flow/'))`, 60000) }
    catch (e) { await shot('diag.png'); console.log('EXC', exceptions.join('\n')); console.log('DIAG', JSON.stringify(await evaluate(`(async () => ({ keys: [...${L}.values().keys()], recs: [...${L}.records().values()].map(r => [r.id, r.enabled]), running: ${L}.compute().running(), rev: ${L}.revision(), stats: ${L}.compute().stats(), rt: ${L}.compute().runtime('cvd'), idle: ${L}.compute().idle(), hist: await (await fetch('/history?exchange=binance&market=usdm&symbol=BTC&tf=1m&series=contracts-drb-v1&limit=5')).text().then(t => t.slice(0, 300)), status: document.getElementById('status').textContent, bars: document.getElementById('plugin-state').title }))()`))); throw e }
    const keys = await evaluate(`[...${L}.values().keys()]`)
    const n = await evaluate(`(${L}.values().get(${JSON.stringify((await evaluate(`[...${L}.values().keys()].find(k => k.startsWith('cvd/'))`)))}) ?? new Map()).size`)
    check('legacy_cvd_flow_compute_values', n >= 300, `n=${n} keys=${keys}`)
  }

  check('import_orderbook_plugin', await importFile(BOOK))
  const desc = await evaluate(`${L}.records().get('orderbook')?.descriptor`)
  check('orderbook_descriptor_book_input', desc?.inputs?.[0] === 'order_book' && desc?.outputs?.[0]?.type === 'price_levels', JSON.stringify(desc))
  check('orderbook_not_a_bar_series_plugin', !(await evaluate(`${L}.compute().running?.() ?? []`)).includes('orderbook'))

  if (!real) {
    // L. Fixed SYNTHETIC base bars: meaningful CVD and Flow values + a
    // user-panned (non-default) viewport recorded BEFORE any book action.
    await waitFor(valuesReady, 30000)
    const r0 = await basis()
    await panChart(260)
    const b0 = await basis()
    check('basis_btc_values_meaningful', basisOk(b0, 'BTC|1m'), JSON.stringify(witness(b0, 'BTC|1m')))
    const neg = witnessNegativeControls(b0, 'BTC|1m')
    check('witness_negative_controls_reject_zero_nan_missing_readout', neg.base && neg.n === 9 && neg.accepted.length === 0, JSON.stringify(neg))
    check('basis_viewport_user_panned', b0.range && r0.range && Math.abs(b0.range[1] - r0.range[1]) > 5, JSON.stringify([r0.range, b0.range]))
    // A. enable before any exchange snapshot exists: syncing/stale, never fabricated depth.
    await toggle('orderbook', true)
    await waitFor(`${B}.state().plugins[0]?.state === 'ready'`, 20000)
    await waitFor(`/stale|syncing|connecting/.test(document.querySelector('#book-panel .book-status')?.textContent ?? '')`, 10000)
    let d = await dom()
    check('syncing_no_fabricated_rows', d.rows.length === 0 && !d.hidden, JSON.stringify(d))
    await closeMenu(); await sleep(300)
    const b1 = await basis()
    // The side panel narrows the chart: the right edge (to) is kept, the view is not reset to default.
    check('enable_keeps_values_and_viewport', sameValues(b0, b1) && near(b1.range[1], b0.range[1]) && !near(b1.range[1], r0.range[1]), JSON.stringify([b0.range, b1.range, r0.range, sameValues(b0, b1), b0.readouts, b1.readouts]))
    let st = await mailbox({ op: 'state' })
    check('server_demand_btc_one_subscriber', JSON.stringify(st.active) === JSON.stringify([['binance/usdm/BTC', 1]]), JSON.stringify(st))
    // B. snapshot + bridging event (pu != L is legal for the first event).
    await mailbox({ op: 'snapshot', sym: 'BTCUSDT', id: 1000, mid: 30000, tick: 0.1, levels: 200 })
    const c0 = st.connects.BTCUSDT ?? 0
    await waitFor(`true`, 1); for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects.BTCUSDT ?? 0) > c0) break; await sleep(50) }
    await sleep(100)
    check('bridge_event_sent', (await mailbox({ op: 'event', sym: 'BTCUSDT', U: 990, u: 1005, pu: 980, b: [['29999.90', '9.999']], a: [['30000.20', '0']] })).ok)
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows > 0`, 15000)
    d = await dom()
    let want = expectedRows(30000, 0.1, 200, [['b', 29999.9, 9.999], ['a', 30000.2, 0]]).map(r => [q3(r[0]), r[1], q3(r[2])])
    check('btc_rows_exact_replay', JSON.stringify(d.rows) === JSON.stringify(want), `got=${JSON.stringify(d.rows.slice(0, 4))} want=${JSON.stringify(want.slice(0, 4))}`)
    check('btc_labels_venue_unit_coverage', /Binance USDⓈ-M perpetual BTCUSDT/.test(d.venue) && /qty BTC/.test(d.title) && /spread 0\.2\b/.test(d.spread) && /Supplied range 29,980\.0–30,020\.0;/.test(d.note) && /single source/.test(d.note) && /Plugin view of supplied depth/.test(d.note), JSON.stringify(d))
    check('book_ticks_keep_values_and_viewport', (b => sameValues(b1, b) && JSON.stringify(b.range) === JSON.stringify(b1.range))(await basis()), JSON.stringify((await basis()).range))
    const gen1 = (await bookState()).generation
    // M. Independent TF-only change while the BTC book is live (feed kept
    // alive by empty pu-chained diffs): the SAME book subscription/socket and
    // server task stay; CVD and Flow recompute valid values for the new TF.
    let btcU = 1005, btcPump = true
    const pumpBtc = (async () => { while (btcPump) { await mailbox({ op: 'event', sym: 'BTCUSDT', U: btcU + 1, u: btcU + 1, pu: btcU, b: [], a: [] }); btcU++; await sleep(600) } })()
    const tfBefore = { st: await mailbox({ op: 'state' }), sockets: sockets.filter(u => u.includes('/book')).length, gen: (await bookState()).generation, cvd: await basis() }
    await evaluate(`(() => { const t = document.getElementById('tf'); t.value = '5m'; t.dispatchEvent(new Event('change')); return t.value })()`)
    await waitFor(`(() => { for (const [k, m] of ${L}.values()) if (k.startsWith('cvd/') && m.size >= 300 && [...m.keys()].every(t => (Number(t) + 1) % 300 === 0)) return true; return false })()`, 30000)
    await waitFor(valuesReady, 30000)
    await sleep(1500)
    const tfAfter = { st: await mailbox({ op: 'state' }), sockets: sockets.filter(u => u.includes('/book')).length, bs: await bookState(), cvd: await basis() }
    check('tf_change_keeps_book_subscription', tfAfter.sockets === tfBefore.sockets && tfAfter.st.connects.BTCUSDT === tfBefore.st.connects.BTCUSDT && JSON.stringify(tfAfter.st.active) === JSON.stringify([['binance/usdm/BTC', 1]]) && tfAfter.bs.generation === tfBefore.gen && tfAfter.bs.connected && tfAfter.bs.status.state === 'live' && tfAfter.bs.plugins[0].rows > 0 && tfAfter.bs.seq > 1005,
      JSON.stringify({ b: [tfBefore.sockets, tfBefore.st.connects, tfBefore.gen], a: [tfAfter.sockets, tfAfter.st.connects, tfAfter.st.active, tfAfter.bs.generation, tfAfter.bs.status, tfAfter.bs.seq] }))
    const flowKeys = b => Object.keys(b.values).filter(k => /^(delta|flow)\//.test(k))
    check('tf_change_cvd_and_flow_recomputed_valid', tfAfter.cvd.tf === '5m' && basisOk(tfAfter.cvd, 'BTC|5m') && flowKeys(tfAfter.cvd).length > 0 && JSON.stringify(tfAfter.cvd.values) !== JSON.stringify(tfBefore.cvd.values)
      && Object.values(tfAfter.cvd.values).every(([, last]) => last.every(([t]) => (Number(t) + 1) % 300 === 0)), JSON.stringify(tfAfter.cvd).slice(0, 500))
    await evaluate(`(() => { const t = document.getElementById('tf'); t.value = '1m'; t.dispatchEvent(new Event('change')); return t.value })()`)
    await waitFor(`(() => { for (const [k, m] of ${L}.values()) if (k.startsWith('cvd/') && m.size >= 300 && [...m.keys()].some(t => (Number(t) + 1) % 300 !== 0)) return true; return false })()`, 30000)
    const tfBack = { st: await mailbox({ op: 'state' }), sockets: sockets.filter(u => u.includes('/book')).length, bs: await bookState() }
    check('tf_back_keeps_book_subscription', tfBack.sockets === tfBefore.sockets && tfBack.st.connects.BTCUSDT === tfBefore.st.connects.BTCUSDT && tfBack.bs.generation === tfBefore.gen && tfBack.bs.status.state === 'live', JSON.stringify([tfBack.sockets, tfBack.st.connects, tfBack.bs.generation]))
    btcPump = false; await pumpBtc
    // U. Page lifecycle for the Book driver. SYNTHETIC visibility injection
    // (overridden document.hidden + visibilitychange) in headless Chromium:
    // NOT Safari emulation or Safari acceptance. The hide fires synchronously
    // right after a real Book compute was posted to its Worker (in flight).
    {
      const H = `${L}.host()`
      const bookWorkers = `[...${H}.instances.keys()].filter(i => i >= 2 ** 29).length`
      const bookBusy = `[...${H}.waiters.values()].some(w => w.instance >= 2 ** 29)`
      const setHid = v => `(globalThis.__hid = ${v}, document.dispatchEvent(new Event('visibilitychange')), true)`
      await evaluate(`(() => { Object.defineProperty(document, 'hidden', { configurable: true, get: () => globalThis.__hid === true })
        const p = globalThis.__wpm = { n: 0 }; Worker.prototype.__realPost ??= Worker.prototype.postMessage
        Worker.prototype.postMessage = function (...a) { p.n++; return Worker.prototype.__realPost.apply(this, a) }
        const h = ${H}, real = h.compute; globalThis.__armHide = true
        h.compute = function (inst, ...a) { const r = real.call(this, inst, ...a)
          if (inst >= 2 ** 29 && globalThis.__armHide) { globalThis.__armHide = false; globalThis.__busyAtHide = ${bookBusy}; ${setHid(true)} }
          return r }
        return true })()`)
      const sockets0 = sockets.filter(u => u.includes('/book')).length, c0 = (await mailbox({ op: 'state' })).connects.BTCUSDT
      const step = async () => { await mailbox({ op: 'event', sym: 'BTCUSDT', U: btcU + 1, u: btcU + 1, pu: btcU, b: [], a: [] }); btcU++ }
      for (let i = 0; i < 50 && !(await evaluate('document.hidden')); i++) { await step(); await sleep(100) }
      const atHide = await evaluate(`({ hidden: document.hidden, busyAtHide: globalThis.__busyAtHide, busy: ${bookBusy}, workers: ${bookWorkers}, posts: __wpm.n })`)
      check('lifecycle_book_hidden_with_compute_in_flight', atHide.hidden === true && atHide.busyAtHide === true, JSON.stringify(atHide))
      check('lifecycle_book_inflight_worker_terminated_on_hide', atHide.busy === false && atHide.workers === 0, JSON.stringify(atHide))
      // Away > 5000 ms compute deadline while the feed keeps ticking.
      for (let i = 0; i < 10; i++) { await step(); await sleep(600) }
      const hid = await evaluate(`({ posts: __wpm.n, workers: ${bookWorkers}, bs: ${B}.state(), bars: ${L}.compute().suspended() })`)
      check('lifecycle_book_no_worker_work_hidden', hid.posts === atHide.posts && hid.workers === 0 && hid.bs.seq === btcU, JSON.stringify({ a: atHide.posts, h: hid.posts, w: hid.workers, seq: hid.bs.seq, btcU }))
      check('lifecycle_book_no_fault_hidden', hid.bs.plugins[0].fault === null && hid.bars === true, JSON.stringify(hid.bs.plugins[0]))
      // Return on a now-quiet source: latest frame recomputed without a new tick.
      await evaluate(setHid(false))
      await waitFor(`${B}.state().plugins[0].seq === ${btcU} && ${B}.state().plugins[0].rows > 0`, 10000).catch(() => {})
      const back = await bookState(); d = await dom()
      check('lifecycle_book_quiet_return_latest_frame', back.plugins[0].seq === btcU && back.status.state === 'live' && back.plugins[0].fault === null && JSON.stringify(d.rows) === JSON.stringify(want), JSON.stringify({ seq: back.plugins[0].seq, btcU, st: back.status, f: back.plugins[0].fault }))
      // > 5000 ms more with the feed ticking: any late reply/deadline of the
      // cancelled call would land here. (Expired-frame withdrawal on a quiet
      // return is covered in tests/book-panel.test.mjs; keeping the replay
      // feed alive here preserves the following sections' server state.)
      for (let i = 0; i < 10; i++) { await step(); await sleep(600) }
      await waitFor(`${B}.state().plugins[0].seq === ${btcU}`, 10000).catch(() => {})
      const late = await evaluate(`({ bs: ${B}.state(), workers: ${bookWorkers}, bars: ${L}.compute().suspended() })`); d = await dom()
      check('lifecycle_book_no_late_fault_keeps_advancing', late.bs.plugins[0].fault === null && late.bs.status.state === 'live' && late.bs.plugins[0].seq === btcU && JSON.stringify(d.rows) === JSON.stringify(want) && late.workers === 1, JSON.stringify({ st: late.bs.status, f: late.bs.plugins[0].fault, seq: late.bs.plugins[0].seq, btcU, w: late.workers }))
      const st1 = await mailbox({ op: 'state' })
      check('lifecycle_book_one_socket_one_upstream', sockets.filter(u => u.includes('/book')).length === sockets0 && st1.connects.BTCUSDT === c0 && JSON.stringify(st1.active) === JSON.stringify([['binance/usdm/BTC', 1]]), JSON.stringify([sockets0, st1.connects, st1.active]))
      check('lifecycle_bars_driver_resumed_healthy', late.bars === false && (!LEGACY || await evaluate(valuesReady)), String(late.bars))
      await evaluate(`(delete ${H}.compute, true)`)
    }
    // C. pu gap => server withdraws (stale), resyncs with a new generation.
    await mailbox({ op: 'event', sym: 'BTCUSDT', U: btcU + 1, u: btcU + 5, pu: btcU, b: [['29999.80', '1.25']], a: [] })
    await waitFor(`${B}.state().seq === ${btcU + 5}`, 10000)
    await mailbox({ op: 'event', sym: 'BTCUSDT', U: btcU + 15, u: btcU + 25, pu: btcU + 10, b: [], a: [] })
    await waitFor(`/stale/.test(document.querySelector('#book-panel .book-status')?.textContent ?? '')`, 10000)
    d = await dom()
    check('gap_withdraws_levels_visible_stale', d.rows.length === 0 && /stale \(sequencegap\)/.test(d.status), JSON.stringify(d))
    await mailbox({ op: 'snapshot', sym: 'BTCUSDT', id: 2000, mid: 30010, tick: 0.1, levels: 200 })
    const c1 = (await mailbox({ op: 'state' })).connects.BTCUSDT
    for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if (st.connects.BTCUSDT > c1) break; await sleep(50) }
    await sleep(100)
    await mailbox({ op: 'event', sym: 'BTCUSDT', U: 1999, u: 2001, pu: 1990, b: [], a: [] })
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().generation > ${gen1} && ${B}.state().plugins[0].rows > 0`, 15000)
    d = await dom()
    want = expectedRows(30010, 0.1, 200, []).map(r => [q3(r[0]), r[1], q3(r[2])])
    check('resync_new_generation_exact_rows', JSON.stringify(d.rows) === JSON.stringify(want), JSON.stringify(d.rows.slice(0, 3)))
    // D. silent exchange feed => stale (server timer), withdrawn.
    await waitFor(`/stale \\(stale_feed\\)/.test(document.querySelector('#book-panel .book-status')?.textContent ?? '')`, 12000)
    check('silent_feed_stale_withdrawn', (await dom()).rows.length === 0)
    // E. exchange disconnect => withdrawn, then reconnect recovers.
    for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if (st.connects.BTCUSDT > c1 + 1) break; await sleep(50) }
    await sleep(100)
    await mailbox({ op: 'event', sym: 'BTCUSDT', U: 1999, u: 2002, pu: 1990, b: [], a: [] })
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows > 0`, 15000)
    check('drop_feed_sent', (await mailbox({ op: 'drop', sym: 'BTCUSDT' })).ok)
    await waitFor(`/stale \\(disconnected\\)/.test(document.querySelector('#book-panel .book-status')?.textContent ?? '')`, 10000)
    check('exchange_disconnect_withdrawn', (await dom()).rows.length === 0)
    // F. shared subscriber: an extra client shares the one book task.
    const extra = new WebSocket(`ws://127.0.0.1:${info.port}/book?exchange=binance&market=usdm&symbol=BTC`)
    await bounded(new Promise(r => extra.addEventListener('message', r, { once: true })), 10000, 'extra frame')
    st = await mailbox({ op: 'state' })
    check('shared_subscribers_one_task', JSON.stringify(st.active) === JSON.stringify([['binance/usdm/BTC', 2]]), JSON.stringify(st.active))
    // Outbox physical-connection replacement while the book is subscribed.
    const rep = await mailbox({ op: 'replace_outbox_connection' })
    check('outbox_connection_replaced_schema_kept', rep.ok && rep.file_backed && JSON.stringify(rep.schema) === JSON.stringify(['pending_bars', 'retired_pending_bars']) && rep.pending_after === rep.pending_before && rep.bars_rows_after === rep.bars_rows_before, JSON.stringify(rep))
    st = await mailbox({ op: 'state' })
    check('book_continues_after_outbox_replacement', JSON.stringify(st.active) === JSON.stringify([['binance/usdm/BTC', 2]]) && st.outbox_pending === st.outbox_pending_at_start && st.bars_rows === st.bars_rows_at_start, JSON.stringify(st))
    extra.close(); await sleep(500)
    st = await mailbox({ op: 'state' })
    check('extra_client_release_keeps_page_demand', JSON.stringify(st.active) === JSON.stringify([['binance/usdm/BTC', 1]]), JSON.stringify(st.active))
    // G. source switch to ETH: BTC cleared + released, ETH distinct data.
    await mailbox({ op: 'snapshot', sym: 'ETHUSDT', id: 500, mid: 2000, tick: 0.01, levels: 1000 })
    await selectSource('usdm', 'ETH')
    await waitFor(`${B}.state().source?.symbol === 'ETH'`, 10000)
    d = await dom()
    check('source_switch_clears_old_rows', d.rows.length === 0, JSON.stringify(d.rows.slice(0, 2)))
    for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects.ETHUSDT ?? 0) > 0 && JSON.stringify(st.active) === JSON.stringify([['binance/usdm/ETH', 1]])) break; await sleep(50) }
    check('btc_demand_released_eth_acquired', JSON.stringify(st.active) === JSON.stringify([['binance/usdm/ETH', 1]]), JSON.stringify(st.active))
    await sleep(100)
    // Legal 1000-level snapshot + 5 inside-range inserts per side => 1005 maintained, top 1000 transmitted.
    const INS_B = [1990.505, 1991.505, 1992.505, 1993.505, 1994.505], INS_A = [2009.495, 2008.495, 2007.495, 2006.495, 2005.495]
    await mailbox({ op: 'event', sym: 'ETHUSDT', U: 499, u: 501, pu: 400, b: INS_B.map(p => [p.toFixed(3), '1']), a: INS_A.map(p => [p.toFixed(3), '1']) })
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows > 0`, 15000)
    d = await dom()
    want = expectedRows(2000, 0.01, 1000, []).map(r => [q3(r[0]), r[1], q3(r[2])])
    check('eth_rows_exact_and_distinct', JSON.stringify(d.rows) === JSON.stringify(want) && /ETHUSDT/.test(d.venue) && /qty ETH/.test(d.title), JSON.stringify(d.rows.slice(0, 3)))
    // Keep the scripted ETH feed alive (empty diffs continue the pu chain) while
    // parameters/layout are exercised; results must stay admitted, not aged out.
    let ethU = 501, pumping = true
    const pump = (async () => { while (pumping) { await mailbox({ op: 'event', sym: 'ETHUSDT', U: ethU + 1, u: ethU + 1, pu: ethU, b: [], a: [] }); ethU++; await sleep(700) } })()
    // ETH base basis (SYNTHETIC ETH bars) + user-panned viewport before the ETH book actions.
    await waitFor(`(() => { for (const [k, m] of ${L}.values()) if (k.startsWith('cvd/') && m.size >= 300) return true; return false })() && ${valuesReady}`, 30000)
    await sleep(500)
    const e0r = await basis()
    await panChart(-220)
    const e0 = await basis()
    check('basis_eth_values_meaningful_and_distinct', basisOk(e0, 'ETH|1m') && JSON.stringify(e0.values) !== JSON.stringify(b0.values) && Math.abs(e0.range[1] - e0r.range[1]) > 5, JSON.stringify([e0r.range, e0.range]))
    // C. 1200-level legal snapshot, USD-M cap 1000 transmitted: supplied coverage is the 1000th level, not the snapshot range.
    { const li0 = (await bookState()).lastInput
      check('coverage_truthful_top_1000_of_1005_after_inside_inserts', /Supplied range 1,990\.05–2,009\.95;/.test(d.note) && li0?.bids === 1000 && li0?.asks === 1000, d.note + JSON.stringify(li0)) }
    // H. both sides visible on desktop (side panel) before any manual scroll.
    await closeMenu(); await sleep(200)
    let v = await bothSides()
    check('desktop_both_sides_visible_side_panel', bothOk(v) && v.side && v.panel[2] <= 360, JSON.stringify(v))
    await shot('book-replay-desktop-closed-menu.png')
    // D. served numeric: group=1 quote on the 700-level ETH book (edge bucket 1995 holds one supplied level).
    await setParams('orderbook', { group: 1, unit: 'quote' })
    await waitFor(`${B}.state().plugins[0]?.unit === 'USDT' && ${B}.state().plugins[0].rows > 0`, 15000)
    d = await dom()
    const gq = (() => {
      const b = new Map(), a = new Map()
      for (const p of INS_B) b.set(Math.floor(p), (b.get(Math.floor(p)) ?? 0) + p)
      for (const p of INS_A) a.set(Math.ceil(p), (a.get(Math.ceil(p)) ?? 0) + p)
      for (let i = 0; i < 995; i++) {
        const bp = +(2000 - 0.01 * (i + 1)).toFixed(2), ap = +(2000 + 0.01 * (i + 1)).toFixed(2)
        const bq = +(0.5 + (i % 7)).toFixed(3), aq = +(0.25 + (i % 5)).toFixed(3)
        const bk = Math.floor(bp), ak = Math.ceil(ap)
        b.set(bk, (b.get(bk) ?? 0) + bp * bq); a.set(ak, (a.get(ak) ?? 0) + ap * aq)
      }
      const qf = x => x >= 1000 ? fmt(x, 0) : x >= 1 ? fmt(x, 3) : fmt(x, 4)
      return [...[...a].sort((x, y) => y[0] - x[0]).map(([p, q]) => ['', fmt(p, 0), qf(q)]), ...[...b].sort((x, y) => y[0] - x[0]).map(([p, q]) => [qf(q), fmt(p, 0), ''])]
    })()
    check('served_grouped_quote_exact_with_edge_bucket', JSON.stringify(d.rows) === JSON.stringify(gq) && /qty USDT/.test(d.title), `got=${JSON.stringify(d.rows)} want=${JSON.stringify(gq)}`)
    // D. statically permitted but too-wide group: recoverable fault in panel AND menu, no price-0 row.
    await setParams('orderbook', { group: 10000, unit: 'base' })
    await waitFor(`${B}.state().plugins[0]?.fault === 'plugin_error'`, 15000)
    d = await dom()
    const HINT = 'fault parameters rejected — edit parameters'
    const bs = await bookState()
    check('invalid_group_safe_public_classification', bs.plugins[0].publicCode === 'invalid_params' && bs.plugins[0].state === 'ready', JSON.stringify(bs.plugins[0]))
    check('invalid_group_visible_hint_no_rows', d.rows.length === 0 && d.title.includes(HINT) && !/smaller group|wider than/.test(d.title), JSON.stringify(d))
    await waitFor(`(document.querySelector('.plugin-menu-row[data-plugin="orderbook"]')?.textContent ?? '').includes(${JSON.stringify(HINT)})`, 5000).catch(() => {})
    const mm = await menuMeta('orderbook')
    check('invalid_group_menu_hint_async', mm.includes(HINT) && !/smaller group|wider than/.test(mm), mm)
    await setParams('orderbook', { group: 0, unit: 'base' })
    await waitFor(`${B}.state().plugins[0]?.fault === null && ${B}.state().plugins[0].unit === 'ETH' && ${B}.state().plugins[0].rows > 0`, 15000)
    d = await dom()
    check('invalid_group_recovered_by_params_edit', JSON.stringify(d.rows) === JSON.stringify(want), JSON.stringify(d.rows.slice(0, 2)))
    check('menu_fault_cleared_async', !/fault|parameters rejected/.test(await menuMeta('orderbook')) && !/fault|parameters rejected/.test(d.title), await menuMeta('orderbook'))
    await closeMenu()
    // N. Manual scroll under live updates: enough rows to overflow, real wheel
    // input moves each section away from its spread edge; ticks must not reset it.
    await setParams('orderbook', { rows: 100, group: 0, unit: 'base' })
    await waitFor(`${B}.state().plugins[0]?.rows === 200`, 15000)
    await closeMenu(); await sleep(300)
    const sec = side => `document.querySelector('#book-panel .book-${side}')`
    const geo = side => evaluate(`(() => { const e = ${sec(side)}; const r = e.getBoundingClientRect(); return { top: e.scrollTop, max: e.scrollHeight - e.clientHeight, x: r.left + r.width / 2, y: r.top + r.height / 2, h: e.clientHeight, sh: e.scrollHeight, n: e.children.length } })()`)
    // Wheel scrolling is animated: wait until the position is stable.
    const settled = async side => { let a = await geo(side); for (let i = 0; i < 40; i++) { await sleep(150); const b = await geo(side); if (b.top === a.top) return b; a = b } return a }
    for (const [side, dy] of [['bids', 240], ['asks', -240]]) {
      const g0 = await geo(side)
      check(`scroll_${side}_section_overflows`, g0.max > 100, JSON.stringify(g0))
      await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: g0.x, y: g0.y, deltaX: 0, deltaY: dy })
      const g1 = await settled(side)
      const offEdge = g1.top > 2 && g1.top < g1.max - 2
      check(`scroll_${side}_moved_off_spread_edge`, offEdge && Math.abs(g1.top - g0.top) > 20, JSON.stringify([g0, g1]))
      const s0 = (await bookState()), r0n = s0.counters.renders
      await waitFor(`${B}.state().seq >= ${s0.seq + 3} && ${B}.state().counters.renders >= ${r0n + 3} && ${B}.state().plugins[0].seq >= ${s0.seq + 2}`, 15000)
      const g2 = await geo(side)
      check(`scroll_${side}_kept_under_live_ticks`, Math.abs(g2.top - g1.top) <= 1, JSON.stringify([g1.top, g2.top]))
    }
    // User scrolls both sections back to their spread edges (pinned again).
    for (const [side, dy] of [['bids', -20000], ['asks', 20000]]) { const g = await geo(side); await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: g.x, y: g.y, deltaX: 0, deltaY: dy }); const e = await settled(side); check(`scroll_${side}_returned_to_spread_edge`, side === 'bids' ? e.top <= 2 : e.max - e.top <= 2, JSON.stringify(e)) }
    // O. Unsupported-`safe` fallback: force the effective pre-`safe` rule, then
    // require far asks reachable, best ask pinned by default, manual scroll kept
    // under live ticks and both sides visible.
    const fb = await evaluate(`(() => { const s = document.createElement('style'); s.id = 'no-safe-sim'; s.textContent = '#book-panel .book-asks { align-content: start !important }'; document.head.appendChild(s); const e = ${sec('asks')}; e.scrollTop = e.scrollHeight; return getComputedStyle(e).alignContent })()`)
    check('fallback_effective_align_is_start', fb === 'start', fb)
    await sleep(300)
    const fa = await settled('asks')
    check('fallback_asks_overflow_and_pinned_to_best', fa.max > 100 && fa.max - fa.top <= 2, JSON.stringify(fa))
    v = await bothSides()
    check('fallback_both_sides_visible_default', bothOk(v), JSON.stringify(v))
    { const g = await geo('asks'); await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: g.x, y: g.y, deltaX: 0, deltaY: -20000 }) }
    const ft = await settled('asks')
    const far = await evaluate(`(() => { const e = ${sec('asks')}, r = e.getBoundingClientRect(), c = e.firstElementChild.firstElementChild.getBoundingClientRect(); return { top: e.scrollTop, inBox: c.top >= r.top - 1 && c.bottom <= r.bottom + 1 } })()`)
    check('fallback_far_ask_reachable_by_manual_scroll', ft.top <= 2 && far.inBox, JSON.stringify([ft, far]))
    { const s0 = await bookState(); await waitFor(`${B}.state().seq >= ${s0.seq + 3} && ${B}.state().counters.renders >= ${s0.counters.renders + 3}`, 15000) }
    const fk = await geo('asks')
    check('fallback_manual_scroll_kept_under_live_ticks', Math.abs(fk.top - ft.top) <= 1, JSON.stringify([ft.top, fk.top]))
    await evaluate(`(() => { document.getElementById('no-safe-sim').remove(); const e = ${sec('asks')}; e.scrollTop = e.scrollHeight; e.dispatchEvent(new Event('scroll')); return true })()`)
    await sleep(300)
    // P/Q observers fire on the semantic event: a new row section
    // ([data-scroll]) inserted anywhere under #book-panel (persistent boxes no
    // longer replace root children); title/age-only text mutations are ignored.
    const ROWS_INSERTED = `ms.some(m => [...m.addedNodes].some(n => n.nodeType === 1 && (n.matches('[data-scroll]') || n.querySelector('[data-scroll]'))))`
    // P. Pin survives render -> queued programmatic-scroll event -> resize.
    // LABELLED LAYOUT FAULT INJECTION: a real live tick renders real WASM rows
    // and pin() restores the spread edge; in the microtask right after that
    // render (before the frame that delivers its queued scroll event) the asks
    // box is shrunk the way the mobile viewport shrinks it. Rows are never
    // filled directly; only the box height is changed.
    {
      const p0 = await settled('asks')
      await evaluate(`new Promise(res => { const mo = new MutationObserver(ms => { if (!${ROWS_INSERTED}) return; mo.disconnect(); const s = document.createElement('style'); s.id = 'resize-sim'; s.textContent = '#book-panel .book-asks { flex: none !important; height: 60px !important }'; document.head.appendChild(s); res(true) }); mo.observe(document.getElementById('book-panel'), { childList: true, subtree: true }) })`)
      const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 3}`, 15000)
      const p1 = await settled('asks')
      check('pin_kept_through_queued_scroll_then_resize', p1.max > p0.max + 20 && p1.max - p1.top <= 2, JSON.stringify([p0, p1]))
      await evaluate(`document.getElementById('resize-sim').remove()`)
      const s1 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s1.counters.renders + 3}`, 15000)
      const p2 = await settled('asks')
      check('pin_kept_after_resize_back', p2.max - p2.top <= 2, JSON.stringify(p2))
    }
    // Q. Genuine away-and-return before the next render is kept.
    // LABELLED SCROLL ORDERING INJECTION: after a real live render where pin()
    // restored a manual offset R on the asks element, two user-intent scrolls
    // on that same connected element are dispatched synchronously: away to
    // A = R - 60, then back to exactly R. Rows are real plugin output.
    {
      await evaluate(`(() => { const e = ${sec('asks')}; e.scrollTop = e.scrollHeight - e.clientHeight - 150; e.dispatchEvent(new Event('scroll')); return true })()`)
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const q = await evaluate(`new Promise(res => { const mo = new MutationObserver(ms => { if (!${ROWS_INSERTED}) return; mo.disconnect(); const e = ${sec('asks')}; const r = e.scrollTop; e.scrollTop = r - 60; const a = e.scrollTop; e.dispatchEvent(new Event('scroll')); e.scrollTop = r; e.dispatchEvent(new Event('scroll')); res({ r, a, back: e.scrollTop }) }); mo.observe(document.getElementById('book-panel'), { childList: true, subtree: true }) })`)
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 3}`, 15000) }
      const q1 = await geo('asks')
      check('return_to_restored_offset_kept_after_away_scroll', q.r > 100 && q.r - q.a === 60 && q.back === q.r && Math.abs(q1.top - q.r) <= 1, JSON.stringify([q, q1.top]))
      await evaluate(`(() => { const e = ${sec('asks')}; e.scrollTop = e.scrollHeight; e.dispatchEvent(new Event('scroll')); return true })()`)
      await sleep(300)
    }
    // R. Saved manual offset survives a layout CLAMP and returns with geometry.
    // REAL wheel sets the offset S; LABELLED LAYOUT INJECTION grows the asks box
    // so its max scroll drops below S (browser clamps), live ticks render, then
    // the box is restored. Required non-vacuity: max_after_resize < S.
    {
      { const g = await geo('asks'); await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: g.x, y: g.y, deltaX: 0, deltaY: -240 }) }
      const r0 = await settled('asks')
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const r1 = await settled('asks')
      const hNew = r1.sh - Math.floor(r1.top / 2)
      await evaluate(`(() => { const s = document.createElement('style'); s.id = 'clamp-sim'; s.textContent = '#book-panel .book-asks { flex: none !important; height: ${hNew}px !important }'; document.head.appendChild(s); return true })()`)
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const r2 = await settled('asks')
      check('clamp_witness_max_below_saved_offset', r1.top > 20 && r1.top < r1.max - 20 && Math.abs(r1.top - r0.top) <= 1 && r2.max < r1.top && r2.top <= r2.max, JSON.stringify([r0, r1, r2]))
      await evaluate(`document.getElementById('clamp-sim').remove()`)
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const r3 = await settled('asks')
      check('saved_offset_restored_after_clamp_geometry_returns', r3.max === r1.max && Math.abs(r3.top - r1.top) <= 1, JSON.stringify([r1, r2, r3]))
      await evaluate(`(() => { const e = ${sec('asks')}; e.scrollTop = e.scrollHeight; e.dispatchEvent(new Event('scroll')); return true })()`)
      await sleep(300)
    }
    // R-1px. Same as R with a ONE-PIXEL clamp: the asks box is sized so its
    // new max scroll is exactly S-1 (measured clientHeight, corrected once).
    // Required non-vacuity: S - max_after_resize === 1.
    {
      { const g = await geo('asks'); await call('Input.dispatchMouseEvent', { type: 'mouseWheel', x: g.x, y: g.y, deltaX: 0, deltaY: -240 }) }
      const p0 = await settled('asks')
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const p1 = await settled('asks')
      const size = (h) => evaluate(`(() => { let s = document.getElementById('clamp-sim-1px'); if (!s) { s = document.createElement('style'); s.id = 'clamp-sim-1px'; document.head.appendChild(s) } s.textContent = '#book-panel .book-asks { flex: none !important; height: ${h}px !important }'; const e = ${sec('asks')}; return e.scrollHeight - e.clientHeight })()`)
      let h = p1.sh - p1.top + 1, m = await size(h)
      if (m !== p1.top - 1) { h += m - (p1.top - 1); m = await size(h) }
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const p2 = await settled('asks')
      check('one_pixel_clamp_witness_max_one_below_saved_offset', p1.top > 20 && p1.top < p1.max - 20 && Math.abs(p1.top - p0.top) <= 1 && p1.top - p2.max === 1 && p2.top <= p2.max, JSON.stringify([p0, p1, p2, h]))
      await evaluate(`document.getElementById('clamp-sim-1px').remove()`)
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 2}`, 15000) }
      const p3 = await settled('asks')
      check('saved_offset_restored_after_one_pixel_clamp_geometry_returns', p3.max === p1.max && p3.top === p1.top, JSON.stringify([p1, p2, p3]))
      await evaluate(`(() => { const e = ${sec('asks')}; e.scrollTop = e.scrollHeight; e.dispatchEvent(new Event('scroll')); return true })()`)
      await sleep(300)
    }
    await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
    await waitFor(`${B}.state().plugins[0]?.rows === 40`, 15000)
    await closeMenu(); await sleep(300)
    const e1 = await basis()
    check('eth_book_ticks_fault_params_keep_values_and_viewport', sameValues(e0, e1) && JSON.stringify(e0.range) === JSON.stringify(e1.range), JSON.stringify([e0.range, e1.range]))
    // I. mobile width layout: no document overflow, legible rows.
    await call('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true })
    await sleep(400)
    const ov = await evaluate(`({ sw: document.documentElement.scrollWidth, iw: innerWidth, pw: document.getElementById('book-panel').getBoundingClientRect().width })`)
    check('mobile_no_document_overflow', ov.sw <= ov.iw && ov.pw <= ov.iw, JSON.stringify(ov))
    v = await bothSides()
    check('mobile_both_sides_visible_stacked', bothOk(v) && !v.side && v.panel[3] <= v.ih * 0.5 + 1, JSON.stringify(v))
    const fsz = await evaluate(`parseFloat(getComputedStyle(document.querySelector('#book-panel .book-price')).fontSize)`)
    check('mobile_readable_font', fsz >= 11, String(fsz))
    await shot('book-replay-mobile.png')
    await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
    await sleep(300)
    await shot('book-replay-desktop.png')
    pumping = false; await pump
    check('eth_feed_advanced_while_exercised', (await bookState()).seq > 501, String((await bookState()).seq))
    // S. M1 deep spot slice: binance/spot/BTC with a SYNTHETIC 5000-level/side
    // snapshot (tick 0.01 => bid floor 29,950.00) through the served /book
    // socket into the IMPORTED order-book WASM. A decisive deep bid (index
    // 4499, beyond both 500 and 1000) only reaches the rendered grouped bucket
    // when all 5000 levels are carried end to end.
    await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
    await mailbox({ op: 'snapshot', sym: 'spot:BTCUSDT', id: 7000, mid: 30000, tick: 0.01, levels: 5000 })
    const spotSockets0 = sockets.filter(u => u.includes('/book') && u.includes('market=spot')).length
    await selectSource('spot', 'BTC')
    await waitFor(`${B}.state().source?.market === 'spot' && ${B}.state().source?.symbol === 'BTC'`, 10000)
    for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects['spot:BTCUSDT'] ?? 0) > 0 && JSON.stringify(st.active) === JSON.stringify([['binance/spot/BTC', 1]])) break; await sleep(50) }
    check('spot_btc_distinct_producer_from_usdm', JSON.stringify(st.active) === JSON.stringify([['binance/spot/BTC', 1]]) && (st.connects['spot:BTCUSDT'] ?? 0) === 1
      && sockets.filter(u => u.includes('/book') && u.includes('market=spot') && u.includes('symbol=BTC')).length === spotSockets0 + 1, JSON.stringify([st.active, st.connects]))
    await sleep(100)
    // Spot bridge: U <= L+1 <= u, no `pu` field (venue shape).
    check('spot_bridge_event_sent', (await mailbox({ op: 'event', sym: 'spot:BTCUSDT', U: 6995, u: 7005, b: [['29955.00', '777.5']], a: [] })).ok)
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows > 0 && ${B}.state().source?.market === 'spot'`, 20000)
    // Official overlap (local 7005, U 7003/u 7010): applied, not a gap.
    await mailbox({ op: 'event', sym: 'spot:BTCUSDT', U: 7003, u: 7010, b: [['29999.99', '4.321']], a: [] })
    await waitFor(`${B}.state().seq === 7010 && ${B}.state().plugins[0].seq === 7010`, 10000)
    let spotU = 7010, spotPump = true
    const pumpSpot = (async () => { while (spotPump) { await mailbox({ op: 'event', sym: 'spot:BTCUSDT', U: spotU + 1, u: spotU + 1, b: [], a: [] }); spotU++; await sleep(600) } })()
    d = await dom()
    want = expectedRows(30000, 0.01, 5000, [['b', 29955, 777.5], ['b', 29999.99, 4.321]]).map(r => [q3(r[0]), r[1], q3(r[2])])
    check('spot_overlap_applied_rows_exact', JSON.stringify(d.rows) === JSON.stringify(want) && /live/.test(d.status), `got=${JSON.stringify(d.rows.slice(18, 22))} want=${JSON.stringify(want.slice(18, 22))} status=${d.status}`)
    check('spot_labels_venue_deep_coverage', /Binance spot BTCUSDT/.test(d.venue) && /qty BTC/.test(d.title) && /Supplied range 29,950\.00–30,050\.00;/.test(d.note) && /Depth 5000\/5000 levels per side \(deep\)/.test(d.note) && /edge bucket sums only supplied levels/.test(d.note), JSON.stringify(d))
    let li = (await bookState()).lastInput
    check('spot_wasm_input_5000_per_side', li?.bids === 5000 && li?.asks === 5000, JSON.stringify(li))
    // Grouped 10 (base): bucket 29,950 is reachable only from levels 4040..4999
    // and carries the decisive 777.5 at index 4499. Independent expectation.
    await setParams('orderbook', { rows: 20, group: 10, unit: 'base' })
    await waitFor(`${B}.state().plugins[0]?.fault === null && ${B}.state().plugins[0].rows === 10`, 15000)
    d = await dom()
    const gs = (() => {
      const b = new Map(), a = new Map()
      for (let i = 0; i < 5000; i++) {
        const bp = +(30000 - 0.01 * (i + 1)).toFixed(2), ap = +(30000 + 0.01 * (i + 1)).toFixed(2)
        let bq = +(0.5 + (i % 7)).toFixed(3); const aq = +(0.25 + (i % 5)).toFixed(3)
        if (bp === 29955) bq = 777.5
        if (bp === 29999.99) bq = 4.321
        const bk = Math.floor(bp / 10) * 10, ak = Math.ceil(ap / 10) * 10
        b.set(bk, (b.get(bk) ?? 0) + bq); a.set(ak, (a.get(ak) ?? 0) + aq)
      }
      const qf = x => x >= 1000 ? fmt(x, 0) : x >= 1 ? fmt(x, 3) : fmt(x, 4)
      return { rows: [...[...a].sort((x, y) => y[0] - x[0]).map(([p, q]) => ['', fmt(p, 0), qf(q)]), ...[...b].sort((x, y) => y[0] - x[0]).map(([p, q]) => [qf(q), fmt(p, 0), ''])], deep: b.get(29950) }
    })()
    check('spot_grouped_deep_bucket_exact', JSON.stringify(d.rows) === JSON.stringify(gs.rows) && gs.deep > 777.5, `got=${JSON.stringify(d.rows)} want=${JSON.stringify(gs.rows)}`)
    check('spot_raw_bbo_spread_precision_kept_when_grouped', /bid 29,999\.99 · spread 0\.02 · ask 30,000\.01/.test(d.spread), d.spread)
    li = (await bookState()).lastInput
    console.log(`NOTE spot5000 input=${JSON.stringify(li)}`)
    await shot('book-replay-spot-5000.png')
    await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
    await closeMenu()
    spotPump = false; await pumpSpot
    // M2. All TEN Binance keys through served /book -> imported WASM -> DOM.
    // Independent expectations: legal SYNTHETIC snapshot per key with a
    // distinct mid; base qty = raw (linear) or contracts*face/LEVEL price
    // (COIN-M, face from the pinned public exchangeInfo excerpt); quote = base*price.
    const TEN = [
      ['spot', 'BTC', 'spot:BTCUSDT', 'BTCUSDT', 'spot', 'USDT', 31000, 0.1, 5000, null],
      ['spot', 'ETH', 'spot:ETHUSDT', 'ETHUSDT', 'spot', 'USDT', 2100, 0.01, 5000, null],
      ['spot_usdc', 'BTC', 'spot:BTCUSDC', 'BTCUSDC', 'spot', 'USDC', 32000, 0.1, 5000, null],
      ['spot_usdc', 'ETH', 'spot:ETHUSDC', 'ETHUSDC', 'spot', 'USDC', 2200, 0.01, 5000, null],
      ['usdm', 'BTC', 'BTCUSDT', 'BTCUSDT', 'USDⓈ-M perpetual', 'USDT', 33000, 0.1, 1000, null],
      ['usdm', 'ETH', 'ETHUSDT', 'ETHUSDT', 'USDⓈ-M perpetual', 'USDT', 2300, 0.01, 1000, null],
      ['usdm_usdc', 'BTC', 'BTCUSDC', 'BTCUSDC', 'USDⓈ-M perpetual', 'USDC', 34000, 0.1, 1000, null],
      ['usdm_usdc', 'ETH', 'ETHUSDC', 'ETHUSDC', 'USDⓈ-M perpetual', 'USDC', 2400, 0.01, 1000, null],
      ['coinm', 'BTC', 'coinm:BTCUSD_PERP', 'BTCUSD_PERP', 'COIN-M perpetual', 'USD', 35000, 0.1, 1000, 100],
      ['coinm', 'ETH', 'coinm:ETHUSD_PERP', 'ETHUSD_PERP', 'COIN-M perpetual', 'USD', 2500, 0.01, 1000, 10],
    ]
    const qf = x => x >= 1000 ? fmt(x, 0) : x >= 1 ? fmt(x, 3) : fmt(x, 4)
    const expectKey = (mid, tick, face, unit) => {
      const lv = side => Array.from({ length: 20 }, (_, i) => { const p = +(side === 'b' ? mid - tick * (i + 1) : mid + tick * (i + 1)).toFixed(2)
        const raw = side === 'b' ? 0.5 + (i % 7) : 0.25 + (i % 5); const base = face ? raw * face / p : raw; return [p, unit === 'quote' ? base * p : base] })
      const b = lv('b'), a = lv('a'); const d = Math.max(0, ...[...a, ...b].map(([p]) => (String(p).split('.')[1] ?? '').length))
      return [...a.reverse().map(([p, q]) => ['', fmt(p, d), qf(q)]), ...b.map(([p, q]) => [qf(q), fmt(p, d), ''])]
    }
    let tenId = 20000
    for (const [m, s, sk, native, vlabel, quote, mid, tick, cap, face] of TEN) {
      const tag = `${m}_${s}`; const id0 = tenId += 1000
      const LEVELS = cap === 5000 ? 1200 : 1000
      await mailbox({ op: 'snapshot', sym: sk, id: id0, mid, tick, levels: LEVELS })
      const c0 = (await mailbox({ op: 'state' })).connects[sk] ?? 0
      await selectSource(m, s)
      await waitFor(`${B}.state().source?.market === ${JSON.stringify(m)} && ${B}.state().source?.symbol === ${JSON.stringify(s)}`, 10000)
      for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects[sk] ?? 0) > c0 && JSON.stringify(st.active) === JSON.stringify([[`binance/${m}/${s}`, 1]])) break; await sleep(50) }
      check(`ten_${tag}_exact_producer_demand`, JSON.stringify(st.active) === JSON.stringify([[`binance/${m}/${s}`, 1]]) && st.connects[sk] === c0 + 1, JSON.stringify([st.active, st.connects]))
      await sleep(100)
      const ev = m.startsWith('spot') ? { U: id0 - 2, u: id0 + 3 } : { U: id0 - 2, u: id0 + 3, pu: id0 - 10 }
      check(`ten_${tag}_bridge_sent`, (await mailbox({ op: 'event', sym: sk, b: [], a: [], ...ev })).ok)
      await waitFor(`${B}.state().status.state === 'live' && ${B}.state().seq === ${id0 + 3} && ${B}.state().plugins[0].rows > 0 && ${B}.state().source?.market === ${JSON.stringify(m)}`, 15000)
      let dd = await dom(); const li = (await bookState()).lastInput; const want = Math.min(cap, LEVELS)
      check(`ten_${tag}_wasm_input_capped_${want}`, li?.bids === want && li?.asks === want, JSON.stringify(li))
      check(`ten_${tag}_base_rows_exact`, JSON.stringify(dd.rows) === JSON.stringify(expectKey(mid, tick, face, 'base')), `got=${JSON.stringify(dd.rows.slice(18, 22))} want=${JSON.stringify(expectKey(mid, tick, face, 'base').slice(18, 22))}`)
      check(`ten_${tag}_labels`, dd.venue.includes(`Binance ${vlabel} ${s}${quote}`) && new RegExp(`qty ${s}\\b`).test(dd.title), JSON.stringify([dd.venue, dd.title]))
      await setParams('orderbook', { rows: 20, group: 0, unit: 'quote' })
      await waitFor(`${B}.state().plugins[0]?.unit === ${JSON.stringify(quote)} && ${B}.state().plugins[0].rows > 0`, 15000).catch(() => {})
      dd = await dom()
      check(`ten_${tag}_quote_rows_exact`, JSON.stringify(dd.rows) === JSON.stringify(expectKey(mid, tick, face, 'quote')) && new RegExp(`qty ${quote}\\b`).test(dd.title), `got=${JSON.stringify(dd.rows.slice(18, 22))} want=${JSON.stringify(expectKey(mid, tick, face, 'quote').slice(18, 22))} title=${dd.title}`)
      await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
      await waitFor(`${B}.state().plugins[0]?.unit === ${JSON.stringify(s)}`, 15000).catch(() => {})
      await closeMenu()
    }
    // M2 concurrent producers: four simultaneous external subscribers on
    // spot vs USD-M and USDT vs USDC BTC books. Each must receive only its own
    // source with its own distinct values; a diff to one producer must not
    // reach the others.
    {
      const CK = [['spot', 'spot:BTCUSDT', 31000], ['usdm', 'BTCUSDT', 33000], ['spot_usdc', 'spot:BTCUSDC', 32000], ['usdm_usdc', 'BTCUSDC', 34000]]
      const c0 = (await mailbox({ op: 'state' })).connects
      const subs = CK.map(([m]) => { const s = new WebSocket(`ws://127.0.0.1:${info.port}/book?exchange=binance&market=${m}&symbol=BTC`); const frames = []; s.onmessage = e => frames.push(JSON.parse(e.data)); return { m, s, frames } })
      for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if (CK.every(([, sk]) => (st.connects[sk] ?? 0) > (c0[sk] ?? 0))) break; await sleep(50) }
      const act = Object.fromEntries(st.active)
      check('concurrent_four_independent_producers', CK.every(([m, sk]) => (act[`binance/${m}/BTC`] ?? 0) >= 1 && st.connects[sk] === (c0[sk] ?? 0) + 1), JSON.stringify([st.active, st.connects]))
      await sleep(150)
      for (const [m, sk] of CK) await mailbox({ op: 'event', sym: sk, b: [], a: [], ...(m.startsWith('spot') ? { U: 1, u: 60000 } : { U: 1, u: 60000, pu: 1 }) })
      const live = sub => sub.frames.filter(f => f.status === 'live' && f.bids.length)
      await bounded((async () => { while (!subs.every(x => live(x).length)) await sleep(50) })(), 15000, 'concurrent live').catch(() => {})
      const bb = subs.map(x => { const f = live(x).at(-1); return f && [f.source.market, f.native_symbol, +f.bids[0][0], f.seq] })
      check('concurrent_sources_distinct_values_no_cross_admission', subs.every((x, i) => x.frames.every(f => f.source.market === x.m && f.source.symbol === 'BTC') && bb[i] && bb[i][2] === +(CK[i][2] - 0.1).toFixed(2)) && new Set(bb.map(v => v && v[2])).size === 4, JSON.stringify(bb))
      const seqs = subs.map(x => live(x).at(-1)?.seq)
      await mailbox({ op: 'event', sym: 'spot:BTCUSDT', U: 60001, u: 60001, b: [['30999.90', '424.242']], a: [] })
      await bounded((async () => { while (!live(subs[0]).some(f => f.seq === 60001)) await sleep(50) })(), 10000, 'spot diff').catch(() => {})
      await sleep(400)
      const has = x => live(x).some(f => f.bids.some(([, q]) => Math.abs(+q - 424.242) < 1e-9))
      check('concurrent_diff_isolated_to_its_producer', has(subs[0]) && subs.slice(1).every((x, i) => !has(x) && live(x).at(-1)?.seq === seqs[i + 1]), JSON.stringify([seqs, subs.map(x => live(x).at(-1)?.seq), subs.map(has)]))
      for (const x of subs) x.s.close()
      await sleep(600)
    }
    // M3A Coinbase Exchange spot (SYNTHETIC level2_batch shapes): full WS
    // snapshot on the same stream (no REST, no upstream seq), absolute
    // l2update replacement/zero delete, nearest-5000 window, internal rev.
    {
      await evaluate(`(() => { const e = document.getElementById('exchange'); e.value = 'coinbase'; e.dispatchEvent(new Event('change')); return e.value })()`)
      const CB = [['BTC', 'BTC-USD', 70000, 0.01, 6000, 21000], ['ETH', 'ETH-USD', 2290, 0.01, 5200, 13275]]
      const expectCb = (mid, tick, unit, bid0, delAsk0) => {
        const lv = side => { const out = []; for (let i = 0; out.length < 20; i++) { if (side === 'a' && delAsk0 && i === 0) continue
          const p = +(side === 'b' ? mid - tick * (i + 1) : mid + tick * (i + 1)).toFixed(2)
          let q = side === 'b' ? 0.5 + (i % 7) * 0.125 : 0.25 + (i % 7) * 0.125; if (side === 'b' && i === 0 && bid0 != null) q = bid0
          out.push([p, unit === 'quote' ? q * p : q]) } return out }
        const b = lv('b'), a = lv('a')
        return [...a.reverse().map(([p, q]) => ['', fmt(p, 2), qf(q)]), ...b.map(([p, q]) => [qf(q), fmt(p, 2), ''])]
      }
      for (const [s, native, mid, tick, nb, na] of CB) {
        const sk = `coinbase:${native}`; const tag = `coinbase_spot_${s}`
        const c0 = (await mailbox({ op: 'state' })).connects[sk] ?? 0
        await selectSource('spot', s)
        await waitFor(`${B}.state().source?.exchange === 'coinbase' && ${B}.state().source?.symbol === ${JSON.stringify(s)}`, 10000)
        for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects[sk] ?? 0) > c0) break; await sleep(50) }
        check(`${tag}_exact_producer_demand`, JSON.stringify(st.active) === JSON.stringify([[`coinbase/spot/${s}`, 1]]) && st.connects[sk] === c0 + 1 && sockets.some(u => u.includes('/book') && u.includes('exchange=coinbase') && u.includes(`symbol=${s}`)), JSON.stringify([st.active, st.connects]))
        check(`${tag}_snapshot_sent`, (await mailbox({ op: 'cb_snapshot', sym: sk, mid, tick, bids: nb, asks: na })).ok)
        await sleep(300)
        check(`${tag}_syncing_until_timestamped_update`, (await bookState()).status.state !== 'live', JSON.stringify((await bookState()).status))
        check(`${tag}_update_sent`, (await mailbox({ op: 'cb_update', sym: sk, changes: [['buy', (mid - tick).toFixed(2), '3.50000000'], ['sell', (mid + tick).toFixed(2), '0']] })).ok)
        await waitFor(`${B}.state().status.state === 'live' && ${B}.state().seq === 1 && ${B}.state().plugins[0].rows > 0 && ${B}.state().source?.exchange === 'coinbase'`, 15000)
        let dd = await dom(); const li = (await bookState()).lastInput
        // Anchored window: nearest 5000/side from the deep snapshot; the zero delete
        // leaves 4999 asks inside the window (never refilled from unknown outside).
        check(`${tag}_wasm_input_window_5000_4999`, li?.bids === 5000 && li?.asks === 4999, JSON.stringify(li))
        check(`${tag}_base_rows_absolute_and_delete`, JSON.stringify(dd.rows) === JSON.stringify(expectCb(mid, tick, 'base', 3.5, true)), `got=${JSON.stringify(dd.rows.slice(18, 22))} want=${JSON.stringify(expectCb(mid, tick, 'base', 3.5, true).slice(18, 22))}`)
        check(`${tag}_labels_native_internal_rev`, dd.venue.includes(`Coinbase Exchange spot ${native}`) && new RegExp(`qty ${s}\\b`).test(dd.title) && /internal rev 1\b/.test(dd.note) && /Depth 5000\/4999 levels per side \(deep\)/.test(dd.note)
          && dd.note.includes(`Supplied range ${fmt(+(mid - tick * 5000).toFixed(2), 2)}–${fmt(+(mid + tick * 5000).toFixed(2), 2)};`), JSON.stringify([dd.venue, dd.title, dd.note]))
        await setParams('orderbook', { rows: 20, group: 0, unit: 'quote' })
        await waitFor(`${B}.state().plugins[0]?.unit === 'USD' && ${B}.state().plugins[0].rows > 0`, 15000).catch(() => {})
        dd = await dom()
        check(`${tag}_quote_usd_rows_exact`, JSON.stringify(dd.rows) === JSON.stringify(expectCb(mid, tick, 'quote', 3.5, true)) && /qty USD\b/.test(dd.title), `got=${JSON.stringify(dd.rows.slice(18, 22))} title=${dd.title}`)
        await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
        await waitFor(`${B}.state().plugins[0]?.unit === ${JSON.stringify(s)}`, 15000).catch(() => {})
        // Absolute replacement (not delta) of the same level.
        await mailbox({ op: 'cb_update', sym: sk, dt_ms: 5, changes: [['buy', (mid - tick).toFixed(2), '1.25000000']] })
        await waitFor(`${B}.state().seq === 2 && ${B}.state().plugins[0].rows > 0`, 15000).catch(() => {})
        await sleep(300)
        dd = await dom()
        check(`${tag}_absolute_replace_rev2`, JSON.stringify(dd.rows) === JSON.stringify(expectCb(mid, tick, 'base', 1.25, true)), JSON.stringify(dd.rows.slice(19, 21)))
        // M3A closure: Coinbase-specific deep numeric oracle. Bid index 1500
        // (beyond 1000, inside the retained 5000 window) gets a distinctive
        // absolute size twice (9 then 4321.5: replacement, no double add);
        // a bid beyond the window (index 5200, present in the snapshot but
        // outside the anchored 5000) must not contribute.
        const deepP = +(mid - tick * 1501).toFixed(2), outP = +(mid - tick * 5201).toFixed(2)
        await mailbox({ op: 'cb_update', sym: sk, dt_ms: 10, changes: [['buy', deepP.toFixed(2), '9.00000000']] })
        await mailbox({ op: 'cb_update', sym: sk, dt_ms: 15, changes: [['buy', deepP.toFixed(2), '4321.50000000'], ['buy', outP.toFixed(2), '999999.00000000']] })
        await waitFor(`${B}.state().seq >= 4`, 15000).catch(() => {})
        await setParams('orderbook', { rows: 20, group: 10, unit: 'base' })
        await waitFor(`${B}.state().plugins[0]?.fault === null && ${B}.state().plugins[0].rows === 10`, 15000).catch(() => {})
        await sleep(200)
        dd = await dom()
        const gcb = (() => {
          const b = new Map(), a = new Map()
          for (let i = 0; i < 5000; i++) {
            const bp = +(mid - tick * (i + 1)).toFixed(2), ap = +(mid + tick * (i + 1)).toFixed(2)
            let bq = 0.5 + (i % 7) * 0.125; if (i === 0) bq = 1.25; if (i === 1500) bq = 4321.5
            const bk = Math.floor(bp / 10) * 10; b.set(bk, (b.get(bk) ?? 0) + bq)
            if (i > 0) { const ak = Math.ceil(ap / 10) * 10; a.set(ak, (a.get(ak) ?? 0) + 0.25 + (i % 7) * 0.125) }
          }
          const k = Math.floor(deepP / 10) * 10
          return { rows: [...[...a].sort((x, y) => y[0] - x[0]).map(([p, q]) => ['', fmt(p, 0), qf(q)]), ...[...b].sort((x, y) => y[0] - x[0]).map(([p, q]) => [qf(q), fmt(p, 0), ''])], deep: b.get(k), k }
        })()
        check(`${tag}_deep_beyond1000_grouped_exact`, JSON.stringify(dd.rows) === JSON.stringify(gcb.rows) && gcb.deep > 4321.5 && gcb.deep < 9000 && dd.rows.some(r => r[1] === fmt(gcb.k, 0) && r[0] === qf(gcb.deep)), `got=${JSON.stringify(dd.rows)} want=${JSON.stringify(gcb.rows)}`)
        check(`${tag}_outside_window_not_counted`, !dd.rows.some(r => r[1] === fmt(Math.floor(outP / 10) * 10, 0)) && !JSON.stringify(dd.rows).includes('999,999'), JSON.stringify(dd.rows))
        await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
        await waitFor(`${B}.state().plugins[0]?.unit === ${JSON.stringify(s)}`, 15000).catch(() => {})
        await closeMenu()
      }
      // Positive cross-exchange isolation: Binance spot BTC and Coinbase spot
      // BTC share market+symbol, differ in exchange; simultaneous producers.
      await mailbox({ op: 'snapshot', sym: 'spot:BTCUSDT', id: 90000, mid: 31000, tick: 0.1, levels: 300 })
      const XS = [['binance', 'spot:BTCUSDT'], ['coinbase', 'coinbase:BTC-USD']]
      const c0 = (await mailbox({ op: 'state' })).connects
      const subs = XS.map(([ex]) => { const s = new WebSocket(`ws://127.0.0.1:${info.port}/book?exchange=${ex}&market=spot&symbol=BTC`); const frames = []; s.onmessage = e => frames.push(JSON.parse(e.data)); return { ex, s, frames } })
      for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if (XS.every(([, sk]) => (st.connects[sk] ?? 0) > (c0[sk] ?? 0))) break; await sleep(50) }
      const act = Object.fromEntries(st.active)
      check('xex_two_producers_same_market_symbol', (act['binance/spot/BTC'] ?? 0) >= 1 && (act['coinbase/spot/BTC'] ?? 0) >= 1 && XS.every(([, sk]) => st.connects[sk] === (c0[sk] ?? 0) + 1), JSON.stringify([st.active, st.connects]))
      await sleep(150)
      await mailbox({ op: 'event', sym: 'spot:BTCUSDT', U: 89999, u: 90001, b: [], a: [] })
      await mailbox({ op: 'cb_snapshot', sym: 'coinbase:BTC-USD', mid: 70000, tick: 0.01, bids: 300, asks: 300 })
      await mailbox({ op: 'cb_update', sym: 'coinbase:BTC-USD', changes: [['buy', '69999.99', '2.00000000']] })
      const live = x => x.frames.filter(f => f.status === 'live' && f.bids.length)
      await bounded((async () => { while (!subs.every(x => live(x).length)) await sleep(50) })(), 15000, 'xex live').catch(() => {})
      const snap = x => { const f = live(x).at(-1); return f && { ex: f.source.exchange, native: f.native_symbol, quote: f.quote, kind: f.seq_kind, seq: f.seq, gen: f.generation, bid: +f.bids[0][0] } }
      const s0 = subs.map(snap)
      check('xex_distinct_identity_values', s0[0]?.ex === 'binance' && s0[0]?.native === 'BTCUSDT' && s0[0]?.quote === 'USDT' && s0[0]?.kind === 'exchange_update_id' && s0[0]?.seq === 90001 && s0[0]?.bid === 30999.9
        && s0[1]?.ex === 'coinbase' && s0[1]?.native === 'BTC-USD' && s0[1]?.quote === 'USD' && s0[1]?.kind === 'internal_revision' && s0[1]?.seq === 1 && s0[1]?.bid === 69999.99
        && subs.every(x => x.frames.every(f => f.source.exchange === x.ex)), JSON.stringify(s0))
      await mailbox({ op: 'cb_update', sym: 'coinbase:BTC-USD', dt_ms: 9, changes: [['buy', '69999.98', '777.00000000']] })
      await bounded((async () => { while (!live(subs[1]).some(f => f.seq === 2)) await sleep(50) })(), 10000, 'cb rev2').catch(() => {})
      await mailbox({ op: 'event', sym: 'spot:BTCUSDT', U: 90002, u: 90002, b: [['30999.80', '555.555']], a: [] })
      await bounded((async () => { while (!live(subs[0]).some(f => f.seq === 90002)) await sleep(50) })(), 10000, 'bn diff').catch(() => {})
      await sleep(400)
      const has = (x, q) => live(x).some(f => f.bids.some(([, v]) => Math.abs(+v - q) < 1e-9))
      check('xex_updates_isolated_no_cross_admission', has(subs[1], 777) && !has(subs[0], 777) && has(subs[0], 555.555) && !has(subs[1], 555.555)
        && live(subs[1]).at(-1)?.seq === 2 && live(subs[0]).at(-1)?.seq === 90002, JSON.stringify(subs.map(snap)))
      for (const x of subs) x.s.close()
      await sleep(600)
    }
    // M3B Coinbase DRB linear USDC perpetuals (SYNTHETIC JSON-RPC envelopes:
    // ack, full WS snapshot with change_id, prev_change_id-chained changes;
    // amounts are BASE coin, never multiplied by contract_size). Base-unit
    // policy is tied to the recorded public instrument excerpt (fixture).
    {
      const meta = JSON.parse(fs.readFileSync(new URL('./fixtures/coinbase-drb-instruments-excerpt.json', import.meta.url), 'utf8'))
      const DRB = [['BTC', 'BTC_USDC-PERPETUAL', 78000, 0.5, 1, 6000, 6000, 1000], ['ETH', 'ETH_USDC-PERPETUAL', 2300, 0.05, 2, 5200, 5200, 100]]
      for (const [s, native] of DRB) {
        const m = meta.instruments.find(x => x.instrument_name === native)
        check(`drb_${s}_registered_linear_usdc_metadata`, !!m && m.instrument_type === 'linear' && m.future_type === 'linear' && m.kind === 'future' && m.settlement_period === 'perpetual'
          && m.base_currency === s && m.quote_currency === 'USDC' && m.settlement_currency === 'USDC' && m.contract_size === 0.0001, JSON.stringify(m))
      }
      const qfD = x => x >= 1000 ? fmt(x, 0) : x >= 1 ? fmt(x, 3) : fmt(x, 4)
      const rnd = x => +x.toFixed(2)
      const expectDrb = (mid, tick, pd, unit, bid0, delAsk0) => {
        const lv = side => { const out = []; for (let i = 0; out.length < 20; i++) { if (side === 'a' && delAsk0 && i === 0) continue
          const p = rnd(side === 'b' ? mid - tick * (i + 1) : mid + tick * (i + 1))
          let q = side === 'b' ? 0.5 + (i % 7) * 0.125 : 0.25 + (i % 7) * 0.125; if (side === 'b' && i === 0 && bid0 != null) q = bid0
          out.push([p, unit === 'quote' ? q * p : q]) } return out }
        const b = lv('b'), a = lv('a')
        return [...a.reverse().map(([p, q]) => ['', fmt(p, pd), qfD(q)]), ...b.map(([p, q]) => [qfD(q), fmt(p, pd), ''])]
      }
      for (const [s, native, mid, tick, pd, nb, na, G] of DRB) {
        const sk = `coinbase:${native}`; const tag = `drb_${s}`
        const c0 = (await mailbox({ op: 'state' })).connects[sk] ?? 0
        await selectSource('usdc', s)
        await waitFor(`${B}.state().source?.exchange === 'coinbase' && ${B}.state().source?.market === 'usdc' && ${B}.state().source?.symbol === ${JSON.stringify(s)}`, 10000)
        for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects[sk] ?? 0) > c0) break; await sleep(50) }
        check(`${tag}_exact_producer_demand`, JSON.stringify(st.active) === JSON.stringify([[`coinbase/usdc/${s}`, 1]]) && st.connects[sk] === c0 + 1 && sockets.some(u => u.includes('/book') && u.includes('exchange=coinbase') && u.includes('market=usdc') && u.includes(`symbol=${s}`)), JSON.stringify([st.active, st.connects]))
        check(`${tag}_ack_and_snapshot_sent`, (await mailbox({ op: 'drb_ack', sym: sk })).ok && (await mailbox({ op: 'drb_snapshot', sym: sk, mid, tick, bids: nb, asks: na, cid: 1000 })).ok)
        await waitFor(`${B}.state().status.state === 'live' && ${B}.state().seq === 1000`, 15000).catch(() => {})
        // change_id may jump (1000 -> 1007) when prev_change_id matches.
        check(`${tag}_change_sent`, (await mailbox({ op: 'drb_change', sym: sk, prev: 1000, cid: 1007, dt_ms: 3, b: [['change', rnd(mid - tick), 3.5]], a: [['delete', rnd(mid + tick), 0]] })).ok)
        await waitFor(`${B}.state().status.state === 'live' && ${B}.state().seq === 1007 && ${B}.state().plugins[0].rows > 0 && ${B}.state().source?.market === 'usdc'`, 15000)
        await sleep(200)
        let dd = await dom(); const li = (await bookState()).lastInput
        check(`${tag}_wasm_input_window_5000_4999`, li?.bids === 5000 && li?.asks === 4999, JSON.stringify(li))
        check(`${tag}_base_rows_change_and_delete_no_contract_size`, JSON.stringify(dd.rows) === JSON.stringify(expectDrb(mid, tick, pd, 'base', 3.5, true)), `got=${JSON.stringify(dd.rows.slice(18, 22))} want=${JSON.stringify(expectDrb(mid, tick, pd, 'base', 3.5, true).slice(18, 22))}`)
        check(`${tag}_labels_native_usdc_seq`, dd.venue.includes(`Coinbase DRB USDC perpetual ${native}`) && new RegExp(`qty ${s}\\b`).test(dd.title) && /\bseq 1007\b/.test(dd.note) && !/internal rev/.test(dd.note) && /Depth 5000\/4999 levels per side \(deep\)/.test(dd.note), JSON.stringify([dd.venue, dd.title, dd.note]))
        await setParams('orderbook', { rows: 20, group: 0, unit: 'quote' })
        await waitFor(`${B}.state().plugins[0]?.unit === 'USDC' && ${B}.state().plugins[0].rows > 0`, 15000).catch(() => {})
        await sleep(200)
        dd = await dom()
        check(`${tag}_quote_usdc_rows_exact_price_times_base`, JSON.stringify(dd.rows) === JSON.stringify(expectDrb(mid, tick, pd, 'quote', 3.5, true)) && /qty USDC\b/.test(dd.title), `got=${JSON.stringify(dd.rows.slice(18, 22))} title=${dd.title}`)
        await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
        await waitFor(`${B}.state().plugins[0]?.unit === ${JSON.stringify(s)}`, 15000).catch(() => {})
        await mailbox({ op: 'drb_change', sym: sk, prev: 1007, cid: 1008, dt_ms: 5, b: [['change', rnd(mid - tick), 1.25]], a: [] })
        await waitFor(`${B}.state().seq === 1008 && ${B}.state().plugins[0].rows > 0`, 15000).catch(() => {})
        await sleep(300)
        dd = await dom()
        check(`${tag}_absolute_replace_seq1008`, JSON.stringify(dd.rows) === JSON.stringify(expectDrb(mid, tick, pd, 'base', 1.25, true)), JSON.stringify(dd.rows.slice(19, 21)))
        // Deep numeric oracle: bid index 1500 (beyond 1000, inside the 5000
        // window) replaced 9 -> 4321.5; index 2500 deleted; an update at index
        // 5200 (outside the anchored window: an existing level in the BTC
        // 6000 snapshot, a new level for ETH's 5200 snapshot) must not count.
        const deepP = rnd(mid - tick * 1501), delP = rnd(mid - tick * 2501), outP = rnd(mid - tick * 5201)
        await mailbox({ op: 'drb_change', sym: sk, prev: 1008, cid: 1009, dt_ms: 10, b: [['change', deepP, 9]], a: [] })
        await mailbox({ op: 'drb_change', sym: sk, prev: 1009, cid: 1010, dt_ms: 15, b: [['change', deepP, 4321.5], ['delete', delP, 0], [nb > 5200 ? 'change' : 'new', outP, 999999]], a: [] })
        await waitFor(`${B}.state().seq === 1010`, 15000).catch(() => {})
        await setParams('orderbook', { rows: 20, group: G, unit: 'base' })
        await waitFor(`${B}.state().plugins[0]?.fault === null && ${B}.state().plugins[0].rows > 0 && ${B}.state().plugins[0].rows < 20`, 15000).catch(() => {})
        await sleep(300)
        dd = await dom()
        const g = (() => {
          const b = new Map(), a = new Map()
          for (let i = 0; i < 5000; i++) {
            const bp = rnd(mid - tick * (i + 1)), ap = rnd(mid + tick * (i + 1))
            let bq = 0.5 + (i % 7) * 0.125; if (i === 0) bq = 1.25; if (i === 1500) bq = 4321.5; if (i === 2500) bq = 0
            const bk = Math.floor(bp / G) * G; b.set(bk, (b.get(bk) ?? 0) + bq)
            if (i > 0) { const ak = Math.ceil(ap / G) * G; a.set(ak, (a.get(ak) ?? 0) + 0.25 + (i % 7) * 0.125) }
          }
          const k = Math.floor(deepP / G) * G
          return { rows: [...[...a].sort((x, y) => y[0] - x[0]).map(([p, q]) => ['', fmt(p, 0), qfD(q)]), ...[...b].sort((x, y) => y[0] - x[0]).map(([p, q]) => [qfD(q), fmt(p, 0), ''])], deep: b.get(k), k }
        })()
        check(`${tag}_deep_beyond1000_grouped_exact`, JSON.stringify(dd.rows) === JSON.stringify(g.rows) && g.deep > 4321.5 && dd.rows.some(r => r[1] === fmt(g.k, 0) && r[0] === qfD(g.deep)), `got=${JSON.stringify(dd.rows)} want=${JSON.stringify(g.rows)}`)
        check(`${tag}_outside_window_not_counted`, !JSON.stringify(dd.rows).includes('999,999') && !dd.rows.some(r => r[1] === fmt(Math.floor(outP / G) * G, 0) && Math.floor(outP / G) * G < Math.floor(rnd(mid - tick * 5000) / G) * G), JSON.stringify(dd.rows))
        await setParams('orderbook', { rows: 20, group: 0, unit: 'base' })
        await waitFor(`${B}.state().plugins[0]?.unit === ${JSON.stringify(s)} && ${B}.state().plugins[0].rows === 40`, 15000).catch(() => {})
        // Native change_id gap: wrong prev ends the generation (rows withdrawn),
        // the producer reconnects and a fresh WS snapshot restores correct
        // rows in generation+1 without re-enabling the plugin.
        const gen0 = (await bookState()).generation; const cg = (await mailbox({ op: 'state' })).connects[sk]
        check(`${tag}_wrong_prev_sent`, (await mailbox({ op: 'drb_change', sym: sk, prev: 9999, cid: 10011, dt_ms: 20, b: [['change', rnd(mid - tick), 777]], a: [] })).ok)
        await waitFor(`${B}.state().status.state !== 'live'`, 10000).catch(() => {})
        await sleep(100)
        dd = await dom(); const bs = await bookState()
        check(`${tag}_gap_withdraws_rows`, bs.status.state !== 'live' && dd.rows.length === 0 && bs.seq !== 10011, JSON.stringify([bs.status, bs.seq, dd.rows.length]))
        for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects[sk] ?? 0) > cg) break; await sleep(50) }
        check(`${tag}_gap_reconnects_same_producer`, st.connects[sk] === cg + 1 && JSON.stringify(st.active) === JSON.stringify([[`coinbase/usdc/${s}`, 1]]), JSON.stringify([st.active, st.connects]))
        await mailbox({ op: 'drb_snapshot', sym: sk, mid, tick, bids: nb, asks: na, cid: 20000, dt_ms: 30 })
        await waitFor(`${B}.state().status.state === 'live' && ${B}.state().seq === 20000 && ${B}.state().plugins[0].rows > 0`, 15000).catch(() => {})
        await sleep(300)
        dd = await dom(); const bs2 = await bookState()
        check(`${tag}_fresh_snapshot_new_generation_restores_rows`, bs2.generation === gen0 + 1 && bs2.seq === 20000 && JSON.stringify(dd.rows) === JSON.stringify(expectDrb(mid, tick, pd, 'base', null, false)) && /\bseq 20000\b/.test(dd.note), JSON.stringify([gen0, bs2.generation, bs2.seq, dd.rows.slice(19, 21)]))
        await closeMenu()
      }
      // Positive isolation: Coinbase Exchange spot BTC vs Coinbase DRB BTC —
      // same exchange+symbol, different market/native/quote/seq kind.
      const XS = [['spot', 'coinbase:BTC-USD'], ['usdc', 'coinbase:BTC_USDC-PERPETUAL']]
      const c0 = (await mailbox({ op: 'state' })).connects
      const subs = XS.map(([mk]) => { const s = new WebSocket(`ws://127.0.0.1:${info.port}/book?exchange=coinbase&market=${mk}&symbol=BTC`); const frames = []; s.onmessage = e => frames.push(JSON.parse(e.data)); return { mk, s, frames } })
      for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if (XS.every(([, sk]) => (st.connects[sk] ?? 0) > (c0[sk] ?? 0))) break; await sleep(50) }
      const act = Object.fromEntries(st.active)
      check('cbx_spot_vs_drb_two_producers', (act['coinbase/spot/BTC'] ?? 0) >= 1 && (act['coinbase/usdc/BTC'] ?? 0) >= 1 && XS.every(([, sk]) => st.connects[sk] === (c0[sk] ?? 0) + 1), JSON.stringify([st.active, st.connects]))
      await sleep(150)
      await mailbox({ op: 'cb_snapshot', sym: 'coinbase:BTC-USD', mid: 70000, tick: 0.01, bids: 300, asks: 300 })
      await mailbox({ op: 'cb_update', sym: 'coinbase:BTC-USD', changes: [['buy', '69999.99', '2.00000000']] })
      await mailbox({ op: 'drb_snapshot', sym: 'coinbase:BTC_USDC-PERPETUAL', mid: 78000, tick: 0.5, bids: 300, asks: 300, cid: 500 })
      const live = x => x.frames.filter(f => f.status === 'live' && f.bids.length)
      await bounded((async () => { while (!subs.every(x => live(x).length)) await sleep(50) })(), 15000, 'cbx live').catch(() => {})
      const snap = x => { const f = live(x).at(-1); return f && { mk: f.source.market, native: f.native_symbol, quote: f.quote, kind: f.seq_kind, seq: f.seq, t: f.exchange_time_ms, bid: +f.bids[0][0], q0: +f.bids[0][1], raw: f.raw_quantity, cs: f.contract_size_quote } }
      const s0 = subs.map(snap)
      check('cbx_distinct_identity_values', s0[0]?.mk === 'spot' && s0[0]?.native === 'BTC-USD' && s0[0]?.quote === 'USD' && s0[0]?.kind === 'internal_revision' && s0[0]?.seq === 1 && s0[0]?.bid === 69999.99
        && s0[1]?.mk === 'usdc' && s0[1]?.native === 'BTC_USDC-PERPETUAL' && s0[1]?.quote === 'USDC' && s0[1]?.kind === 'exchange_update_id' && s0[1]?.seq === 500 && s0[1]?.bid === 77999.5 && s0[1]?.q0 === 0.5
        && s0[1]?.raw === 'base' && s0[1]?.cs === null && s0[1]?.t > 1.7e12 && subs.every(x => x.frames.every(f => f.source.exchange === 'coinbase' && f.source.market === x.mk)), JSON.stringify(s0))
      await mailbox({ op: 'drb_change', sym: 'coinbase:BTC_USDC-PERPETUAL', prev: 500, cid: 501, dt_ms: 9, b: [['change', 77999.5, 777]], a: [] })
      await bounded((async () => { while (!live(subs[1]).some(f => f.seq === 501)) await sleep(50) })(), 10000, 'drb 501').catch(() => {})
      await mailbox({ op: 'cb_update', sym: 'coinbase:BTC-USD', dt_ms: 9, changes: [['buy', '69999.98', '555.55500000']] })
      await bounded((async () => { while (!live(subs[0]).some(f => f.seq === 2)) await sleep(50) })(), 10000, 'cb rev2').catch(() => {})
      await sleep(400)
      const has = (x, q) => live(x).some(f => f.bids.some(([, v]) => Math.abs(+v - q) < 1e-9))
      check('cbx_updates_isolated_no_cross_admission', has(subs[1], 777) && !has(subs[0], 777) && has(subs[0], 555.555) && !has(subs[1], 555.555)
        && live(subs[1]).at(-1)?.seq === 501 && live(subs[0]).at(-1)?.seq === 2, JSON.stringify(subs.map(snap)))
      for (const x of subs) x.s.close()
      await sleep(600)
    }
    // J. unsupported source in UI: no /book socket, explicit label, demand released.
    const before = sockets.filter(u => u.includes('/book')).length
    await evaluate(`(() => { const e = document.getElementById('exchange'); e.value = 'coinbase'; e.dispatchEvent(new Event('change')); return e.value })()`)
    // M3B: all 14 registered sources are supported and the UI offers only
    // registered BTC/ETH sources, so no real unsupported user selection
    // remains. COMPONENT INJECTION (not a user flow): drive the actual
    // BookPanel.setSource boundary with an unregistered identity.
    check('unsupported_no_ui_option_remains', await evaluate(`(() => { const m = document.getElementById('market'), s = document.getElementById('symbol'); return [...s.options].every(o => ['BTC', 'ETH'].includes(o.value)) && [...m.options].every(o => ['spot', 'usdc'].includes(o.value)) })()`), 'coinbase market/symbol options')
    await evaluate(`${B}.setSource({ exchange: 'coinbase', market: 'usdc', symbol: 'SOL' })`)
    await waitFor(`${B}.state().status.state === 'unsupported'`, 10000)
    d = await dom()
    check('unsupported_source_label_no_rows', /unavailable for this source/.test(d.status) && d.rows.length === 0, JSON.stringify(d))
    await sleep(500)
    check('unsupported_no_book_socket', sockets.filter(u => u.includes('/book')).length === before && !sockets.some(u => u.includes('/book') && u.includes('symbol=SOL')))
    st = await mailbox({ op: 'state' })
    check('unsupported_releases_demand', st.active.length === 0, JSON.stringify(st.active))
    // K. disable releases demand and hides panel; bars path unaffected.
    await evaluate(`(() => { const e = document.getElementById('exchange'); e.value = 'binance'; e.dispatchEvent(new Event('change')); return e.value })()`)
    await selectSource('usdm', 'BTC')
    await waitFor(`${B}.state().source?.symbol === 'BTC' && ${B}.state().connected`, 10000)
    await waitFor(valuesReady, 30000); await sleep(500)
    await panChart(200)
    const k0 = await basis()
    await toggle('orderbook', false)
    await sleep(500)
    await closeMenu(); await sleep(300)
    const k1 = await basis()
    check('disable_keeps_values_and_viewport', basisOk(k0, 'BTC|1m') && sameValues(k0, k1) && near(k0.range[1], k1.range[1]), JSON.stringify([k0.range, k1.range]))
    st = await mailbox({ op: 'state' })
    check('disable_releases_demand_and_hides', st.active.length === 0 && (await dom()).hidden === true, JSON.stringify(st.active))
    check('no_db_or_outbox_writes_from_book', st.bars_rows === st.bars_rows_at_start && st.outbox_pending === st.outbox_pending_at_start, JSON.stringify(st))
    // R. Reload lifecycle through IndexedDB (no forced re-enable).
    const reopen = async () => {
      await call('Page.reload', { ignoreCache: true })
      await waitFor(`!!window.__cvdLocal && !!window.__cvdLocal.host() && !!window.__cvdLocal.book() && !!window.__cvdSrv`, 30000)
      await selectSource('usdm', 'BTC')
      await waitFor(`document.getElementById('market').value === 'usdm' && ${L}.records().has('orderbook')`, 30000)
    }
    const goLive = async (id) => {
      const c = (await mailbox({ op: 'state' })).connects.BTCUSDT ?? 0
      let s; for (let i = 0; i < 300; i++) { s = await mailbox({ op: 'state' }); if ((s.connects.BTCUSDT ?? 0) > c) break; await sleep(50) }
      await sleep(100)
      await mailbox({ op: 'event', sym: 'BTCUSDT', U: id - 1, u: id + 1, pu: id - 10, b: [], a: [] })
    }
    await mailbox({ op: 'snapshot', sym: 'BTCUSDT', id: 3000, mid: 30020, tick: 0.1, levels: 200 })
    // R1. enabled + non-default parameter, then reload.
    await toggle('orderbook', true)
    await setParams('orderbook', { rows: 7, group: 0, unit: 'base' }); await closeMenu()
    await goLive(3000)
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0]?.rows === 14`, 15000)
    const clicksBefore = toggleClicks
    await mailbox({ op: 'snapshot', sym: 'BTCUSDT', id: 4000, mid: 30030, tick: 0.1, levels: 200 })
    const sockR1 = sockets.length
    await reopen()
    const rec1 = await evaluate(`(() => { const r = ${L}.records().get('orderbook'); return { enabled: r?.enabled, params: r?.params } })()`)
    await goLive(4000)
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0]?.rows === 14 && ${B}.state().plugins[0].state === 'ready'`, 20000)
    d = await dom()
    check('reload_enabled_restores_params_and_computes_without_reenable', toggleClicks === clicksBefore && rec1.enabled === true && rec1.params?.rows === 7 && d.rows.length === 14 && /30,030\.1|30,029\.9/.test(JSON.stringify(d.rows)) && sockets.slice(sockR1).some(u => u.includes('/book')), JSON.stringify({ rec1, n: d.rows.length }))
    // R2. disabled, then reload: stays disabled, no book socket/Worker/demand.
    await toggle('orderbook', false); await closeMenu(); await sleep(500)
    const sockR2 = sockets.length
    await reopen(); await sleep(1500)
    st = await mailbox({ op: 'state' })
    const rec2 = await evaluate(`${L}.records().get('orderbook')?.enabled`)
    const bs2 = await bookState()
    check('reload_disabled_stays_disabled_no_demand', rec2 === false && bs2.plugins.length === 0 && (await dom()).hidden === true && !sockets.slice(sockR2).some(u => u.includes('/book')) && st.active.length === 0, JSON.stringify({ rec2, plugins: bs2.plugins, active: st.active }))
    // R3. remove + re-import through the real menu path.
    if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`)
    await evaluate(`document.querySelector('.plugin-menu-row[data-plugin="orderbook"] [data-action=remove]').click()`)
    await waitFor(`!${L}.records().has('orderbook') && !document.querySelector('.plugin-menu-row[data-plugin="orderbook"]')`, 10000)
    check('remove_via_menu', !(await evaluate(`${L}.records().has('orderbook')`)))
    check('reimport_via_menu', await importFile(BOOK) && await evaluate(`${L}.records().get('orderbook')?.descriptor?.inputs?.[0] === 'order_book' && !!document.querySelector('.plugin-menu-row[data-plugin="orderbook"]')`))
    await closeMenu()
  } else {
    // Real public Binance: bounded run.
    await toggle('orderbook', true)
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows >= 40`, 45000)
    let d = await dom()
    const top = parseFloat(d.rows[19][1].replace(/,/g, '')) /* best ask */, bid = parseFloat(d.rows[20][1].replace(/,/g, ''))
    check('real_btc_live_rows', d.rows.length === 40 && top > bid && bid > 1000 && /BTCUSDT/.test(d.venue), JSON.stringify(d.rows.slice(18, 22)))
    const s1 = await bookState(); await sleep(1500); const s2 = await bookState()
    check('real_btc_updates_flow', s2.seq > s1.seq && s2.status.state === 'live', `${s1.seq} -> ${s2.seq}`)
    await closeMenu(); await sleep(200)
    const rv = await bothSides()
    check('real_desktop_both_sides_visible', bothOk(rv) && rv.side, JSON.stringify(rv))
    await shot('book-real-btc.png')
    await selectSource('usdm', 'ETH')
    await waitFor(`${B}.state().source?.symbol === 'ETH' && ${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows >= 40`, 45000)
    d = await dom()
    const ethBid = parseFloat(d.rows[20][1].replace(/,/g, ''))
    check('real_eth_live_rows_distinct', /ETHUSDT/.test(d.venue) && ethBid > 10 && ethBid < bid, `${ethBid} vs ${bid}`)
    await shot('book-real-eth.png')
    // Bounded real spot BTC read (5000/side REST snapshot + spot diff stream).
    await selectSource('spot', 'BTC')
    await waitFor(`${B}.state().source?.market === 'spot' && ${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows >= 40`, 45000)
    d = await dom()
    const sli = (await bookState()).lastInput
    console.log(`NOTE real spot input=${JSON.stringify(sli)}`)
    check('real_spot_btc_live_deep_input', /Binance spot BTCUSDT/.test(d.venue) && sli?.bids === 5000 && sli?.asks === 5000, JSON.stringify([d.venue, sli]))
    await shot('book-real-spot-btc.png')
    await toggle('orderbook', false)
    await sleep(500)
    const st = await mailbox({ op: 'state' })
    check('real_disable_releases_demand', st.active.length === 0, JSON.stringify(st.active))
    check('real_no_db_or_outbox_writes', st.bars_rows === st.bars_rows_at_start && st.outbox_pending === st.outbox_pending_at_start, JSON.stringify(st))
  }
  const barsStatus1 = await evaluate(`document.getElementById('status').textContent`)
  console.log(`NOTE bars status before=${JSON.stringify(barsStatus0)} after=${JSON.stringify(barsStatus1)}`)
  if (LEGACY) check('legacy_plugins_still_running', (await evaluate(`${L}.compute().running()`)).includes('cvd'))
  check('no_page_exceptions', exceptions.length === 0, exceptions.join('\n'))
  const allowed = e => e.method === 'GET' && ((e.url.startsWith('https://fonts.googleapis.com/css2?') && e.type === 'Stylesheet') || (e.url.startsWith('https://fonts.gstatic.com/') && e.type === 'Font'))
  check('no_browser_external_requests_outside_font_allowlist', external.filter(e => !allowed(e)).length === 0, JSON.stringify(external.filter(e => !allowed(e))))
  exit = checks.every(c => c.ok) ? 0 : 1
} catch (e) { console.log('FAIL harness', e.stack); exit = 1 } finally {
  try { if (info) await bounded(mailbox({ op: 'stop' }), 15000, 'stop') } catch (e) { console.log('FAIL stop mailbox', e.message); exit = 1 }
  try { ws?.close() } catch {}
  if (chrome) {
    const td = await teardown({ child: chrome, profileDir: profile })
    console.log(`TEARDOWN ${JSON.stringify(td)}`)
    if (!td.ok) exit = 1
  }
  console.log(`${checks.filter(c => c.ok).length}/${checks.length} checks passed`)
  process.exit(exit)
}
