// Red->green lifecycle/live tests for static/js/local-compute.mjs driven by the
// REAL Rust coordinator (static/wasm-host/cvd_browser_coordinator.wasm, built
// from crates/browser-coordinator). The plugin host is scripted so ordering
// races (slow/failed load, late errors, slow peers) are deterministic; real
// plugin WASM numerics are exercised in the Chromium gates.
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { createLocalCompute } from '../static/js/local-compute.mjs'
import { createCoordinator } from '../static/js/coordinator.mjs'
import { INPUT_FIELDS } from '../static/js/local-plugins.mjs'

// scripts/node-unit-gate.sh injects the coordinator it just built from source
// (CVD_COORDINATOR_WASM); otherwise the gate-assembled static copy is used.
const COORD = fs.readFileSync(process.env.CVD_COORDINATOR_WASM || new URL('../static/wasm-host/cvd_browser_coordinator.wasm', import.meta.url))
const wire = (t, k = 0) => Object.fromEntries([['time', t], ...INPUT_FIELDS.map((f, i) => [f, i + t / 60 + k]), ['trades', 3]])
const scope = { exchange: 'binance', market: 'spot', symbol: 'BTCUSDT', tf: '1m', mode: 'source' }
const tick = (n = 4) => new Promise(r => { let i = 0; const f = () => ++i >= n ? r() : setTimeout(f, 0); f() })
const out = times => JSON.stringify({ times, series: {} })

function scriptedHost({ live = true, loadGate = null } = {}) {
  const calls = []; const pending = []; const loads = []
  const instances = new Set()
  return {
    calls, pending, loads, instances,
    load: (inst) => new Promise((resolve, reject) => {
      calls.push(['load', inst]); instances.add(inst)
      const rec = { inst, resolve: () => resolve({ descriptor: { live, context: { bars: 2 } } }), reject: c => reject(Object.assign(new Error(c), { code: c })) }
      if (loadGate) loads.push(rec); else rec.resolve()
    }),
    compute: (inst, scopeKey, rev, op, body) => new Promise((resolve, reject) => {
      calls.push(['compute', inst, op]); pending.push({ resolve, reject, inst, scopeKey, rev, op, body: JSON.parse(body) })
    }),
    supersede: () => 0,
    disable: inst => { calls.push(['disable', inst]); instances.delete(inst); for (const l of loads.filter(l => l.inst === inst)) l.reject('disabled') },
  }
}
async function setup(opts) {
  const host = scriptedHost(opts); const results = []; const statuses = []
  const coord = await createCoordinator(COORD)
  const c = createLocalCompute({ host, coord, onResult: (id, m, o, want, meta) => results.push({ id, kind: meta.kind, op: meta.op, rev: want.revision, times: o.times }), onStatus: (id, s) => statuses.push([id, s.state, s.code]) })
  return { host, c, results, statuses }
}
const bars = n => Array.from({ length: n }, (_, i) => wire((i + 1) * 60))
const answer = p => p.resolve(out(p.op === 1 ? p.body.bars.slice(p.body.output_from).map(b => b.time) : p.op === 2 ? [p.body.bars.at(-1).time] : p.body.ops.map(o => o.bar.time)))

test('live plugin: History once, then LiveInit (op2) and bounded LiveUpdate (op3) on ordinary ticks', async () => {
  const { host, c, results } = await setup()
  let cache = bars(300)
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: cache }); await tick()
  assert.equal(host.pending[0].op, 1); answer(host.pending.shift()); await tick()
  const init = host.pending.shift(); assert.equal(init.op, 2, 'LiveInit after History')
  assert.equal(init.body.bars.length, 2 + 1 + 64, 'bounded SDK window, not whole cache')
  answer(init); await tick()
  // ordinary tick: replace forming bar + append => one op3 with exactly 2 ops
  cache = [...cache.slice(0, -1), wire(300 * 60, 9), wire(301 * 60)]
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 1, bars: cache }); await tick()
  const upd = host.pending.shift()
  assert.equal(upd.op, 3); assert.equal(upd.body.seq, 1); assert.equal(upd.body.revision, init.body.revision, 'session revision fixed')
  assert.deepEqual(upd.body.ops.map(o => o.op), ['replace', 'append'])
  answer(upd); await tick()
  assert.equal(host.pending.length, 0, 'no History recompute on ticks')
  assert.deepEqual(results.map(r => r.kind), ['history', 'live', 'live'])
  assert.deepEqual(c.ops('a'), { 2: 1, 3: 1 })
  // rejected update (sequence_gap) => explicit LiveInit reinit, never History
  cache = [...cache, wire(302 * 60)]
  c.setDemand({ scope, scopeKey: 's', revision: 3, historyRevision: 1, bars: cache }); await tick()
  host.pending.shift().reject(Object.assign(new Error(), { code: 'sequence_gap' })); await tick()
  assert.equal(host.pending[0].op, 2, 'reinit with LiveInit'); answer(host.pending.shift()); await tick()
  assert.equal(c.fault('a'), null)
})

test('prepend recomputes stateless History without resetting the live session', async () => {
  const { host, c } = await setup()
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(100).slice(50) }); await tick()
  answer(host.pending.shift()); await tick(); answer(host.pending.shift()); await tick()
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 2, bars: bars(100) }); await tick()
  assert.equal(host.pending[0].op, 1); answer(host.pending.shift()); await tick()
  assert.equal(host.pending.length, 0, 'tail unchanged: no LiveInit, session preserved')
  assert.equal(host.calls.filter(x => x[0] === 'load').length, 1)
})

