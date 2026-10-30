#!/usr/bin/env node
// Served-application chain: REAL cvd-server app_router (HTTP + WS + static)
// backed by a disposable PostgreSQL, started by the opt-in Rust test
// `postgres_served_app_chain` (crates/cvd-server/src/usdc_tests.rs). The
// served static tree is the ASSEMBLED tree from scripts/assemble-static.sh
// (wasm-host/assets.json required). Bars are SYNTHETIC, seeded through the
// real outbox -> PG -> hydrate path; corrections/live ticks go through the
// real push_publish / broadcast_live_payload via a test-only file mailbox
// whose ack carries the exact emitted bar.
//
// Expected values come from a CROSS-RUNTIME EXECUTION REFERENCE: the same
// public plugin .wasm run by the shipped plugin-worker.js +
// cvd_browser_host.wasm in Node over the bars fetched from this server's
// /history plus the producer-emitted live bar. This is NOT an independent
// algorithm implementation (C's pure numerical conformance and the archived
// native comparison are the independent algorithm evidence).
//
// Test instrumentation: a read-only accessor (window.__cvdSrv) is appended to
// the SERVED COPY of js/app.js in the disposable assembled tree only (same
// technique as tests/local-wasm-browser.mjs); product source is unchanged.
//
//   CVD_SERVED_CHAIN_DIR   mailbox dir (port file written by the Rust test)
//   CVD_SERVED_STATIC_DIR  assembled static tree served by the Rust test
//   CVD_PUBLIC_WASM_DIR    dir with cvd_indicator_cvd.wasm, hostile_wasm_plugin.wasm
//   CVD_SERVED_INJECT      test-only: 'harness' throws mid-run (cleanup control)
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import crypto from 'node:crypto'
import { spawn } from 'node:child_process'
import { WasmOracleClient, inputBar } from './wasm-oracle-client.mjs'
import { teardown } from './harness-teardown.mjs'

