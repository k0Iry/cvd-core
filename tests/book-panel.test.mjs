// book-panel.js unit contracts with a fake WebSocket, fake host and fake
// timers (browser-free). Real Chromium/WASM coverage: tests/book-browser.mjs.
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createBookPanel, bookSupported, isBookDescriptor, BOOK_STALE_MS } from '../static/js/book-panel.js'
import { createCoordinator } from '../static/wasm-host/host-bridge.js'
// Real Rust coordinator (book admission authority) built by the gate.
const COORD = fs.readFileSync(process.env.CVD_COORDINATOR_WASM || new URL('../static/wasm-host/cvd_browser_coordinator.wasm', import.meta.url))

async function harness({ computeImpl, onChange } = {}) {
  const coord = await createCoordinator(COORD)
  const sockets = []
  class FakeWS { constructor(url) { this.url = String(url); this.closed = false; sockets.push(this) } close() { this.closed = true } }
  let t = 0; const due = []
  const timers = { setTimeout: (f, ms) => { const h = { at: t + ms, f, live: true }; due.push(h); return h }, clearTimeout: h => { if (h) h.live = false } }
  const advance = ms => { t += ms; for (;;) { const h = due.filter(x => x.live && x.at <= t).sort((a, b) => a.at - b.at)[0]; if (!h) break; h.live = false; h.f() } }
  const calls = { load: [], compute: [], disable: [], supersede: [] }
  const host = {
    async load(inst) { calls.load.push(inst) },
    compute(inst, scopeKey, revision, op, body) { calls.compute.push({ inst, scopeKey, revision, op, body: JSON.parse(body) }); return computeImpl ? computeImpl(inst, JSON.parse(body)) : Promise.resolve(JSON.stringify(rowsFor(JSON.parse(body)))) },
    disable(inst) { calls.disable.push(inst) },
    supersede(inst, k, r) { calls.supersede.push([inst, k, r]); return 0 },
  }
  const panel = createBookPanel({ host, coord, container: null, wsUrl: s => `ws://x/book?symbol=${s.symbol}&market=${s.market}`, WebSocketImpl: FakeWS, timers, now: () => t, onChange })
  return { panel, sockets, host, calls, advance, coord }
}
function rowsFor(b) { return { unit: b.base, best_bid: b.bids[0][0], best_ask: b.asks[0][0], rows: [{ price: b.asks[0][0], bid: 0, ask: b.asks[0][1] }, { price: b.bids[0][0], bid: b.bids[0][1], ask: 0 }] } }
const frame = (symbol, generation, seq, extra = {}) => ({ data: JSON.stringify({ type: 'book', status: 'live', source: { exchange: 'binance', market: 'usdm', symbol }, base: symbol, quote: 'USDT', generation, seq, exchange_time_ms: 1, depth_limit: 1000,
  bids: [[100, 1]], asks: [[101, 2]], coverage: { bid_floor: 99, ask_ceiling: 102 }, ...extra }) })
const flush = () => new Promise(r => setImmediate(r))
const BTC = { exchange: 'binance', market: 'usdm', symbol: 'BTC' }
const plugin = { id: 'orderbook', name: 'Order Book', bytes: new ArrayBuffer(8), params: {}, loadKey: 'a' }

test('descriptor capability and supported sources', () => {
  assert.equal(isBookDescriptor({ inputs: ['order_book'] }), true)
  assert.equal(isBookDescriptor({ inputs: ['price'] }), false)
  assert.equal(bookSupported(BTC), true)
  assert.equal(bookSupported({ ...BTC, symbol: 'ETH' }), true)
  for (const s of [{ ...BTC, market: 'spot' }, { ...BTC, exchange: 'coinbase' }, { ...BTC, symbol: 'SOL' }, { ...BTC, market: 'usdm_usdc' }]) assert.equal(bookSupported(s), false)
})

