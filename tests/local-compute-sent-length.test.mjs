// Bars transport of setDemand (static/wasm-host/host-bridge.js) through the
// REAL Rust coordinator. Oracle = the Rust mirror itself, observed through
// the History body it sends a scripted non-live plugin (op1 bars), compared
// with the caller's canonical array; never a copied model of the delta.
// The plugin host is scripted (deterministic scheduling/failures only).
// The app-level cases run the real app.js mutation paths (applyTailBars,
// setBars, applyRealtimeBase, setMergedBars, removeCacheTimes) in the shared
// vm harness with this bridge + coordinator installed as `localCompute`.
// LC_MODULE_DIR may point at another host-bridge.js (baseline equivalence).
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { createRequire } from 'node:module'
import { pathToFileURL } from 'node:url'

const DIR = process.env.LC_MODULE_DIR ? pathToFileURL(path.resolve(process.env.LC_MODULE_DIR) + '/').href : new URL('../static/wasm-host/', import.meta.url).href
const { createLocalCompute, createCoordinator } = await import(DIR + 'host-bridge.js')
const COORD = fs.readFileSync(process.env.CVD_COORDINATOR_WASM || new URL('../static/wasm-host/cvd_browser_coordinator.wasm', import.meta.url))
const { harness, bar: rawAppBar } = createRequire(import.meta.url)('./app-harness.cjs')
const F = ['po', 'ph', 'pl', 'price_close', 'apo', 'aph', 'apl', 'apc', 'cvd_usdt', 'ac', 'delta_usdt', 'ad', 'bvb', 'abvb', 'svb', 'asvb', 'buy_volume_usdt', 'agb', 'sell_volume_usdt', 'ags']
const wire = (t, k = 0) => Object.fromEntries([['time', t], ...F.map((f, i) => [f, i + t / 60 + k]), ['trades', 3]])
const appBar = (t, p = 100) => ({ ...wire(t), ...rawAppBar(t, p) })
const bars = n => Array.from({ length: n }, (_, i) => wire((i + 1) * 60))
const scope = { exchange: 'binance', market: 'spot', symbol: 'BTCUSDT', tf: '1m', mode: 'source' }
const tick = (n = 8) => new Promise(r => { let i = 0; const f = () => ++i >= n ? r() : setTimeout(f, 0); f() })
const KEYS = ['time', ...F, 'trades']
const norm = v => JSON.parse(JSON.stringify(v, (k, x) => k === '' || /^\d+$/.test(k) || KEYS.includes(k) ? x : undefined))

// Non-live plugin, no context: every accepted demand revision recomputes
// History from the mirror, so the op1 body exposes exactly what Rust holds.
async function setup({ failDemand = null } = {}) {
  const bodies = [], demands = [], results = []
  const host = {
    load: async () => ({ descriptor: { live: false, context: { bars: 0 } } }),
    compute: async (inst, sk, rev, op, body) => { const b = JSON.parse(body); bodies.push({ op, rev, bars: b.bars }); return JSON.stringify({ times: b.bars.slice(b.output_from).map(x => x.time), series: {} }) },
    supersede: () => 0, disable: () => {},
  }
  const coord = await createCoordinator(COORD)
  const demand = coord.demand
  coord.demand = d => {
    demands.push(d.bars ? { full: d.bars.length } : { tail: d.tail.length, len: d.len })
    if (failDemand?.(d)) throw Object.assign(new Error('injected demand failure'), { code: 'injected' })   // MOCKED failure
    return demand(d)
  }
  const c = createLocalCompute({ host, coord, onResult: (id, m, out, want) => results.push({ rev: want.revision, times: out.times }) })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  return { c, bodies, demands, results, coord }
}
// Latest History body for `rev` must equal the canonical array (whole cache: wantFromIndex 0).
function mirror(s, rev) { const b = s.bodies.filter(x => x.op === 1 && x.rev === rev).at(-1); assert.ok(b, `History for rev ${rev}: ${JSON.stringify(s.bodies.map(x => [x.op, x.rev, x.bars?.length]))} ${JSON.stringify(s.demands)}`); return b.bars }
function assertMirror(s, rev, cache) { assert.deepEqual(mirror(s, rev), norm(cache), `Rust mirror at rev ${rev}`) }

