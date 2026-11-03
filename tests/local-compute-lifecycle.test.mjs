// Page-lifecycle (tab away/return) tests for the Worker driver in
// static/wasm-host/host-bridge.js with the REAL Rust coordinator. The host is
// scripted to model the bridge contract: `disable(inst)` terminates the exact
// incarnation and rejects its in-flight request with `disabled`; a request
// still in flight when a (wall-clock) deadline timer fires rejects `deadline`.
// EMULATION ONLY: this does not prove Safari scheduling (see audit P).
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createLocalCompute, createCoordinator } from '../static/wasm-host/host-bridge.js'
const INPUT_FIELDS = ['po', 'ph', 'pl', 'price_close', 'apo', 'aph', 'apl', 'apc', 'cvd_usdt', 'ac', 'delta_usdt', 'ad', 'bvb', 'abvb', 'svb', 'asvb', 'buy_volume_usdt', 'agb', 'sell_volume_usdt', 'ags']
const COORD = fs.readFileSync(process.env.CVD_COORDINATOR_WASM || new URL('../static/wasm-host/cvd_browser_coordinator.wasm', import.meta.url))
const wire = (t, k = 0) => Object.fromEntries([['time', t], ...INPUT_FIELDS.map((f, i) => [f, i + t / 60 + k]), ['trades', 3]])
const scope = { exchange: 'binance', market: 'usdm', symbol: 'BTCUSDT', tf: '30m', mode: 'aggregate' }
const tick = (n = 6) => new Promise(r => { let i = 0; const f = () => ++i >= n ? r() : setTimeout(f, 0); f() })
const out = times => JSON.stringify({ times, series: {} })
const err = c => Object.assign(new Error(c), { code: c })
const bars = n => Array.from({ length: n }, (_, i) => wire((i + 1) * 60))
const answer = p => p.resolve(out(p.op === 1 ? p.body.bars.slice(p.body.output_from).map(b => b.time) : p.op === 2 ? [p.body.bars.at(-1).time] : p.body.ops.map(o => o.bar.time)))

function host() {
  const calls = []; const pending = []; let gen = 0; const live = new Map()
  const h = {
    calls, pending, live,
    load: inst => { calls.push(['load', inst]); live.set(inst, ++gen); return Promise.resolve({ descriptor: { live: true, context: { bars: 2 } } }) },
    compute: (inst, scopeKey, rev, op, body) => new Promise((resolve, reject) => {
      calls.push(['compute', inst, op]); pending.push({ resolve, reject, inst, gen: live.get(inst), op, body: JSON.parse(body) })
    }),
    supersede: () => 0,
    disable: inst => {
      calls.push(['disable', inst]); live.delete(inst)
      for (const p of pending.filter(p => p.inst === inst)) { pending.splice(pending.indexOf(p), 1); p.reject(err('disabled')) }
    },
  }
  return h
}
async function setup() {
  const h = host(); const results = []; const statuses = []; const logs = []
  const coord = await createCoordinator(COORD)
  const c = createLocalCompute({ host: h, coord, onResult: (id, m, o, want, meta) => results.push({ id, kind: meta.kind, rev: want.revision }),
    onStatus: (id, s) => statuses.push([id, s.state, s.code]), log: (msg, id, code) => logs.push([msg, id, String(code)]) })
  return { h, c, results, statuses, logs }
}

test('RED->GREEN: tab away while compute in flight, return: no deadline fault, fresh load + recompute without a new tick', async () => {
  const { h, c, results } = await setup()
  c.setPlugins([{ id: 'cvd', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(200) }); await tick()
  const inflight = h.pending[0]; assert.equal(inflight.op, 1)
  // Page becomes hidden (visibilitychange/pagehide). The app calls suspend().
  c.suspend?.()
  await tick()
  // Return: the wall-clock deadline timer of a request that is STILL in flight
  // fires first (Safari resumes timers before the suspended Worker replies).
  if (h.pending.includes(inflight)) { h.pending.splice(h.pending.indexOf(inflight), 1); inflight.reject(err('deadline')) }
  await tick()
  c.resume?.()
  await tick()
  assert.equal(c.fault('cvd'), null, 'lifecycle interruption is not an algorithm fault')
  assert.ok(h.calls.some(x => x[0] === 'disable'), 'in-flight Worker physically cancelled on hide')
  // Reconstructed on return from the latest demand, no market tick needed.
  assert.equal(h.pending.length, 1); assert.equal(h.pending[0].op, 1); assert.notEqual(h.pending[0].gen, inflight.gen, 'fresh incarnation')
  answer(h.pending.shift()); await tick(); answer(h.pending.shift()); await tick()
  assert.deepEqual(results.map(r => r.kind), ['history', 'live'])
  // Late old reply after cancellation has no effect.
  inflight.resolve(out([60])); await tick()
  assert.equal(c.fault('cvd'), null); assert.equal(results.length, 2)
})

test('no compute is issued while hidden; demand/selection changes while away apply on return', async () => {
  const { h, c, results } = await setup()
  c.setPlugins([{ id: 'cvd', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(100) }); await tick()
  answer(h.pending.shift()); await tick(); answer(h.pending.shift()); await tick()
  const n = h.calls.length
  c.suspend()
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 1, bars: [...bars(100), wire(101 * 60)] }); await tick()
  c.setDemand({ scope: { ...scope, symbol: 'ETHUSDT' }, scopeKey: 's2', revision: 3, historyRevision: 3, bars: bars(120) }); await tick()
  assert.equal(h.calls.slice(n).filter(x => x[0] !== 'disable').length, 0, 'hidden page issues no Worker work')
  c.resume(); await tick()
  assert.ok(h.pending.length === 1 && h.pending[0].op === 1, 'History for the current selection on return')
  assert.equal(h.pending[0].body.bars.length >= 1, true)
  answer(h.pending.shift()); await tick(); answer(h.pending.shift()); await tick()
  assert.equal(results.at(-1).rev, 3); assert.equal(c.fault('cvd'), null)
})

test('repeated away/return and idle away keep the plugin healthy and bounded', async () => {
  const { h, c } = await setup()
  c.setPlugins([{ id: 'cvd', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(100) }); await tick()
  for (let i = 0; i < 5; i++) { c.suspend(); await tick(); c.resume(); await tick() }
  assert.equal(h.pending.length, 1, 'exactly one in-flight request'); assert.equal(c.fault('cvd'), null)
  answer(h.pending.shift()); await tick(); answer(h.pending.shift()); await tick()
  const loads = h.calls.filter(x => x[0] === 'load').length
  c.suspend(); await tick(); c.resume(); await tick()   // idle away: nothing in flight, nothing reloaded
  assert.equal(h.calls.filter(x => x[0] === 'load').length, loads); assert.equal(h.pending.length, 0)
})

test('foreground hang still faults with deadline (watchdog not weakened)', async () => {
  const { h, c, statuses } = await setup()
  c.setPlugins([{ id: 'cvd', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(100) }); await tick()
  h.pending.shift().reject(err('deadline')); await tick()
  assert.equal(c.fault('cvd')?.code, 'deadline'); assert.deepEqual(statuses.at(-1), ['cvd', 'fault', 'deadline'])
  assert.equal(h.pending.length, 0, 'no automatic retry')
  c.suspend(); c.resume(); await tick()
  assert.equal(c.fault('cvd')?.code, 'deadline', 'return does not clear a real fault'); assert.equal(h.pending.length, 0)
})
