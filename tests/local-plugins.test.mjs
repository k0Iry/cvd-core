// Unit tests for browser-local plugin packages and the compute coordinator.
// Scheduling/fencing logic uses a scripted host; REAL WASM is exercised in
// tests/local-wasm-browser.mjs (Chromium, real modules, no mocked outputs).
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import { parseParamInput, checkImportFile, classifyImport, importMessage, createLocalCompute, createCoordinator } from '../static/wasm-host/host-bridge.js'

// Descriptor/params approval runs in the ACTUAL compiled coordinator wasm
// (SDK Descriptor::validate + resolve_params); JS only coerces DOM values.
const COORD = fs.readFileSync(process.env.CVD_COORDINATOR_WASM || new URL('../static/wasm-host/cvd_browser_coordinator.wasm', import.meta.url))
const desc = (extra = {}) => ({ id: 'p.x', version: '1.0.0', name: 'X', abi: 1, context: { bars: 0 },
  params: [{ key: 'n', type: 'int', default: 5, min: 0, max: 10 }, { key: 'm', type: 'choice', default: 'a', options: ['a', 'b'] },
    { key: 'f', type: 'bool', default: true }, { key: 'x', type: 'float', default: 0.5, min: 0, max: 1 }],
  outputs: [{ key: 'v', pane: 'separate', type: 'line' }], ...extra })

test('params: restore drops unknown keys and defaults each invalid field (compiled coordinator); zero/false kept', async () => {
  const c = await createCoordinator(COORD)
  assert.deepEqual(c.approve(desc(), { n: 99, m: 'b', evil: 1, f: false, x: 0 }).params, { n: 5, m: 'b', f: false, x: 0 })
  assert.deepEqual(c.approve(desc(), { n: '7', m: 'zz', f: 'true', x: 2 }).params, { n: 5, m: 'a', f: true, x: 0.5 })
  assert.deepEqual(c.approve(desc(), null).params, { n: 5, m: 'a', f: true, x: 0.5 })
  assert.deepEqual(c.approve(desc(), { n: 0 }).params.n, 0)
  const big = desc(); big.params[0].max = 2 ** 60
  assert.equal(c.approve(big, { n: 2 ** 53 }).params.n, 5, 'JS-unsafe integer never approved')
  assert.equal(c.approve(big, { n: 2 ** 53 - 1 }, true).params.n, 2 ** 53 - 1)
})

test('params: lenient fallback resolves the default through the SDK; final ints JS-safe (compiled coordinator)', async () => {
  const c = await createCoordinator(COORD)
  const set = (i, kv) => { const d = desc(); Object.assign(d.params[i], kv); return d }
  const rej = (d, p, strict) => assert.throws(() => c.approve(d, p, strict), e => e.code === 'invalid_params', JSON.stringify([d.params, p, strict]))
  for (const p of [{}, { n: 50 }, null]) for (const s of [false, true]) rej(set(0, { default: 99 }), p, s)
  rej(set(1, { default: 'zz' }), { m: 'q' }, false)
  rej(set(1, { default: 'zz' }), {}, false)
  assert.equal(c.approve(set(0, { default: 99 }), { n: 3 }).params.n, 3)
  for (const s of [false, true]) {
    rej(set(0, { default: 2 ** 53, max: 2 ** 60 }), {}, s)
    rej(set(0, { default: -(2 ** 53), min: -(2 ** 60), max: 0 }), {}, s)
  }
  // i64::MIN is not representable from JS (f64 -> invalid_descriptor); covered in Rust tests/approve.rs.
  assert.deepEqual(c.approve(desc(), { n: 0, f: false, m: 'b' }).params, { n: 0, m: 'b', f: false, x: 0.5 })
})

test('params: strict form edit = SDK resolver errors; DOM coercion only in JS', async () => {
  const c = await createCoordinator(COORD)
  const d = desc()
  assert.equal(parseParamInput(d.params[0], '7'), 7)
  assert.equal(parseParamInput(d.params[0], ''), null)
  assert.equal(parseParamInput(d.params[0], 'abc'), null)
  assert.equal(parseParamInput(d.params[2], false), false)
  assert.equal(parseParamInput(d.params[1], 'b'), 'b')
  assert.deepEqual(c.approve(d, { n: 0, f: false, m: 'b', x: 0 }, true).params, { n: 0, f: false, m: 'b', x: 0 })
  for (const bad of [{ n: 11 }, { n: 1.5 }, { n: null }, { n: 2 ** 53 }, { m: 'c' }, { x: null }, { evil: 1 }])
    assert.throws(() => c.approve(d, bad, true), e => e.code === 'invalid_params', JSON.stringify(bad))
  assert.throws(() => c.approve(d, { n: 11 }, true), /out of range/)
})

test('descriptor: corrupt/incompatible shapes fail closed in the compiled coordinator', async () => {
  const c = await createCoordinator(COORD)
  const o = { key: 'v', pane: 'separate', type: 'line' }
  const bad = [{ id: '../x' }, { abi: 2 }, { outputs: [] }, { outputs: [{ ...o, type: 'js' }] }, { outputs: [{ ...o, pane: 'top' }] },
    { outputs: [o, o] }, { outputs: [{ ...o, type: 'segmented_lines', layers: 33 }] }, { outputs: [{ ...o, type: 'histogram', readout_only: true }] },
    { outputs: [{ ...o, widths: [0] }] }, { params: [{ key: 'n', type: 'text' }] }, { context: undefined }, { version: 7 }]
  for (const extra of bad) assert.throws(() => c.approve(desc(extra), {}), e => e.code === 'invalid_descriptor', JSON.stringify(extra))
  assert.throws(() => c.approve('nope', {}), e => e.code === 'invalid_descriptor')
  assert.equal(c.approve(desc(), {}).descriptor.id, 'p.x')
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
  const { sha256Hex, sameBytes } = await import('../static/wasm-host/host-bridge.js')
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

import { fnv64Hex, validRecord } from '../static/wasm-host/host-bridge.js'

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