const DIR = process.env.CVD_SERVED_CHAIN_DIR
const STATIC = process.env.CVD_SERVED_STATIC_DIR
const W = process.env.CVD_PUBLIC_WASM_DIR
const CHROMIUM = process.env.CVD_CHROMIUM || '/usr/bin/chromium'
const sleep = ms => new Promise(r => setTimeout(r, ms))
const checks = []
const check = (name, ok, detail = '') => { checks.push({ name, ok: !!ok }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : ' :: ' + String(detail).slice(0, 400)}`) }
const bounded = (p, ms, what) => Promise.race([p, sleep(ms).then(() => { throw new Error(`timeout ${ms}ms: ${what}`) })])
const fetchT = (url, ms = 15000) => fetch(url, { signal: AbortSignal.timeout(ms) })
const sha = b => crypto.createHash('sha256').update(b).digest('hex')
const TRUSTED = ['cvd_browser_host.wasm', 'cvd_browser_coordinator.wasm']

// Pure manifest verifier (also exercised by negative controls below).
function verifyManifest(m, files) {
  if (m?.format !== 'cvd-trusted-runtime-assets-v1' || !Array.isArray(m.assets)) return 'bad format'
  const names = m.assets.map(a => a.file).sort()
  if (JSON.stringify(names) !== JSON.stringify([...TRUSTED].sort())) return `asset set ${names}`
  for (const a of m.assets) {
    const b = files[a.file]; if (!b) return `missing bytes ${a.file}`
    if (a.sha256 !== sha(b)) return `sha mismatch ${a.file}`
    if (a.bytes !== b.length) return `size mismatch ${a.file}`
  }
  return null
}

let exit = 1, chrome, profile, oracle, ws, info, seq = 0
let id = 0; const pending = new Map(); const exceptions = []; const external = []
async function mailbox(cmd) {
  const n = seq++
  fs.writeFileSync(path.join(DIR, `cmd.${n}.tmp`), JSON.stringify(cmd)); fs.renameSync(path.join(DIR, `cmd.${n}.tmp`), path.join(DIR, `cmd.${n}`))
  for (let i = 0; i < 500; i++) { const f = path.join(DIR, `ack.${n}`); if (fs.existsSync(f)) { await sleep(20); return fs.readFileSync(f, 'utf8') } await sleep(20) }
  throw new Error('mailbox timeout')
}
const call = (method, params = {}) => bounded(new Promise((res, rej) => { const i = ++id; pending.set(i, m => m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result)); ws.send(JSON.stringify({ id: i, method, params })) }), 20000, method)
const evaluate = async expr => { const r = await call('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 300)); return r.result.value }
async function waitFor(expr, ms = 30000) { const end = Date.now() + ms; while (Date.now() < end) { try { if (await evaluate(expr)) return } catch {} await sleep(50) } throw new Error('timeout: ' + expr) }

try {
  if (!DIR || !STATIC || !W) throw new Error('CVD_SERVED_CHAIN_DIR, CVD_SERVED_STATIC_DIR, CVD_PUBLIC_WASM_DIR required')
  for (let i = 0; i < 600 && !info; i++) { try { info = JSON.parse(fs.readFileSync(path.join(DIR, 'port'), 'utf8')) } catch { await sleep(200) } }
  if (!info) throw new Error('server did not start')
  const BASE = `http://127.0.0.1:${info.port}`

  // Instrument the served copy only (read-only accessors).
  const appPath = path.join(STATIC, 'js/app.js')
  const BRIDGE = `\nwindow.__cvdSrv = { series: k => { const r = renderers.get(k); const s = r && r.seriesHandle && r.seriesHandle(); return s ? s.data().filter(p => p.value !== undefined).map(p => [p.time, p.value]) : null }, readouts: () => paneSummaryEls.map(e => e.textContent), fmt: (k, v) => { const o = registry.panes.flatMap(p => p.outputs).find(o => o.key === k); return o ? readoutText(o, v) : null } }\n`
  const app = fs.readFileSync(appPath, 'utf8'); if (!/\ninit\(\)\s*$/.test(app)) throw new Error('app.js shape changed')
  fs.writeFileSync(appPath, app.replace(/\ninit\(\)\s*$/, BRIDGE + '\ninit()\n'))

  // 0. Assets: served == assembled == source-built static copy == manifest.
  const files = {}
  for (const f of TRUSTED) {
    const served = Buffer.from(await (await fetchT(`${BASE}/wasm-host/${f}`)).arrayBuffer())
    files[f] = served
    check(`served_${f}_equals_assembled`, sha(served) === sha(fs.readFileSync(path.join(STATIC, 'wasm-host', f))))
    check(`served_${f}_equals_built_static`, sha(served) === sha(fs.readFileSync(new URL(`../static/wasm-host/${f}`, import.meta.url))))
  }
  const amf = await fetchT(`${BASE}/wasm-host/assets.json`)
  check('served_assets_manifest_present', amf.ok, amf.status)
  const manifest = amf.ok ? await amf.json() : null
  check('served_assets_manifest_exact', verifyManifest(manifest, files) === null, verifyManifest(manifest, files))
  if (manifest) {
    const clone = () => JSON.parse(JSON.stringify(manifest))
    const neg = {
      missing_entry: (() => { const m = clone(); m.assets = m.assets.filter(a => a.file !== TRUSTED[1]); return verifyManifest(m, files) })(),
      sha_mismatch: (() => { const m = clone(); m.assets[0].sha256 = '0'.repeat(64); return verifyManifest(m, files) })(),
      size_mismatch: (() => { const m = clone(); m.assets[1].bytes += 1; return verifyManifest(m, files) })(),
      extra_entry: (() => { const m = clone(); m.assets.push({ file: 'plugin.wasm', sha256: 'x', bytes: 1 }); return verifyManifest(m, files) })(),
      missing_bytes: verifyManifest(manifest, { [TRUSTED[0]]: files[TRUSTED[0]] }),
      tampered_bytes: verifyManifest(manifest, { ...files, [TRUSTED[0]]: Buffer.concat([files[TRUSTED[0]], Buffer.from([0])]) }),
    }
    for (const [k, v] of Object.entries(neg)) check(`assets_negative_${k}_rejected`, v !== null, 'accepted')
  }
  const miss = await fetchT(`${BASE}/wasm-host/cvd_indicator_cvd.wasm`)
  check('no_plugin_wasm_served', miss.status === 404, miss.status)

  oracle = await WasmOracleClient.spawn(path.join(W, 'cvd_indicator_cvd.wasm'))
  const ctx = oracle.descriptor.context?.bars ?? 0
  async function allBars(tf) {
    let bars = [], before = null
    for (let i = 0; i < 20; i++) {
      const q = new URLSearchParams({ exchange: 'binance', market: 'spot', symbol: 'BTC', tf, limit: '5000', ...(before ? { before: String(before) } : {}) })
      const h = await (await fetchT(`${BASE}/history?${q}`)).json()
      bars = [...h.bars, ...bars]
      if (!h.has_more_before || !h.bars.length) break
      before = h.bars[0].time
    }
    return bars
  }
  let liveBar = null // exact producer-emitted forming bar (ack payload)
  async function expected(tf, mode, firstTime) {
    let bars = (await allBars(tf)).filter(b => Number.isFinite(b.price_close))
    if (liveBar && tf === '1m') { bars = bars.filter(b => b.time !== liveBar.time); bars.push(liveBar); bars.sort((a, b) => a.time - b.time) }
    const idx = bars.findIndex(b => b.time >= firstTime); const start = Math.max(0, idx - ctx)
    const win = bars.slice(start)
    const r = await oracle.history({ exchange: 'binance', market: 'spot', symbol: 'BTC', tf, mode }, idx - start, win.map(inputBar))
    if (!r.ok) throw new Error('oracle ' + JSON.stringify(r.error))
    const o = r.output; const out = new Map(); const key = Object.keys(o.series)[0]
    o.times.forEach((t, i) => out.set(t, o.series[key].values[i]))
    return out
  }

  profile = fs.mkdtempSync(path.join(os.tmpdir(), 'served-chain-'))
  chrome = spawn(CHROMIUM, ['--headless=new', '--no-sandbox', '--disable-gpu', `--user-data-dir=${profile}`, '--remote-debugging-port=0', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] })
  const wsUrl = await new Promise((resolve, reject) => { let b = ''; const t = setTimeout(() => reject(new Error('chromium start timeout')), 20000); chrome.stderr.on('data', d => { b += d; const m = b.match(/DevTools listening on (ws:\/\/\S+)/); if (m) { clearTimeout(t); resolve(m[1]) } }) })
  const targets = await (await fetchT(`http://${new URL(wsUrl).host}/json/list`)).json()
  ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl)
  await bounded(new Promise(r => ws.addEventListener('open', r, { once: true })), 10000, 'cdp open')
  ws.addEventListener('message', ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) }
    if (m.method === 'Runtime.exceptionThrown') exceptions.push(JSON.stringify(m.params.exceptionDetails).slice(0, 300))
    if (m.method === 'Network.requestWillBeSent') { const u = new URL(m.params.request.url); if (/^(https?|wss?):$/.test(u.protocol) && u.host !== `127.0.0.1:${info.port}`) external.push({ url: u.href, method: m.params.request.method, type: m.params.type }) } })
  await call('Runtime.enable'); await call('Network.enable'); await call('DOM.enable'); await call('Page.enable')
  await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
  const open = async () => { await call('Page.navigate', { url: `${BASE}/?exchange=binance&market=spot&symbol=BTC&tf=1m` }); await waitFor(`!!window.__cvdLocal && !!window.__cvdLocal.host() && !!window.__cvdSrv`, 30000) }
  await open()
  const L = 'window.__cvdLocal'
  const values = (k, mode) => evaluate(`Object.fromEntries([...(${L}.values().get(${JSON.stringify(k + '|' + mode)}) ?? new Map())])`)
  const idle = `${L}.compute() && ${L}.compute().idle()`
  async function importFile(file) {
    if (await evaluate(`document.getElementById('plugin-menu').hidden`)) await evaluate(`document.querySelector('.plugin-menu-trigger').click()`)
    const { root } = await call('DOM.getDocument', { depth: -1 }); const { nodeId } = await call('DOM.querySelector', { nodeId: root.nodeId, selector: '.plugin-import-input' })
    const before = await evaluate(`${L}.records().size`)
    await call('DOM.setFileInputFiles', { nodeId, files: [file] })
    await waitFor(`${L}.records().size > ${before} || /already|rejected|error|not/i.test(document.querySelector('.plugin-menu-notice')?.textContent ?? '')`, 20000)
    return evaluate(`${L}.records().size > ${before}`)
  }
  const enable = async pid => { if (!(await evaluate(`${L}.records().get(${JSON.stringify(pid)})?.enabled`))) await evaluate(`document.querySelector('.plugin-menu-row[data-plugin=${JSON.stringify(pid)}] input[type=checkbox]').click()`) }
  if (process.env.CVD_SERVED_INJECT === 'harness') throw new Error('injected harness failure (cleanup control)')

  // Exact coverage + bit equality + rendered LWC series + DOM readout.
  async function compare(tag, mode = 'source', tf = '1m', { expectTime } = {}) {
    await waitFor(`[...${L}.values().keys()].some(k => k.startsWith('cvd/') && k.endsWith('|${mode}')) && ${idle}`, 60000); await sleep(300); await waitFor(idle, 30000)
    const keys = await evaluate(`[...${L}.values().keys()].filter(k => k.startsWith('cvd/') && k.endsWith('|${mode}'))`)
    const key = keys[0].split('|')[0]
    const got = await values(key, mode)
    const times = Object.keys(got).map(Number).sort((a, b) => a - b)
    const out = await expected(tf, mode, times[0])
    let bad = 0, browserOnly = 0
    for (const t of times) { if (!out.has(t)) { browserOnly++; continue } const e = out.get(t); if (!Object.is(got[t], e) && !(got[t] == null && e == null)) bad++ }
    const inRange = [...out.keys()].filter(t => t >= times[0])
    const expectedOnly = inRange.filter(t => !(t in got)).length
    console.log(`WITNESS ${tag} browser=${times.length} expected_in_range=${inRange.length} mismatched=${bad} browser_only=${browserOnly} expected_only=${expectedOnly} last=${times.at(-1)}`)
    check(`${tag}_values_exact_full_coverage`, times.length > 100 && bad === 0 && browserOnly === 0 && expectedOnly === 0, `n=${times.length} bad=${bad} browserOnly=${browserOnly} expectedOnly=${expectedOnly}`)
    if (expectTime !== undefined) check(`${tag}_has_exact_time_${expectTime}`, times.at(-1) === expectTime && Object.is(got[expectTime], out.get(expectTime)), `${times.at(-1)} ${got[expectTime]} vs ${out.get(expectTime)}`)
    // Rendered: LWC series for the current mode equals the browser values.
    await sleep(200)
    const series = await evaluate(`__cvdSrv.series(${JSON.stringify(key)})`)
    const sm = new Map(series ?? [])
    const finite = times.filter(t => got[t] != null && Number.isFinite(got[t]))
    const sBad = finite.filter(t => !Object.is(sm.get(t), got[t])).length
    check(`${tag}_lwc_series_equals_values`, series && series.length === finite.length && sBad === 0, `series=${series?.length} finite=${finite.length} bad=${sBad}`)
    const lastT = finite.at(-1), want = await evaluate(`__cvdSrv.fmt(${JSON.stringify(key)}, ${JSON.stringify(got[lastT])})`)
    const ro = (await evaluate(`__cvdSrv.readouts()`)).join(' | ')
    check(`${tag}_dom_readout_shows_latest`, !!want && ro.includes(want), `want=${want} got=${ro}`)
    return { got, times, out, key }
  }
  // 1. cold start: import + enable public CVD via the real UI path
  check('import_public_cvd', await importFile(path.join(W, 'cvd_indicator_cvd.wasm')))
  await enable('cvd')
  let r = await compare('cold_start')
  // 2. reload: IndexedDB persisted, recomputed from served data
  await open(); r = await compare('reload')
  // 3. mode change (real checkbox) and back
  await evaluate(`document.getElementById('toggle-agg').click()`); await sleep(300)
  await compare('mode_aggregate', 'aggregate')
  await evaluate(`document.getElementById('toggle-agg').click()`); await sleep(300)
  // 4. timeframe change via the real <select> (5m seeded independently), and back
  const setTf = tf => evaluate(`(() => { const s = document.getElementById('tf'); s.value = ${JSON.stringify(tf)}; s.dispatchEvent(new Event('change', { bubbles: true })); return s.value })()`)
  check('tf_select_5m', (await setTf('5m')) === '5m'); await sleep(500)
  await compare('tf_5m', 'source', '5m')
  await setTf('1m'); await sleep(500)
  r = await compare('tf_back_1m')
  // 5. same-time correction through real push_publish (closed bar k)
  const last = r.times.at(-1); const k = Math.round((last - info.t0) / 60) - 2
  const tk = info.t0 + 60 * k
  const before = r.got[tk]
  await mailbox({ op: 'publish', k, close: 31234.5, dcvd: 777.25 })
  await waitFor(`(${L}.values().get(${JSON.stringify(r.key + '|source')}) ?? new Map()).get(${tk}) !== ${JSON.stringify(before)} && ${idle}`, 20000).catch(() => {})
  await sleep(500); await waitFor(idle)
  r = await compare('after_correction')
  check('correction_changed_value', !Object.is(r.got[tk], before), `${before} -> ${r.got[tk]}`)
  // 6. invalidation + repair
  await mailbox({ op: 'publish', k, close: NaN })
  await sleep(800); await waitFor(idle)
  const afterInv = await values(r.key, 'source')
  check('invalidation_withdraws_bucket', afterInv[tk] === undefined || afterInv[tk] === null, afterInv[tk])
  const sInv = new Map(await evaluate(`__cvdSrv.series(${JSON.stringify(r.key)})`))
  check('invalidation_withdraws_rendered_point', !sInv.has(tk), sInv.get(tk))
  await mailbox({ op: 'publish', k, close: 30111.25 })
  await sleep(800); await waitFor(idle)
  r = await compare('after_repair')
  check('repair_restores_bucket', r.got[tk] !== undefined && r.got[tk] !== null)
  // 7. live tick on a NEW forming bar: expected basis includes the exact emitted bar.
  liveBar = JSON.parse(await mailbox({ op: 'live', k: k + 3, close: 30500 }))
  check('live_bar_is_new_time', liveBar.time > last, `${liveBar.time} <= ${last}`)
  await waitFor(`(${L}.values().get(${JSON.stringify(r.key + '|source')}) ?? new Map()).has(${liveBar.time}) && ${idle}`, 20000)
  r = await compare('after_live', 'source', '1m', { expectTime: liveBar.time })
  // 8. independent plugin failure: hostile fixture imported, enabled, faulted.
  check('import_hostile_fixture', await importFile(path.join(W, 'hostile_wasm_plugin.wasm')))
  const hid = await evaluate(`[...${L}.records().keys()].find(k => k !== 'cvd') ?? null`)
  check('hostile_fixture_id', hid === 'hostile.fixture', hid)
  await enable(hid)
  check('hostile_enabled', await evaluate(`${L}.records().get(${JSON.stringify(hid)})?.enabled === true`))
  await waitFor(`!!${L}.compute().fault(${JSON.stringify(hid)}) && ${idle}`, 30000)
  check('hostile_faulted', !!(await evaluate(`${L}.compute().fault(${JSON.stringify(hid)})`)))
  check('hostile_released_not_running', !(await evaluate(`${L}.compute().running().includes(${JSON.stringify(hid)})`)))
  check('cvd_still_running', await evaluate(`${L}.compute().running().includes('cvd')`))
  // Live replacement of the same forming bar after the fault: cvd stays exact + rendered.
  const prevLive = r.got[liveBar.time]
  liveBar = JSON.parse(await mailbox({ op: 'live', k: k + 3, close: 30777.5, dcvd: 4242.5 }))
  await waitFor(`(${L}.values().get(${JSON.stringify(r.key + '|source')}) ?? new Map()).get(${liveBar.time}) !== ${JSON.stringify(prevLive)} && ${idle}`, 20000)
  r = await compare('peer_failure_isolated', 'source', '1m', { expectTime: liveBar.time })
  check('peer_failure_live_value_changed', !Object.is(r.got[liveBar.time], prevLive), `${prevLive} -> ${r.got[liveBar.time]}`)
  check('no_page_exceptions', exceptions.length === 0, exceptions.join('\n'))
  // Narrow allowlist: only the existing product Google Fonts stylesheet/font GETs.
  const allowed = e => e.method === 'GET' && ((e.url.startsWith('https://fonts.googleapis.com/css2?') && e.type === 'Stylesheet') || (e.url.startsWith('https://fonts.gstatic.com/') && e.type === 'Font'))
  const ext = external.filter(e => !allowed(e))
  console.log(`NOTE allowlisted Google Fonts GET requests: ${external.length - ext.length} (existing product typography; not plugin/data traffic)`)
  check('no_external_requests_outside_font_allowlist', ext.length === 0, JSON.stringify(ext))
  exit = checks.every(c => c.ok) ? 0 : 1
} catch (e) { console.log('FAIL harness', e.stack); exit = 1 } finally {
  try { if (info) await bounded(mailbox({ op: 'stop' }), 15000, 'stop') } catch (e) { console.log('FAIL stop mailbox', e.message); exit = 1 }
  try { oracle?.close() } catch {}
  try { ws?.close() } catch {}
  if (chrome) {
    const td = await teardown({ child: chrome, profileDir: profile })
    console.log(`TEARDOWN ${JSON.stringify(td)}`)
    if (!td.ok) exit = 1
  }
  console.log(`${checks.filter(c => c.ok).length}/${checks.length} checks passed`)
  process.exit(exit)
}
