// Unit tests for browser-local plugin packages and the compute coordinator.
// Scheduling/fencing logic uses a scripted host; REAL WASM is exercised in
// tests/local-wasm-browser.mjs (Chromium, real modules, no mocked outputs).
import test from 'node:test'
import assert from 'node:assert/strict'
import { inputBar, historyBody, contextBars, sanitizeParams, parseParamInput, checkImportFile, classifyImport, importMessage, INPUT_FIELDS } from '../static/js/local-plugins.mjs'
import { createLocalCompute } from '../static/js/local-compute.mjs'

const wire = t => Object.fromEntries([['time', t], ...INPUT_FIELDS.map((f, i) => [f, i + t / 60]), ['trades', 3], ['quality', { x: 1 }]])

test('inputBar is an exact projection of canonical wire fields (cumulative CVD passed through), fails closed on gaps', () => {
  const b = wire(120); b.cvd_usdt = -12345.5; b.ac = 0
  const i = inputBar(b)
  assert.equal(i.cvd_usdt, -12345.5); assert.equal(i.ac, 0, 'true zero preserved')
  assert.equal(i.quality, undefined); assert.equal(Object.keys(i).length, INPUT_FIELDS.length + 2)
  assert.throws(() => inputBar({ ...b, ad: null }), e => e.code === 'invalid_input')
  assert.throws(() => inputBar({ ...b, trades: -1 }), e => e.code === 'invalid_input')
})

test('historyBody: exact warm-up via output_from from descriptor context (per-tf override)', () => {
  const d = { context: { bars: 3, per_tf: { '1h': 1 } } }
  const bars = [60, 120, 180, 240, 300].map(wire)
  const scope = { exchange: 'binance', market: 'spot', symbol: 'BTC', tf: '1m', mode: 'source' }
  const { body, outputFrom } = historyBody(bars, scope, 7, d)
  assert.equal(outputFrom, 3); const parsed = JSON.parse(body)
  assert.equal(parsed.revision, 7); assert.equal(parsed.bars.length, 5); assert.deepEqual(parsed.scope, scope)
  assert.equal(contextBars(d, '1h'), 1)
  assert.equal(historyBody(bars.slice(0, 2), scope, 1, d).outputFrom, 2, 'short cache: all warm-up, no fabricated output')
})

test('params: declared keys/ranges only; form parsing rejects out-of-range', () => {
  const d = { params: [{ key: 'n', type: 'int', default: 5, min: 1, max: 10 }, { key: 'm', type: 'choice', default: 'a', options: ['a', 'b'] }, { key: 'f', type: 'bool', default: false }] }
  assert.deepEqual(sanitizeParams(d, { n: 99, m: 'b', evil: 1, f: true }), { n: 5, m: 'b', f: true })
  assert.throws(() => parseParamInput(d.params[0], '11'), e => e.code === 'invalid_params')
  assert.equal(parseParamInput(d.params[0], '7'), 7)
})

test('import file checks and duplicate/version decisions', () => {
  assert.throws(() => checkImportFile({ name: 'x.js', size: 10 }), e => e.code === 'bad_file_type')
  assert.throws(() => checkImportFile({ name: 'x.wasm', size: 0 }), e => e.code === 'bad_magic')
  assert.throws(() => checkImportFile({ name: 'x.wasm', size: (16 << 20) + 1 }), e => e.code === 'module_too_large')
  const recs = [{ id: 'a', sha256: '1' }]
  assert.equal(classifyImport(recs, { id: 'a', sha256: '1' }).action, 'same')
  assert.equal(classifyImport(recs, { id: 'a', sha256: '2' }).action, 'replace')
  assert.equal(classifyImport(recs, { id: 'b', sha256: '2' }).action, 'add')
  assert.match(importMessage({ code: 'forbidden_import' }).text, /imports host functions/)
})

