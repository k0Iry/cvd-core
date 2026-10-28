// Bounded Chromium acceptance of the REAL static frontend against an exported
// producer corpus (CVD_PRODUCER_CORPUS). The corpus is SYNTHETIC input bars/trades
// serialized by the ACTUAL production app_router (/meta, /history, /ws) in the
// opt-in PostgreSQL test `postgres_router_http_ws_isolate_usdt_usdc_sources_across_timeframes`.
// This loopback server only replays those recorded bodies keyed by request; it
// never builds a response. Not production data and not Safari acceptance.
// Plugins: when the corpus was exported with CVD_PUBLIC_INDICATOR_DIR, /indicators and
// every /history + /ws plugin envelope are ACTUAL producer output of the PUBLIC CVD/Flow
// libraries via the independent runner, and rendered readouts are asserted. Without it
// (bare-host corpus) /indicators is an explicit MOCK empty list. /health is always a MOCK.
// Windowing of recorded pages (historyBody) re-slices actual producer bars only.
import fs from 'node:fs'
import http from 'node:http'
import path from 'node:path'
import crypto from 'node:crypto'
import os from 'node:os'
import { fileURLToPath } from 'node:url'
import { spawn } from 'node:child_process'
import assert from 'node:assert/strict'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const SRC = path.resolve(HERE, '../static')
const corpus = JSON.parse(fs.readFileSync(process.env.CVD_PRODUCER_CORPUS || (() => { throw new Error('CVD_PRODUCER_CORPUS required') })(), 'utf8'))
const OUT = process.env.CVD_R_OUT || null
const CHROMIUM = process.env.CVD_CHROMIUM || '/usr/bin/chromium'
const requests = []
const missing = []
// Server contract: spot may omit series (== spot-v1); contracts must send contracts-drb-v1.
const keyOf = sp => ['exchange', 'market', 'symbol', 'tf', 'series'].map(k => `${k}=${sp.get(k) ?? (k === 'series' && (sp.get('market') || '').startsWith('spot') ? 'spot-v1' : '')}`).join('&')

function historyBody(sp) {
    const base = keyOf(sp)
    const full = corpus.history[base]
    if (!full) return null
    const before = sp.get('before'), after = sp.get('after')
    const limit = Number(sp.get('limit') || 800)
    if (before && corpus.history[`${base}&limit=2&before=${before}`] && limit <= 2) return corpus.history[`${base}&limit=2&before=${before}`]
    if (!before && !after && limit >= full.bars.length) return full
    // Window over the recorded producer page only (no value synthesis).
    let bars = full.bars.filter(b => (!before || b.time < Number(before)) && (!after || b.time > Number(after)))
    const more = bars.length > limit
    bars = before ? bars.slice(-limit) : bars.slice(0, limit)
    return { ...full, bars, has_more_before: before ? more : full.has_more_before && !after, has_more_after: after ? more : false,
        oldest_time: bars[0]?.time ?? null, newest_time: bars.at(-1)?.time ?? null }
}
const sockets = new Set()
const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1')
    const json = body => { res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(body)) }
    if (url.pathname === '/meta') return json(corpus.meta)
    if (url.pathname === '/indicators') return json(corpus.indicators_endpoint ?? { api_version: 1, indicators: [] }) // actual, or MOCK empty for bare-host corpus
    if (url.pathname === '/health') return json( // MOCK health (not producer output)
       { healthy: true, ready: true, sources: [], indicators: [] })
    if (url.pathname === '/history') {
        requests.push({ kind: 'http', q: url.search })
        const body = historyBody(url.searchParams)
        if (!body) { missing.push(url.search); res.writeHead(404); return res.end('not in producer corpus') }
        return json(body)
    }
    const file = url.pathname === '/' ? path.join(SRC, 'index.html') : path.resolve(SRC, '.' + decodeURIComponent(url.pathname))
    if (!(file === SRC || file.startsWith(SRC + path.sep)) || !fs.existsSync(file) || !fs.statSync(file).isFile()) { res.writeHead(404); return res.end() }
    let data = fs.readFileSync(file)
    if (file === path.join(SRC, 'js/app.js')) {
        // Test-only read bridge (no behavior change): expose module-scoped state.
        const src = String(data)
        if (!/\ninit\(\)\s*$/.test(src)) throw new Error('app.js init anchor not found')
        data = src.replace(/\ninit\(\)\s*$/, '\nwindow.__r = { page, useAggregateMode, indicatorStore, get registry() { return registry }, summaries: () => paneSummaryEls.map(e => e.textContent) }\ninit()\n')
    }
    if (file.endsWith('index.html')) data = String(data).replace(/<link[^>]*fonts\.(googleapis|gstatic)[^>]*>/g, '')
    res.writeHead(200, { 'Content-Type': file.endsWith('.html') ? 'text/html' : file.endsWith('.css') ? 'text/css' : /\.m?js$/.test(file) ? 'text/javascript' : 'application/octet-stream', 'Cache-Control': 'no-store' })
    res.end(data)
})
function wsText(socket, text) {
    const p = Buffer.from(text)
    const head = p.length < 126 ? Buffer.from([0x81, p.length]) : p.length < 65536 ? Buffer.from([0x81, 126, p.length >> 8, p.length & 255]) : (() => { const h = Buffer.alloc(10); h[0] = 0x81; h[1] = 127; h.writeBigUInt64BE(BigInt(p.length), 2); return h })()
    socket.write(Buffer.concat([head, p]))
}
server.on('upgrade', (req, socket) => {
    const url = new URL(req.url, 'http://127.0.0.1')
    const frames = corpus.ws[keyOf(url.searchParams)]
    requests.push({ kind: 'ws', q: url.search })
    const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64')
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
    sockets.add(socket); socket.on('data', () => {}); socket.on('close', () => sockets.delete(socket)); socket.on('error', () => sockets.delete(socket))
    if (!frames) { missing.push('ws' + url.search); return }
    setTimeout(() => frames.forEach(f => wsText(socket, JSON.stringify(f))), 150)
})
await new Promise(r => server.listen(0, '127.0.0.1', r))
const PORT = server.address().port

