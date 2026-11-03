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
//   CVD_LEGACY_WASM_DIR dir with released v0.1.0 cvd_indicator_cvd.wasm + cvd_indicator_flow.wasm
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
  for (const q of ['exchange=binance&market=spot&symbol=BTC', 'exchange=coinbase&market=usdc&symbol=BTC', 'exchange=binance&market=usdm&symbol=SOL']) {
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

  // Legacy released CVD/Flow v0.1.0 plugins (no rebuild) import and compute.
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
    check('server_demand_btc_one_subscriber', JSON.stringify(st.active) === JSON.stringify([['BTC', 1]]), JSON.stringify(st))
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
    check('tf_change_keeps_book_subscription', tfAfter.sockets === tfBefore.sockets && tfAfter.st.connects.BTCUSDT === tfBefore.st.connects.BTCUSDT && JSON.stringify(tfAfter.st.active) === JSON.stringify([['BTC', 1]]) && tfAfter.bs.generation === tfBefore.gen && tfAfter.bs.connected && tfAfter.bs.status.state === 'live' && tfAfter.bs.plugins[0].rows > 0 && tfAfter.bs.seq > 1005,
      JSON.stringify({ b: [tfBefore.sockets, tfBefore.st.connects, tfBefore.gen], a: [tfAfter.sockets, tfAfter.st.connects, tfAfter.st.active, tfAfter.bs.generation, tfAfter.bs.status, tfAfter.bs.seq] }))
    const flowKeys = b => Object.keys(b.values).filter(k => /^(delta|flow)\//.test(k))
    check('tf_change_cvd_and_flow_recomputed_valid', tfAfter.cvd.tf === '5m' && basisOk(tfAfter.cvd, 'BTC|5m') && flowKeys(tfAfter.cvd).length > 0 && JSON.stringify(tfAfter.cvd.values) !== JSON.stringify(tfBefore.cvd.values)
      && Object.values(tfAfter.cvd.values).every(([, last]) => last.every(([t]) => (Number(t) + 1) % 300 === 0)), JSON.stringify(tfAfter.cvd).slice(0, 500))
    await evaluate(`(() => { const t = document.getElementById('tf'); t.value = '1m'; t.dispatchEvent(new Event('change')); return t.value })()`)
    await waitFor(`(() => { for (const [k, m] of ${L}.values()) if (k.startsWith('cvd/') && m.size >= 300 && [...m.keys()].some(t => (Number(t) + 1) % 300 !== 0)) return true; return false })()`, 30000)
    const tfBack = { st: await mailbox({ op: 'state' }), sockets: sockets.filter(u => u.includes('/book')).length, bs: await bookState() }
    check('tf_back_keeps_book_subscription', tfBack.sockets === tfBefore.sockets && tfBack.st.connects.BTCUSDT === tfBefore.st.connects.BTCUSDT && tfBack.bs.generation === tfBefore.gen && tfBack.bs.status.state === 'live', JSON.stringify([tfBack.sockets, tfBack.st.connects, tfBack.bs.generation]))
    btcPump = false; await pumpBtc
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
    check('shared_subscribers_one_task', JSON.stringify(st.active) === JSON.stringify([['BTC', 2]]), JSON.stringify(st.active))
    extra.close(); await sleep(500)
    st = await mailbox({ op: 'state' })
    check('extra_client_release_keeps_page_demand', JSON.stringify(st.active) === JSON.stringify([['BTC', 1]]), JSON.stringify(st.active))
    // G. source switch to ETH: BTC cleared + released, ETH distinct data.
    await mailbox({ op: 'snapshot', sym: 'ETHUSDT', id: 500, mid: 2000, tick: 0.01, levels: 700 })
    await selectSource('usdm', 'ETH')
    await waitFor(`${B}.state().source?.symbol === 'ETH'`, 10000)
    d = await dom()
    check('source_switch_clears_old_rows', d.rows.length === 0, JSON.stringify(d.rows.slice(0, 2)))
    for (let i = 0; i < 200; i++) { st = await mailbox({ op: 'state' }); if ((st.connects.ETHUSDT ?? 0) > 0 && JSON.stringify(st.active) === JSON.stringify([['ETH', 1]])) break; await sleep(50) }
    check('btc_demand_released_eth_acquired', JSON.stringify(st.active) === JSON.stringify([['ETH', 1]]), JSON.stringify(st.active))
    await sleep(100)
    await mailbox({ op: 'event', sym: 'ETHUSDT', U: 499, u: 501, pu: 400, b: [], a: [] })
    await waitFor(`${B}.state().status.state === 'live' && ${B}.state().plugins[0].rows > 0`, 15000)
    d = await dom()
    want = expectedRows(2000, 0.01, 700, []).map(r => [q3(r[0]), r[1], q3(r[2])])
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
    // C. 700-level snapshot, 500 transmitted: supplied coverage is the 500th level, not the snapshot range.
    check('coverage_truthful_for_transmitted_500', /Supplied range 1,995\.00–2,005\.00;/.test(d.note), d.note)
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
      for (let i = 0; i < 500; i++) {
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
    // P. Pin survives render -> queued programmatic-scroll event -> resize.
    // LABELLED LAYOUT FAULT INJECTION: a real live tick renders real WASM rows
    // and pin() restores the spread edge; in the microtask right after that
    // render (before the frame that delivers its queued scroll event) the asks
    // box is shrunk the way the mobile viewport shrinks it. Rows are never
    // filled directly; only the box height is changed.
    {
      const p0 = await settled('asks')
      await evaluate(`new Promise(res => { const mo = new MutationObserver(() => { mo.disconnect(); const s = document.createElement('style'); s.id = 'resize-sim'; s.textContent = '#book-panel .book-asks { flex: none !important; height: 60px !important }'; document.head.appendChild(s); res(true) }); mo.observe(document.getElementById('book-panel'), { childList: true }) })`)
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
      const q = await evaluate(`new Promise(res => { const mo = new MutationObserver(() => { mo.disconnect(); const e = ${sec('asks')}; const r = e.scrollTop; e.scrollTop = r - 60; const a = e.scrollTop; e.dispatchEvent(new Event('scroll')); e.scrollTop = r; e.dispatchEvent(new Event('scroll')); res({ r, a, back: e.scrollTop }) }); mo.observe(document.getElementById('book-panel'), { childList: true }) })`)
      { const s0 = await bookState(); await waitFor(`${B}.state().counters.renders >= ${s0.counters.renders + 3}`, 15000) }
      const q1 = await geo('asks')
      check('return_to_restored_offset_kept_after_away_scroll', q.r > 100 && q.r - q.a === 60 && q.back === q.r && Math.abs(q1.top - q.r) <= 1, JSON.stringify([q, q1.top]))
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
    // J. unsupported source in UI: no /book socket, explicit label, demand released.
    const before = sockets.filter(u => u.includes('/book')).length
    await selectSource('spot', 'BTC')
    await waitFor(`${B}.state().status.state === 'unsupported'`, 10000)
    d = await dom()
    check('unsupported_source_label_no_rows', /unavailable for this source/.test(d.status) && d.rows.length === 0, JSON.stringify(d))
    await sleep(500)
    check('unsupported_no_book_socket', sockets.filter(u => u.includes('/book')).length === before && !sockets.some(u => u.includes('/book') && u.includes('market=spot')))
    st = await mailbox({ op: 'state' })
    check('unsupported_releases_demand', st.active.length === 0, JSON.stringify(st.active))
    // K. disable releases demand and hides panel; bars path unaffected.
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