let rev = 0
const send = async (s, cache, opts = {}) => { rev += 1; s.c.setDemand({ scope, scopeKey: opts.scopeKey ?? 's', revision: rev, historyRevision: opts.h ?? 1, bars: cache, exhausted: true, stablePrefix: opts.stable ?? true }); await tick(30); return rev }

test('cases 1-4,6: tail replace, append, replace+multi-append, skipped publication and no-op republish keep the exact mirror', async () => {
  const s = await setup(); const cache = bars(200)
  assertMirror(s, await send(s, cache), cache)
  cache[cache.length - 1] = wire(200 * 60, 7)                          // 1 same-time replacement
  assertMirror(s, await send(s, cache), cache)
  cache.push(wire(201 * 60))                                           // 2 append
  assertMirror(s, await send(s, cache), cache)
  cache[cache.length - 1] = wire(201 * 60, 3); cache.push(wire(202 * 60), wire(203 * 60), wire(204 * 60))   // 3 batch
  assertMirror(s, await send(s, cache), cache)
  cache[cache.length - 1] = wire(204 * 60, 5); cache.push(wire(205 * 60))   // 4 skipped publication...
  cache[cache.length - 1] = wire(205 * 60, 6); cache.push(wire(206 * 60))   // ...then a later one
  assertMirror(s, await send(s, cache), cache)
  assertMirror(s, await send(s, cache), cache)                         // 6 no-op republish (warm-up promotion shape)
  if (!process.env.LC_MODULE_DIR) {
    // Structural (candidate only): suffix from the prior accepted last row, never full, no identity scan.
    assert.deepEqual(s.demands, [{ full: 200 }, { tail: 1, len: 200 }, { tail: 2, len: 201 }, { tail: 4, len: 204 }, { tail: 3, len: 206 }, { tail: 1, len: 206 }])
  }
})

test('cases 7,9-11: no opt-in, new basis, shrink, scope change and a new bridge send the full cache', async () => {
  const s = await setup(); let cache = bars(100)
  await send(s, cache)
  cache[10] = wire(11 * 60, 9)                                         // 7 interior change without opt-in
  assertMirror(s, await send(s, cache, { stable: false }), cache)
  cache = [wire(30), ...cache]                                          // 9 prepend under a new history basis
  assertMirror(s, await send(s, cache, { h: 2 }), cache)
  cache = cache.slice(0, 50)                                           // 10 shrink (same basis, contract broken by caller)
  assertMirror(s, await send(s, cache, { h: 2 }), cache)
  assertMirror(s, await send(s, cache, { h: 2, scopeKey: 's2' }), cache)   // 11 scope change
  const fresh = await setup(); assertMirror(fresh, await send(fresh, cache, { h: 2, scopeKey: 's2' }), cache)   // 11 new bridge
  if (!process.env.LC_MODULE_DIR) {
    assert.deepEqual(s.demands.map(d => 'full' in d ? 'full' : 'tail'), ['full', 'full', 'full', 'full', 'full'])
    assert.deepEqual(fresh.demands, [{ full: 50 }])
  }
})

test('case 8 negative control: an opted-in caller violating the prefix contract is NOT detected (documented limit)', { skip: !!process.env.LC_MODULE_DIR && 'baseline identity scan sends the interior row' }, async () => {
  const s = await setup(); const cache = bars(60)
  await send(s, cache)
  cache[5] = wire(6 * 60, 9); cache.push(wire(61 * 60))                // interior edit under stablePrefix (violation)
  const r = await send(s, cache)
  assert.notDeepEqual(mirror(s, r)[5], norm(cache[5]), 'stale interior row: the contract is the caller\'s, not guarded')
  assert.deepEqual(mirror(s, r).at(-1), norm(cache.at(-1)))
})

test('case 12: need_full (non-contiguous tail) falls back to one successful full demand', async () => {
  const s = await setup(); const cache = bars(30)
  await send(s, cache)
  // Accepted len 30, but the caller's array grew by a gap-producing jump: Rust
  // still applies the conservative tail; force need_full via a length mismatch
  // by sending a demand whose basis matches but rows were dropped then re-added.
  const other = [...cache.slice(0, 29), wire(40 * 60), wire(41 * 60)]  // replaced last row with a later time + append
  const r = await send(s, other)
  assertMirror(s, r, other)
})