const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'cvd-r-'))
const chrome = spawn(CHROMIUM, ['--headless=new', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--no-first-run',
    '--disable-background-networking', '--window-size=1280,800', '--remote-debugging-port=0', '--user-data-dir=' + profileDir, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'], detached: true }) // own process group: /usr/bin/chromium is a sh wrapper
const chromeExited = new Promise(r => { if (chrome.exitCode !== null) r(); chrome.once('exit', r) })
let socket = null
async function cleanup() {
    try { socket?.close() } catch {}
    try { process.kill(-chrome.pid, 'SIGKILL') } catch {}
    for (let i = 0; i < 50; i++) { try { process.kill(-chrome.pid, 0) } catch { break } await new Promise(r => setTimeout(r, 100)) } // wait for owned group
    await Promise.race([chromeExited, new Promise(r => setTimeout(r, 5000))])
    for (const s of sockets) s.destroy()
    await new Promise(r => server.close(() => r()))
    fs.rmSync(profileDir, { recursive: true, force: true })
}
const hard = setTimeout(async () => { console.error('hard timeout'); await cleanup(); process.exit(3) }, 150000)
let failed = null
try {
const devtools = await new Promise((resolve, reject) => {
    let buf = ''
    chrome.stderr.on('data', d => { buf += d; const m = buf.match(/DevTools listening on (ws:\/\/\S+)/); if (m) resolve(m[1]) })
    chrome.on('exit', () => reject(new Error('chromium exited')))
})
const targets = await (await fetch(devtools.replace('ws://', 'http://').replace(/\/devtools\/browser\/.*/, '/json'))).json()
socket = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl)
await new Promise(r => socket.addEventListener('open', r))
let seq = 0; const pending = new Map(); const errors = []
socket.addEventListener('message', e => {
    const m = JSON.parse(e.data)
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) }
    if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text)
})
const call = (method, params = {}) => new Promise(r => { const id = ++seq; pending.set(id, r); socket.send(JSON.stringify({ id, method, params })) })
const evaluate = async expr => { const r = await call('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 800)); return r.result.result.value }
const sleep = ms => new Promise(r => setTimeout(r, ms))
async function waitFor(expr, ms = 10000) { const end = Date.now() + ms; while (!(await evaluate(expr))) { if (Date.now() > end) { console.error(JSON.stringify({ requests: requests.slice(-6), missing, errors, bars: await evaluate('__r.page.bars.slice(0,3)') })); throw new Error('timeout: ' + expr) }; await sleep(50) } }
await call('Runtime.enable'); await call('Page.enable')
await call('Page.navigate', { url: `http://127.0.0.1:${PORT}/` })
await waitFor(`window.__r && document.getElementById('market').options.length > 0`)

const code = (s, m) => s * 4 + m + 1
const MARKETS = ['spot', 'spot_usdc', 'usdm', 'usdm_usdc']
const LABEL = { spot: 'USDT spot', spot_usdc: 'USDC spot', usdm: 'USDT linear perpetual', usdm_usdc: 'USDC linear perpetual' }
const QUOTE = { spot: 'USDT', spot_usdc: 'USDC', usdm: 'USDT', usdm_usdc: 'USDC' }
const results = []
const PLUGINS = (corpus.public_plugins || []).length > 0
if (PLUGINS) assert.deepEqual([...corpus.public_plugins].sort(), ['cvd', 'flow'])
// Real rendered plugin values vs ACTUAL producer envelopes (history + WS) and quote-unit readout.
const OUTPUTS = [['cvd', 'cvd'], ['flow', 'delta'], ['flow', 'buy_sell'], ['flow', 'total']]
const valueAt = (series, i) => series.type === 'dual_range' ? { upper: series.upper[i], lower: series.lower[i] } : series.values[i]
const normal = v => v !== null && v !== undefined && (typeof v !== 'object' || (Number.isFinite(v.upper) && Number.isFinite(v.lower)))
const BUY_SELL_SEL = 'select[data-group]:has(option[value="flow/buy_sell"])'
// Real rendered plugin values vs ACTUAL producer envelopes (history + WS), checked PER output id/key.
async function checkPlugins(market, tf, hist, aggregate, wsFrames = [], buySellCheck = false) {
    const mode = aggregate ? 'aggregate' : 'source'
    const want = new Map()
    const add = (id, out) => { for (const [oid, key] of OUTPUTS) { if (oid !== id) continue; const ser = out?.series?.[key]; assert.ok(ser, `${market} ${tf} ${mode} producer ${id}/${key} missing`); out.times.forEach((t, i) => want.set(`${id}|${key}|${t}`, valueAt(ser, i))) } }
    for (const id of ['cvd', 'flow']) { const o = hist.indicators[id].modes[mode]; assert.equal(o.status, 'ok', `${id} ${mode} status`); add(id, o.output) }
    for (const f of wsFrames) if (f.type === 'indicator' && f.mode === mode) add(f.id, f.output)
    for (const [k, v] of want) assert.ok(normal(v), `${market} ${tf} ${mode} ${k}: producer output null/non-finite (fixture must reconcile)`)
    await waitFor(`__r.summaries().some(t => t.includes('CVD'))`)
    const got = await evaluate(`(() => { const outs = __r.registry.panes.flatMap(p => p.outputs).filter(o => ['cvd','flow'].includes(o.pluginId));
        const r = {}; for (const o of outs) for (const b of __r.page.bars) r[o.pluginId + '|' + o.key.split('/').pop() + '|' + b.time] = __r.indicatorStore.get(o.key, ${JSON.stringify(mode)}, b.time) ?? null;
        return { r, keys: outs.map(o => o.key), summaries: __r.summaries() } })()`)
    const matched = Object.fromEntries(OUTPUTS.map(([id, key]) => [`${id}/${key}`, 0]))
    for (const [k, v] of want) {
        if (!(k in got.r)) continue
        assert.deepEqual(got.r[k], v, `${market} ${tf} ${mode} ${k}: rendered ${JSON.stringify(got.r[k])} != producer ${JSON.stringify(v)}`)
        const [id, key] = k.split('|'); matched[`${id}/${key}`]++
    }
    for (const [o, n] of Object.entries(matched)) assert.ok(n >= 2, `${market} ${tf} ${mode} ${o}: only ${n} rendered values matched producer (keys ${got.keys})`)
    const unit = aggregate ? 'nominal USD/USDT/USDC quote · no FX conversion' : `${QUOTE[market]} quote turnover`
    assert.ok(got.summaries.some(t => t.includes(unit) && t.includes('CVD')), `readout unit ${unit}: ${got.summaries}`)
    let buySell = null
    if (buySellCheck) {
        // Switch the real Flow pane mode select to Buy/Sell and read the visible readout for the latest bar.
        const prev = await evaluate(`(() => { const s = document.querySelector(${JSON.stringify(BUY_SELL_SEL)}); if (!s) return null; const p = s.value; s.value = 'flow/buy_sell'; s.onchange(); return p })()`)
        assert.ok(prev, 'Flow Buy/Sell mode select present')
        await sleep(200)
        const r = await evaluate(`(() => ({ time: __r.page.bars.at(-1).time, mode: document.querySelector(${JSON.stringify(BUY_SELL_SEL)}).value, summaries: __r.summaries() }))()`)
        assert.equal(r.mode, 'flow/buy_sell')
        const v = want.get(`flow|buy_sell|${r.time}`); assert.ok(normal(v), `producer buy_sell for latest bar ${r.time}`)
        const fmt = x => Math.abs(x) >= 1e3 ? `${(x / 1e3).toFixed(2)}K` : x.toLocaleString(undefined, { maximumFractionDigits: 2 })
        const text = `Buy/Sell ${fmt(v.upper)} / ${fmt(v.lower)}`
        assert.ok(r.summaries.some(t => t.includes(text) && t.includes(unit)), `Buy/Sell readout "${text} … ${unit}": ${r.summaries}`)
        await evaluate(`(() => { const s = document.querySelector(${JSON.stringify(BUY_SELL_SEL)}); s.value = ${JSON.stringify(prev)}; s.onchange() })()`)
        buySell = { time: r.time, producer: v, text, summary: r.summaries.find(t => t.includes(text)) }
    }
    return { matched, summary: got.summaries.find(t => t.includes(unit)), buySell }
}
async function select(exchange, symbol, market, tf) {
    await evaluate(`(() => { const e = document.getElementById('exchange'); e.value = ${JSON.stringify(exchange)}; e.onchange() })()`)
    await evaluate(`(() => { const s = document.getElementById('symbol'); s.value = ${JSON.stringify(symbol)}; s.onchange() })()`)
    await evaluate(`(() => { const m = document.getElementById('market'); m.value = ${JSON.stringify(market)}; m.onchange() })()`)
    await evaluate(`(() => { const t = document.getElementById('tf'); t.value = ${JSON.stringify(tf)}; t.onchange() })()`)
}
const state = () => evaluate(`({ market: document.getElementById('market').value, symbol: document.getElementById('symbol').value, tf: document.getElementById('tf').value,
    label: document.getElementById('source-series').textContent, title: document.getElementById('source-series').title,
    bars: __r.page.bars.map(b => ({ time: b.time, price_close: b.price_close, cvd_usdt: b.cvd_usdt, ac: b.ac })) })`)
let prevCode = null
for (const [s, symbol] of ['BTC', 'ETH'].entries()) {
    for (const [m, market] of MARKETS.entries()) {
        for (const tf of ['1m', '1h']) {
            await select('binance', symbol, market, tf)
            const c = code(s, m)
            await waitFor(`document.getElementById('market').value === ${JSON.stringify(market)} && __r.page.bars.length > 0 && __r.page.bars.every(b => Math.floor(b.price_close / 100) === ${c})`)
            await sleep(400) // let replayed actual WS frames land
            const st = await state()
            assert.equal(st.label, LABEL[market])
            assert.match(st.title, new RegExp(`binance ${symbol}${QUOTE[market]}; source quote turnover ${QUOTE[market]}`))
            for (const b of st.bars) {
                assert.equal(Math.floor(b.price_close / 100), c, `${symbol} ${market} ${tf}: foreign price ${b.price_close}`)
                assert.equal(Math.floor(b.cvd_usdt / 1000), c, `${symbol} ${market} ${tf}: foreign cvd ${b.cvd_usdt}`)
            }
            const hist = corpus.history[`exchange=binance&market=${market}&symbol=${symbol}&tf=${tf}&series=${market.startsWith('spot') ? 'spot-v1' : 'contracts-drb-v1'}`]
            for (const hb of hist.bars) {
                const shown = st.bars.find(b => b.time === hb.time)
                assert.ok(shown, `history bar ${hb.time} displayed`)
                assert.equal(shown.cvd_usdt, hb.cvd_usdt); assert.equal(shown.ac, hb.ac)
            }
            const live = st.bars.some(b => b.price_close === c * 100 + 50)
            assert.ok(prevCode === null || !st.bars.some(b => Math.floor(b.price_close / 100) === prevCode && prevCode !== c), 'no old-source points after switch')
            prevCode = c
            assert.ok(live, `${symbol} ${market} ${tf}: actual producer WS bar frame not applied`)
            const wsKey = `exchange=binance&market=${market}&symbol=${symbol}&tf=${tf}&series=${market.startsWith('spot') ? 'spot-v1' : 'contracts-drb-v1'}`
            const plugins = PLUGINS ? await checkPlugins(market, tf, hist, false, corpus.ws[wsKey] || [], market === 'spot_usdc' && tf === '1m') : null
            results.push({ symbol, market, tf, label: st.label, bars: st.bars.length, liveFrameApplied: live, plugins })
        }
    }
}
// Aggregate mode on BTC usdm_usdc 1m: canonical ac values come from producer bars.
await select('binance', 'BTC', 'usdm_usdc', '1m')
await waitFor(`__r.page.bars.length > 0 && __r.page.bars.every(b => Math.floor(b.price_close / 100) === 4)`)
await evaluate(`(() => { const a = document.getElementById('toggle-agg'); a.checked = true; a.onchange() })()`)
await sleep(500)
const agg = await evaluate(`({ agg: __r.useAggregateMode(), label: document.getElementById('source-series').textContent, acs: __r.page.bars.map(b => b.ac) })`)
assert.equal(agg.agg, true)
const aggHist = corpus.history['exchange=binance&market=usdm_usdc&symbol=BTC&tf=1m&series=contracts-drb-v1'].bars.map(b => b.ac)
for (const v of aggHist) assert.ok(agg.acs.includes(v), 'aggregate value from producer')
const aggPlugins = PLUGINS ? await checkPlugins('usdm_usdc', '1m', corpus.history['exchange=binance&market=usdm_usdc&symbol=BTC&tf=1m&series=contracts-drb-v1'], true, corpus.ws['exchange=binance&market=usdm_usdc&symbol=BTC&tf=1m&series=contracts-drb-v1'] || [], true) : null
await evaluate(`(() => { const a = document.getElementById('toggle-agg'); a.checked = false; a.onchange() })()`)
// Empty-history honesty: Coinbase USDC perpetual has no rows in the corpus.
await select('coinbase', 'BTC', 'usdc', '1m')
await sleep(1200)
const empty = await evaluate(`({ market: document.getElementById('market').value, n: __r.page.bars.length, label: document.getElementById('source-series').textContent, status: document.getElementById('status').textContent })`)
assert.equal(empty.market, 'usdc'); assert.equal(empty.n, 0, 'no invented history')
if (OUT) {
    const shot = await call('Page.captureScreenshot', { format: 'png' })
    fs.writeFileSync(path.join(OUT, 'browser-empty-coinbase-usdc.png'), Buffer.from(shot.result.data, 'base64'))
}
const report = { label: corpus.label, results, aggregate: { acsShown: agg.acs.length, label: agg.label, plugins: aggPlugins }, pluginMode: PLUGINS ? 'actual public cvd+flow' : 'bare-host (MOCK empty /indicators)', empty, missing, requests: requests.length, errors }
if (OUT) fs.writeFileSync(path.join(OUT, 'browser-result.json'), JSON.stringify({ ...report, requestLog: requests }, null, 2))
console.log(JSON.stringify(report, null, 1))
assert.deepEqual(errors, [], 'no page exceptions')
assert.ok(!missing.some(m => m.includes('binance')), 'every binance request served from producer corpus: ' + missing.join(' '))
} catch (e) { failed = e }
clearTimeout(hard)
await cleanup()
if (failed) { console.error(failed); process.exit(1) }
console.log('browser-usdc-producer: PASS')