test('no plugin => no socket; unsupported source => no socket', async () => {
  const h = await harness()
  h.panel.setSource(BTC)
  assert.equal(h.sockets.length, 0)
  h.panel.setSource({ ...BTC, market: 'spot' })
  h.panel.setPlugins([plugin]); await flush()
  assert.equal(h.sockets.length, 0)
  assert.equal(h.panel.state().status.state, 'unsupported')
})

test('live frame computes via OP_BOOK; non-live frame withdraws rows', async () => {
  const h = await harness()
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  assert.equal(h.sockets.length, 1)
  h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush()
  assert.equal(h.calls.compute.length, 1)
  assert.equal(h.calls.compute[0].op, 4)
  assert.equal(h.calls.compute[0].body.source.symbol, 'BTC')
  assert.deepEqual(h.panel.state().plugins[0], { id: 'orderbook', state: 'ready', fault: null, publicCode: null, hint: null, rows: 2, unit: 'BTC', view: 'live', seq: 10, generation: 1, bestBid: 100, ageMs: 0 })
  h.sockets[0].onmessage({ data: JSON.stringify({ type: 'book', status: 'stale', reason: 'sequencegap', source: BTC, bids: [], asks: [] }) })
  assert.equal(h.panel.state().status.state, 'stale')
  assert.equal(h.panel.state().plugins[0].rows, 0)
  // negative control: a live-status frame with empty sides is not drawn as live
  h.sockets[0].onmessage(frame('BTC', 2, 1, { bids: [] }))
  assert.equal(h.panel.state().status.state, 'syncing')
})

test('client stale timer withdraws a silent live book', async () => {
  const h = await harness()
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush()
  h.advance(BOOK_STALE_MS - 1)
  assert.equal(h.panel.state().status.state, 'live')
  h.sockets[0].onmessage(frame('BTC', 1, 11)); await flush()
  h.advance(BOOK_STALE_MS - 1)
  assert.equal(h.panel.state().status.state, 'live', 'timer re-armed by fresh frame')
  h.advance(1)
  assert.deepEqual(h.panel.state().status, { state: 'stale', reason: 'no_update' })
  assert.equal(h.panel.state().plugins[0].rows, 0)
})

test('disconnect withdraws then reconnects with backoff', async () => {
  const h = await harness()
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush()
  h.sockets[0].onclose()
  assert.equal(h.panel.state().status.state, 'disconnected')
  assert.equal(h.panel.state().plugins[0].rows, 0)
  h.advance(499); assert.equal(h.sockets.length, 1)
  h.advance(1); assert.equal(h.sockets.length, 2)
  h.sockets[1].onmessage(frame('BTC', 1, 12)); await flush()
  assert.equal(h.panel.state().status.state, 'live')
})

test('source switch fences delayed frames and results from the old source', async () => {
  let release
  const h = await harness({ computeImpl: (inst, b) => new Promise(r => { release = () => r(JSON.stringify(rowsFor(b))) }) })
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush()
  const old = h.sockets[0]
  h.panel.setSource({ ...BTC, symbol: 'ETH' })
  assert.equal(old.closed, true)
  assert.match(h.sockets[1].url, /symbol=ETH/)
  release(); await flush()
  assert.equal(h.panel.state().plugins[0].rows, 0, 'old BTC result not drawn')
  old.onmessage?.(frame('BTC', 1, 11))
  const fenced = h.panel.state().counters.fenced
  h.sockets[1].onmessage(frame('BTC', 1, 12)) // wrong source on new socket
  assert.equal(h.panel.state().counters.fenced, fenced + 1)
  assert.notEqual(h.panel.state().status.state, 'live')
  assert.ok(h.calls.supersede.some(([, k]) => k === 'book:binance/usdm/ETH'))
})