test('cases 13-14: a failed demand forces full next time; acceptance is recorded only after Rust accepted', async () => {
  let fail = false
  const s = await setup({ failDemand: () => fail })
  const cache = bars(40)
  await send(s, cache)
  cache[cache.length - 1] = wire(40 * 60, 2); cache.push(wire(41 * 60))
  fail = true
  assert.throws(() => s.c.setDemand({ scope, scopeKey: 's', revision: ++rev, historyRevision: 1, bars: cache, exhausted: true, stablePrefix: true }), /injected/)
  fail = false
  cache.push(wire(42 * 60))
  const r = await send(s, cache)
  assertMirror(s, r, cache)
  if (!process.env.LC_MODULE_DIR) assert.ok('full' in s.demands.at(-1), 'after an uncertain demand the next delivery is full')
})

test('case 13: tail need_full then failing full leaves the mirror uncertain; next delivery is full and exact', async () => {
  let failFull = false
  const s = await setup({ failDemand: d => failFull && !!d.bars })
  const cache = bars(40)
  await send(s, cache)
  const reshaped = [...cache.slice(0, 39), wire(50 * 60), wire(51 * 60)]   // tail may partially mutate the mirror
  failFull = true
  try { s.c.setDemand({ scope, scopeKey: 's', revision: ++rev, historyRevision: 1, bars: reshaped, exhausted: true, stablePrefix: true }) } catch (e) { assert.match(String(e), /injected/) }
  failFull = false
  reshaped.push(wire(52 * 60))
  assertMirror(s, await send(s, reshaped), reshaped)
})

test('case 13b: full fallback answered need_full (non-array bars, normal return) is NOT a checkpoint; next delivery is full', async () => {
  const s = await setup(); const cache = bars(30)
  await send(s, cache)
  s.c.setDemand({ scope, scopeKey: 's', revision: ++rev, historyRevision: 1, bars: { length: 31, slice: () => [] }, exhausted: true, stablePrefix: true })   // malformed boundary input
  cache.push(wire(31 * 60), wire(32 * 60))
  assertMirror(s, await send(s, cache), cache)
  if (!process.env.LC_MODULE_DIR) assert.equal(Object.keys(s.demands.at(-1))[0], 'full', 'unaccepted reply forces full next time')
})

test('case 14: a presentation (onView) throw AFTER Rust accepted keeps the accepted checkpoint', async () => {
  const cache = bars(30)
  let boom = true
  // Dedicated bridge whose onView throws on the scope-change clear (values exist after the first History).
  const bodies = [], demands = []
  const coord = await createCoordinator(COORD); const demand = coord.demand
  coord.demand = d => { demands.push(d.bars ? 'full' : 'tail'); return demand(d) }
  const host = { load: async () => ({ descriptor: { live: false, context: { bars: 0 } } }),
    compute: async (i, sk, r, op, body) => { const b = JSON.parse(body); bodies.push({ op, rev: r, bars: b.bars }); const ts = b.bars.slice(b.output_from).map(x => x.time); return JSON.stringify({ times: ts, series: { x: { type: 'line', values: ts.map(() => 1) } } }) },
    supersede: () => 0, disable: () => {} }
  const c = createLocalCompute({ host, coord, onResult: () => {}, onView: (id, v) => { if (id === null && boom) throw new Error('view boom') } })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  const t = { bodies, demands }
  c.setDemand({ scope, scopeKey: 's', revision: ++rev, historyRevision: 1, bars: cache, exhausted: true, stablePrefix: true }); await tick(30)
  assert.throws(() => c.setDemand({ scope, scopeKey: 's2', revision: ++rev, historyRevision: 1, bars: cache, exhausted: true, stablePrefix: true }), /view boom/)
  boom = false; await tick(30)
  cache.push(wire(31 * 60))
  c.setDemand({ scope, scopeKey: 's2', revision: ++rev, historyRevision: 1, bars: cache, exhausted: true, stablePrefix: true }); await tick(30)
  assertMirror(t, rev, cache)
  if (!process.env.LC_MODULE_DIR) assert.deepEqual(demands, ['full', 'full', 'tail'], 'checkpoint recorded before the presentation callback')
})