test('disable while loading physically terminates the pending Worker; no restart', async () => {
  const { host, c } = await setup({ loadGate: true })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(10) }); await tick()
  assert.equal(host.loads.length, 1)
  c.setPlugins([]); await tick()
  assert.ok(host.calls.some(x => x[0] === 'disable'), 'disable reached the host while load pending')
  assert.equal(host.instances.size, 0); assert.equal(host.calls.filter(x => x[0] === 'load').length, 1)
  assert.deepEqual(c.running(), [])
})

test('old load failure after replacement cannot fault the new version', async () => {
  const { host, c } = await setup({ loadGate: true })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'v1' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(10) }); await tick()
  const old = host.loads.shift()
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: { p: 1 }, loadKey: 'v2' }]); await tick()
  old.reject('trap'); await tick()
  assert.equal(c.fault('a'), null, 'late old error ignored')
  host.loads.shift().resolve(); await tick()
  assert.equal(host.pending[0]?.op, 1, 'new version computing')
})

test('retry after fault: clearFault + setPlugins reloads exactly the new params once', async () => {
  const { host, c } = await setup()
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'v1' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(10) }); await tick()
  host.pending.shift().reject(Object.assign(new Error(), { code: 'trap' })); await tick()
  assert.equal(c.fault('a').code, 'trap'); const loads = host.calls.filter(x => x[0] === 'load').length
  await tick(); assert.equal(host.calls.filter(x => x[0] === 'load').length, loads, 'no autorestart')
  c.clearFault('a'); await tick()
  assert.equal(host.calls.filter(x => x[0] === 'load').length, loads, 'clearFault alone does not restart obsolete version')
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: { x: 2 }, loadKey: 'v2' }]); await tick()
  assert.equal(host.calls.filter(x => x[0] === 'load').length, loads + 1)
})

test('slow peer does not block a healthy plugin from processing later ticks', async () => {
  const { host, c, results } = await setup({ live: false })
  c.setPlugins([{ id: 'slow', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }, { id: 'fast', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(10) }); await tick()
  const slowInst = c.runtime('slow').instance
  for (let r = 2; r <= 4; r++) {
    const f = host.pending.find(p => p.inst !== slowInst); host.pending.splice(host.pending.indexOf(f), 1); answer(f); await tick()
    c.setDemand({ scope, scopeKey: 's', revision: r, historyRevision: r, bars: bars(10 + r) }); await tick()
  }
  assert.ok(results.filter(r => r.id === 'fast').length >= 3, 'fast progressed while slow stuck')
  assert.equal(results.filter(r => r.id === 'slow').length, 0)
})

test('quiet feed: latest demand completes without another data change (no round cap stranding)', async () => {
  const { host, c, results } = await setup({ live: false })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  for (let r = 1; r <= 30; r++) c.setDemand({ scope, scopeKey: 's', revision: r, historyRevision: r, bars: bars(10 + r) })
  for (let i = 0; i < 40 && !c.idle(); i++) { if (host.pending.length) answer(host.pending.shift()); await tick() }
  assert.equal(results.at(-1).rev, 30); assert.ok(c.idle())
})

test('permanently rejected live basis: bounded reinit, then stalled until genuinely new input (no spin)', async () => {
  const { host, c, statuses } = await setup()
  let cache = bars(300)
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: cache }); await tick()
  answer(host.pending.shift()); await tick()
  // every live attempt on this unchanged basis is rejected
  let attempts = 0
  for (let i = 0; i < 20 && host.pending.length; i++) { host.pending.shift().reject(Object.assign(new Error(), { code: 'invalid_input' })); attempts++; await tick() }
  assert.equal(host.pending.length, 0, 'no further live attempts on unchanged basis')
  assert.ok(attempts <= 3, `bounded attempts, got ${attempts}`)
  assert.ok(c.idle(), 'quiet invalid source does not keep running')
  assert.deepEqual(statuses.at(-1).slice(1), ['stalled', 'invalid_input'])
  assert.equal(c.fault('a'), null, 'not a plugin fault')
  // genuinely new input retries once more
  cache = [...cache, wire(301 * 60)]
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 1, bars: cache }); await tick()
  assert.equal(host.pending.length, 1, 'new input re-attempts')
  assert.equal(host.pending[0].op, 2)
})

test('delayed History on an older basis (prepend/correction mid-compute) is discarded, never painted', async () => {
  const { host, c, results } = await setup({ live: false })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(20) }); await tick()
  const old = host.pending.shift()
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 2, bars: bars(40) }); await tick()
  answer(old); await tick()
  assert.equal(results.filter(r => r.rev === 1).length, 0, 'older-basis output discarded')
  for (let i = 0; i < 5 && host.pending.length; i++) { answer(host.pending.shift()); await tick() }
  assert.equal(results.at(-1).rev, 2)
})

test('delayed live output on an older basis is not painted', async () => {
  const { host, c, results } = await setup()
  let cache = bars(300)
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: cache }); await tick()
  answer(host.pending.shift()); await tick()
  const init = host.pending.shift(); assert.equal(init.op, 2)
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 2, bars: cache }); await tick()
  answer(init); await tick()
  assert.equal(results.filter(r => r.kind === 'live').length, 0)
})