test('coalesces: one compute in flight, latest frame next', async () => {
  const pend = []
  const h = await harness({ computeImpl: (inst, b) => new Promise(r => pend.push(() => r(JSON.stringify(rowsFor(b))))) })
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  for (let s = 1; s <= 5; s++) h.sockets[0].onmessage(frame('BTC', 1, s))
  assert.equal(h.calls.compute.length, 1)
  pend.shift()(); await flush(); await flush()
  assert.equal(h.calls.compute.length, 2)
  assert.equal(h.calls.compute[1].body.seq, 5)
})

test('plugin fault is isolated and disable releases instance and socket', async () => {
  const h = await harness({ computeImpl: inst => inst === 2 ** 29 ? Promise.reject(Object.assign(new Error('x'), { code: 'trap' })) : Promise.resolve(null) })
  const h2 = { ...plugin, id: 'other', loadKey: 'b' }
  h.host.compute = (inst, k, r, op, body) => inst === 2 ** 29 ? Promise.reject(Object.assign(new Error('x'), { code: 'trap' })) : Promise.resolve(JSON.stringify(rowsFor(JSON.parse(body))))
  h.panel.setSource(BTC); h.panel.setPlugins([plugin, h2]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush(); await flush()
  const st = h.panel.state().plugins
  assert.deepEqual(st.map(p => [p.id, p.state, p.fault, p.rows]), [['orderbook', 'fault', 'trap', 0], ['other', 'ready', null, 2]])
  h.panel.setPlugins([]); await flush()
  assert.deepEqual(h.calls.disable.sort(), [2 ** 29, 2 ** 29 + 1])
  assert.equal(h.sockets[0].closed, true)
  assert.equal(h.panel.state().status.state, 'off')
})

// ---------------------------------------------------------- correction A
// Admission/freshness policy lives in the Rust coordinator; these drive the
// real adapter (book-panel.js) against the real coordinator wasm.
const view = h => h.panel.state().plugins[0]

test('A: feed faster than compute keeps output advancing; basis is the computed frame', async () => {
  const pend = []
  const h = await harness({ computeImpl: (inst, b) => new Promise(r => pend.push(() => r(JSON.stringify(rowsFor(b))))) })
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  let seq = 0
  const shown = []
  for (let k = 0; k < 10; k++) {
    for (let j = 0; j < 3; j++) { seq++; h.sockets[0].onmessage(frame('BTC', 1, seq, { bids: [[100 + seq, 1]], coverage: { bid_floor: 99, ask_ceiling: 102 } })); h.advance(50) }
    pend.shift()(); await flush(); await flush()
    shown.push(view(h).seq)
    assert.equal(view(h).bestBid, 100 + view(h).seq, 'BBO/seq from the same computed input')
  }
  assert.ok(shown.every((s, i) => i === 0 || s > shown[i - 1]), `monotonic ${shown}`)
  assert.ok(seq - shown.at(-1) <= 3, `bounded lag ${seq} vs ${shown.at(-1)}`)
  assert.ok(['live', 'updating'].includes(view(h).view))
})

test('A: an expired result is not refreshed by new raw frames', async () => {
  const pend = []
  const h = await harness({ computeImpl: (inst, b) => b.seq === 1 ? Promise.resolve(JSON.stringify(rowsFor(b))) : new Promise(r => pend.push(r)) })
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 1)); await flush(); await flush()
  assert.equal(view(h).seq, 1)
  for (let s = 2; s < 40; s++) { h.sockets[0].onmessage(frame('BTC', 1, s)); h.advance(200) }
  assert.equal(h.panel.state().status.state, 'live', 'raw feed itself is live')
  assert.equal(view(h).view, 'stale', 'old computed result is not labelled fresh')
  assert.equal(view(h).rows, 0, 'stale result withdrawn')
})