function scriptedHost() {
  const calls = []; const pending = []
  return {
    calls, pending,
    load: async (inst) => { calls.push(['load', inst]); return { descriptor: { context: { bars: 0 } } } },
    compute: (inst, scopeKey, rev) => new Promise((resolve, reject) => { calls.push(['compute', inst, scopeKey, rev]); pending.push({ resolve, reject, inst, scopeKey, rev }) }),
    supersede: (inst) => { calls.push(['supersede', inst]); const n = pending.filter(p => p.inst === inst).length; for (const p of pending.filter(p => p.inst === inst)) p.reject(Object.assign(new Error(), { code: 'superseded' })); pending.splice(0, pending.length, ...pending.filter(p => p.inst !== inst)); return n },
    disable: (inst) => { calls.push(['disable', inst]) },
  }
}
const tick = () => new Promise(r => setTimeout(r, 0))
const out = t => JSON.stringify({ times: [t], series: {} })

// Scheduling tests moved to tests/local-compute.test.mjs (real Rust coordinator wasm).

test('content identity works without crypto.subtle (plain-http LAN) and same requires equal bytes', async () => {
  const { sha256Hex, sameBytes } = await import('../static/js/local-plugins.mjs')
  const a = new Uint8Array([0, 97, 115, 109, 1]).buffer, b = new Uint8Array([0, 97, 115, 109, 2]).buffer
  const saved = Object.getOwnPropertyDescriptor(globalThis, 'crypto')
  Object.defineProperty(globalThis, 'crypto', { value: {}, configurable: true })
  try {
    const ha = await sha256Hex(a), hb = await sha256Hex(b)
    assert.match(ha, /^fnv64:[0-9a-f]{16}:5$/); assert.notEqual(ha, hb)
  } finally { Object.defineProperty(globalThis, 'crypto', saved) }
  assert.match(await sha256Hex(a), /^[0-9a-f]{64}$/)
  assert.equal(sameBytes(a, a.slice(0)), true); assert.equal(sameBytes(a, b), false)
})

import { fnv64Hex, validRecord } from '../static/js/local-plugins.mjs'

test('plain-HTTP fallback hash is bit-exact FNV-1a 64 and yields between bounded chunks', async () => {
  const ref = bytes => { let h = 0xcbf29ce484222325n; for (const b of bytes) h = BigInt.asUintN(64, (h ^ BigInt(b)) * 0x100000001b3n); return `fnv64:${h.toString(16).padStart(16, '0')}:${bytes.length}` }
  const bytes = new Uint8Array(5000); for (let i = 0; i < bytes.length; i++) bytes[i] = (i * 131 + 7) & 0xff
  let yields = 0
  const got = await fnv64Hex(bytes.buffer, { chunk: 1024, yieldFn: async () => { yields++ } })
  assert.equal(got, ref(bytes)); assert.equal(yields, 4, 'yields between every chunk')
  assert.equal(await fnv64Hex(new ArrayBuffer(0)), ref(new Uint8Array(0)))
  const ff = new Uint8Array(300).fill(0xff); assert.equal(await fnv64Hex(ff.buffer), ref(ff), 'carry path')
})

test('restored records: shape/identity validated, corrupted or foreign records rejected', () => {
  const good = { id: 'p', bytes: new ArrayBuffer(8), size: 8, descriptor: { id: 'p' }, sha256: 'x', enabled: false, importedAt: 1, params: {} }
  assert.equal(validRecord(good), true)
  for (const bad of [null, { ...good, id: '' }, { ...good, bytes: 'b' }, { ...good, size: 9 }, { ...good, descriptor: { id: 'other' } },
    { ...good, sha256: undefined }, { ...good, enabled: 'yes' }, { ...good, importedAt: NaN }, { ...good, bytes: new ArrayBuffer(0) }])
    assert.equal(validRecord(bad), false, JSON.stringify(bad))
  assert.equal(importMessage({ code: 'replace_params' }).text.includes('previous version kept'), true)
})