test('case 16: in-flight History of an older revision is fenced; the newest tail revision publishes the exact mirror', async () => {
  const s = await setup(); const cache = bars(80)
  rev += 1; const r0 = rev
  s.c.setDemand({ scope, scopeKey: 's', revision: r0, historyRevision: 1, bars: cache, exhausted: true, stablePrefix: true })   // no await: load/compute in flight
  cache[cache.length - 1] = wire(80 * 60, 4); cache.push(wire(81 * 60))
  const r1 = await send(s, cache)
  await tick(20)
  assertMirror(s, r1, cache)
  assert.ok(s.results.every(x => x.rev >= r0))
  assert.equal(s.results.at(-1).rev, r1)
})

// ---------------------------------------------------------------- app paths
async function appSetup() {
  const h = harness()
  const s = await setup()
  h.run('modeConnectionsReady = true; page.reachedStart = true; page.reachedEnd = true')
  h.run(`setBars(${JSON.stringify(Array.from({ length: 120 }, (_, i) => appBar((i + 1) * 60, 100 + i)))}, {from:90,to:119})`)
  h.flushFrames()
  const ctx = h.run('globalThis'); ctx.__lc = s.c
  h.run('localCompute = globalThis.__lc; noteDataChanged("history")')
  await tick()
  // History covers the app's requested window (a prepended context row may sit before it).
  const check = async () => {
    await tick(30); const r = h.run('dataRevision'), m = mirror(s, r), page = h.run('page.bars')
    assert.ok(m.length >= 120 && m.length >= page.length - 1, `window size ${m.length}/${page.length}`)
    assert.deepEqual(m, norm(page.slice(page.length - m.length)), `app mirror at rev ${r}`)
  }
  await check()
  return { h, s, check }
}

test('app cases 1-3,5,6,9 (+removal): real app mutation paths keep the Rust mirror equal to page.bars', async () => {
  const { h, s, check } = await appSetup()
  h.run(`applyTailBars([${JSON.stringify(appBar(120 * 60, 300))}])`); await check()                  // 1
  h.run(`applyTailBars([${JSON.stringify(appBar(121 * 60, 301))}])`); await check()                  // 2
  h.run(`applyTailBars([${JSON.stringify(appBar(121 * 60, 302))}, ${JSON.stringify(appBar(122 * 60))}, ${JSON.stringify(appBar(123 * 60))}])`); await check()   // 3
  // 5: render throws AFTER the tail mutation and BEFORE publication; a later append recovers it.
  h.run('globalThis.__upd = priceSeries.update; priceSeries.update = () => { throw new Error("render boom") }')
  assert.throws(() => h.run(`applyTailBars([${JSON.stringify(appBar(123 * 60, 999))}])`), /render boom/)
  h.run('priceSeries.update = globalThis.__upd')
  assert.equal(h.run('page.bars.at(-1).price_close'), 999, 'mutated but unpublished')
  h.run(`applyTailBars([${JSON.stringify(appBar(124 * 60))}])`); await check()
  assert.equal(mirror(s, h.run('dataRevision')).at(-2).price_close, 999, 'skipped replacement recovered')
  h.run('publishLocalDemand()'); await tick(); h.run('noteDataChanged("tail")'); await check()        // 6 republish shape
  // 9: older in-place correction via the real live path (bumps history basis)
  h.run(`applyRealtimeBase({ bar: ${JSON.stringify(appBar(50 * 60, 555))}, rev: 1 })`); await check()
  h.run(`setMergedBars([${JSON.stringify(appBar(10, 1))}, ...page.bars])`); await check()          // 9 prepend
  h.run(`removeCacheTimes([${70 * 60}])`); await check()                                              // removal
  h.run(`applyTailBars([${JSON.stringify(appBar(125 * 60))}])`); await check()
  if (!process.env.LC_MODULE_DIR) assert.ok(s.demands.some(d => 'tail' in d), 'app uses the opted-in suffix path')
})