test('A: late success and late error during source switch / resync are fenced', async () => {
  for (const kind of ['switch', 'resync']) {
    for (const fail of [false, true]) {
      let settle
      const h = await harness({ computeImpl: (inst, b) => new Promise((ok, no) => { settle = () => fail ? no(Object.assign(new Error('x'), { code: 'plugin_error' })) : ok(JSON.stringify(rowsFor(b))) }) })
      h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
      h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush()
      if (kind === 'switch') h.panel.setSource({ ...BTC, symbol: 'ETH' })
      else h.sockets[0].onmessage(frame('BTC', 2, 1))
      const s = settle; s(); await flush(); await flush()
      const v = view(h)
      assert.equal(v.fault, null, `${kind}/${fail}: no stale fault`)
      assert.ok(v.seq == null || (kind === 'resync' && v.generation === 2), `${kind}/${fail}: old result not shown ${JSON.stringify(v)}`)
    }
  }
})

test('A: params edit recovers a recoverable fault; menu notified asynchronously; no host internals', async () => {
  let changes = 0
  const h = await harness({ onChange: () => changes++, computeImpl: (inst, b) => inst === 2 ** 29 ? Promise.reject(Object.assign(new Error('bad group'), { code: 'plugin_error' })) : Promise.resolve(JSON.stringify(rowsFor(b))) })
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 10)); const c0 = changes; await flush(); await flush()
  assert.equal(view(h).fault, 'plugin_error')
  assert.ok(changes > c0, 'async fault change notifies the menu')
  h.panel.setPlugins([{ ...plugin, loadKey: 'b', params: { group: 1 } }]); await flush(); await flush()
  assert.equal(view(h).fault, null)
  assert.equal(view(h).seq, 10, 'recomputed latest frame with new params')
  // Supersede that killed a Worker is detected from the public return value.
  h.host.supersede = (inst, k, r) => { h.calls.supersede.push([inst, k, r]); return 1 }
  const loads = h.calls.load.length
  h.panel.setSource({ ...BTC, symbol: 'ETH' }); await flush()
  assert.equal(h.calls.load.length, loads + 1, 'terminated Worker restarted')
})

test('closure §2: safe invalid_params hint via real coordinator; arbitrary text dropped; preaccepted fault cleared on source switch; fatal stays', async () => {
  let msg = 'invalid_params: indicator rejected the parameters'
  const h = await harness({ computeImpl: () => Promise.reject(Object.assign(new Error(msg), { code: 'plugin_error' })) })
  h.panel.setSource(BTC); h.panel.setPlugins([plugin]); await flush()
  h.sockets[0].onmessage(frame('BTC', 1, 10)); await flush(); await flush()
  let v = view(h)
  assert.deepEqual([v.state, v.fault, v.publicCode, v.hint], ['ready', 'plugin_error', 'invalid_params', 'parameters rejected — edit parameters'])
  // Pre-accepted recoverable fault -> source switch -> no new result yet: not labelled on the new basis.
  h.panel.setSource({ ...BTC, symbol: 'ETH' }); await flush()
  v = view(h)
  assert.deepEqual([v.fault, v.hint, v.state], [null, null, 'ready'])
  // Arbitrary plugin/host text never becomes a public classification.
  msg = '/home/u/.secret/plugin.rs:42 panicked'
  h.sockets.at(-1).onmessage(frame('ETH', 1, 5)); await flush(); await flush()
  v = view(h)
  assert.deepEqual([v.fault, v.publicCode, v.hint], ['plugin_error', null, 'plugin_error'])
  // Fatal Worker failure stays visible across withdrawal (never "starting").
  const f = await harness({ computeImpl: () => Promise.reject(Object.assign(new Error('gone'), { code: 'worker_crashed' })) })
  f.panel.setSource(BTC); f.panel.setPlugins([plugin]); await flush()
  f.sockets[0].onmessage(frame('BTC', 1, 10)); await flush(); await flush()
  f.panel.setSource({ ...BTC, symbol: 'ETH' }); await flush()
  v = view(f)
  assert.deepEqual([v.state, v.fault], ['fault', 'worker_crashed'])
})
