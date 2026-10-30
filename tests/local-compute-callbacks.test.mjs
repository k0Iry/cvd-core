// Driver boundary: a throwing app callback (onStatus / onResult / onInvalid)
// must not strand an installed inflight token, spin a pending loop, or leave a
// ready display with unpublished output. It is a presentation failure, never a
// WASM trap. Real driver (static/wasm-host/host-bridge.js) + real Rust coordinator.
// LC_MODULE_DIR may point at another driver copy (pre-fix RED evidence).
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { pathToFileURL } from 'node:url'

const DIR = process.env.LC_MODULE_DIR ? pathToFileURL(path.resolve(process.env.LC_MODULE_DIR) + '/').href : new URL('../static/wasm-host/', import.meta.url).href
const { createLocalCompute, createCoordinator } = await import(DIR + 'host-bridge.js')
const COORD = fs.readFileSync(process.env.CVD_COORDINATOR_WASM || new URL('../static/wasm-host/cvd_browser_coordinator.wasm', import.meta.url))
const F = ['po', 'ph', 'pl', 'price_close', 'apo', 'aph', 'apl', 'apc', 'cvd_usdt', 'ac', 'delta_usdt', 'ad', 'bvb', 'abvb', 'svb', 'asvb', 'buy_volume_usdt', 'agb', 'sell_volume_usdt', 'ags']
const wire = t => Object.fromEntries([['time', t], ...F.map((f, i) => [f, i + t / 60]), ['trades', 3]])
const bars = n => Array.from({ length: n }, (_, i) => wire((i + 1) * 60))
const scope = { exchange: 'binance', market: 'spot', symbol: 'BTCUSDT', tf: '1m', mode: 'source' }
const tick = (n = 6) => new Promise(r => { let i = 0; const f = () => ++i >= n ? r() : setTimeout(f, 0); f() })

async function setup(throwOn) {
  const calls = [], logs = [], statuses = [], results = []
  const host = {
    load: async () => { calls.push('load'); return { descriptor: { live: false, context: { bars: 2 } } } },
    compute: async (inst, sk, rev, op, body) => { calls.push('compute'); const b = JSON.parse(body); return JSON.stringify({ times: b.bars.slice(b.output_from).map(x => x.time), series: { x: { type: 'line', values: b.bars.slice(b.output_from).map(() => 1) } } }) },
    supersede: () => 0, disable: () => calls.push('disable'),
  }
  const coord = await createCoordinator(COORD)
  const c = createLocalCompute({ host, coord,
    onStatus: (id, s) => { statuses.push([s.state, s.code]); if (throwOn(s)) throw new Error('chart status failed') },
    onResult: (id, m, out) => { results.push(out); if (throwOn('result')) throw new Error('chart paint failed') },
    log: (...a) => logs.push(a.map(String).join(' ')) })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(50), wantFromIndex: 10 })
  await tick(20)
  return { c, calls, logs, statuses, results }
}

test('onStatus throws after next installed an inflight token: bounded presentation fault, not stuck, not a trap', async () => {
  const { c, logs, statuses } = await setup(s => s.state === 'computing')
  const rt = c.runtime('a')
  assert.equal(rt.inflight, false, 'no stranded inflight token')
  assert.equal(c.idle(), true, 'no endless pending loop')
  assert.equal(rt.fault?.code, 'presentation_error')
  assert.ok(!statuses.some(([s, code]) => code === 'trap'), 'never mislabeled as a WASM trap')
  assert.ok(logs.some(l => l.includes('presentation error')))
})

test('onResult throws: plugin is not left ready with unpublished output; values withdrawn', async () => {
  const { c, statuses } = await setup(s => s === 'result')
  assert.notDeepEqual(statuses.at(-1), ['ready', ''], 'no ready display for an unpublished result')
  assert.equal(c.runtime('a').fault?.code, 'presentation_error')
  assert.equal(c.idle(), true)
})

test('control: no callback failure reaches ready with the published result', async () => {
  const { c, statuses, results } = await setup(() => false)
  assert.deepEqual(statuses.at(-1), ['ready', ''])
  assert.equal(results.length, 1)
  assert.equal(c.runtime('a').fault, null)
})

test('P1 at the compiled-WASM bridge: a late trap from an obsolete history basis reloads and the quiet new basis completes; current-basis trap still faults', async () => {
  const calls = []; const gates = []
  const host = {
    load: async () => { calls.push('load'); return { descriptor: { live: false, context: { bars: 2 } } } },
    compute: (inst, sk, rev) => new Promise((resolve, reject) => { calls.push(['compute', rev]); gates.push({ rev, resolve, reject }) }),
    supersede: () => 0, disable: () => calls.push('disable'),
  }
  const statuses = []
  const coord = await createCoordinator(COORD)
  const c = createLocalCompute({ host, coord, onStatus: (id, s) => statuses.push([s.state, s.code]), onResult: () => {} })
  c.setPlugins([{ id: 'a', bytes: new ArrayBuffer(1), params: {}, loadKey: 'k' }])
  c.setDemand({ scope, scopeKey: 's', revision: 1, historyRevision: 1, bars: bars(50), wantFromIndex: 10 }); await tick()
  assert.equal(gates.length, 1)
  c.setDemand({ scope, scopeKey: 's', revision: 2, historyRevision: 2, bars: bars(60), wantFromIndex: 10 }); await tick()
  gates.shift().reject(Object.assign(new Error('trap'), { code: 'trap' })); await tick(20)
  assert.equal(c.runtime('a').fault, null, 'obsolete-basis failure has no authority')
  assert.equal(calls.filter(x => x === 'load').length, 2, 'dead Worker reloaded, not assumed loaded')
  assert.equal(gates.at(-1)?.rev, 2, 'new basis computed without another tick')
  const g = gates.shift()
  g.reject(Object.assign(new Error('trap'), { code: 'trap' })); await tick(20)
  assert.equal(c.runtime('a').fault?.code, 'trap', 'current-basis failure still faults')
})
