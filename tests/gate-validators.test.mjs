// Controls for the two gate validators (no build/network/browser/DB):
//  - core-app-gate.sh check_inventory (shared by default-12 and coverage-2)
//  - b-browser-gate.sh stage selector (unknown/whitespace must fail pre-run)
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const tmp = (p) => fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), p))
const WANT12 = 'cached_drag,cached_zoom,crossing_buffer,crossing_newer,hidden_resize_resume,history_prepend_delayed,indicator_menu,long_drag,pane_contract,realtime_while_history,source_reset_new_grid,wide_zoom_return'
const WANT2 = 'tf_roundtrip_pending,warmup_exposed_drag'
// Pins the gate's own lists so a drift in either is caught here too.
const gateSrc = fs.readFileSync(path.join(ROOT, 'scripts/core-app-gate.sh'), 'utf8')
test('core-app-gate pins the 12 + 2 inventories', () => {
  assert.ok(gateSrc.includes(`\nwant=${WANT12}\n`))
  assert.ok(gateSrc.includes(`\ncovwant=${WANT2}\n`))
})

function check(label, want, results) {
  const d = tmp('cvd-inv-'); const f = path.join(d, 'r.json')
  try {
    fs.writeFileSync(f, JSON.stringify(results))
    return spawnSync('bash', [path.join(ROOT, 'scripts/core-app-gate.sh'), '--check-inventory', label, f, want], { encoding: 'utf8' })
  } finally { fs.rmSync(d, { recursive: true, force: true }) }
}
const green = (want) => ({ scenarios: Object.fromEntries(want.split(',').map(k => [k, { all_ok: true }])), errors: [], cleanup: { ok: true } })

for (const [label, want] of [['scenario', WANT12], ['coverage', WANT2]]) {
  test(`${label} inventory: green fixture passes`, () => {
    const r = check(label, want, green(want)); assert.equal(r.status, 0, r.stderr)
  })
  const names = want.split(',')
  const cases = {
    omitted: (g) => { delete g.scenarios[names[0]] },
    misnamed: (g) => { delete g.scenarios[names[0]]; g.scenarios[names[0] + '_x'] = { all_ok: true } },
    extra: (g) => { g.scenarios.zz_extra = { all_ok: true } },
    all_ok_false: (g) => { g.scenarios[names[1]].all_ok = false },
    errors_nonempty: (g) => { g.errors = ['boom'] },
    errors_missing: (g) => { delete g.errors },
    cleanup_false: (g) => { g.cleanup.ok = false },
    cleanup_missing: (g) => { delete g.cleanup },
  }
  for (const [k, mut] of Object.entries(cases)) {
    test(`${label} inventory: ${k} is nonzero`, () => {
      const g = green(want); mut(g); const r = check(label, want, g)
      assert.notEqual(r.status, 0); assert.match(r.stderr, /FAIL/)
    })
  }
  test(`${label} inventory: empty want is nonzero`, () => {
    assert.notEqual(check(label, '', green(want)).status, 0)
  })
}

// Selector control: owned sandbox copy of the gate; per-child PATH stubs make
// any stage that does start fail fast (rc 97) instead of building/running.
function selector(stages) {
  const sb = tmp('cvd-sel-')
  try {
    fs.mkdirSync(path.join(sb, 'src/scripts'), { recursive: true }); fs.mkdirSync(path.join(sb, 'nobin'))
    for (const f of ['b-browser-gate.sh', 'owned-group.sh']) fs.copyFileSync(path.join(ROOT, 'scripts', f), path.join(sb, 'src/scripts', f))
    for (const t of ['cargo', 'node', 'timeout']) { const p = path.join(sb, 'nobin', t); fs.writeFileSync(p, '#!/bin/sh\nexit 97\n'); fs.chmodSync(p, 0o755) }
    const env = { ...process.env, PATH: `${path.join(sb, 'nobin')}:${process.env.PATH}`, B_GATE_OUT: path.join(sb, 'out') }
    delete env.B_GATE_STAGES; delete env.B_GATE_INJECT_FAIL
    if (stages !== undefined) env.B_GATE_STAGES = stages
    const r = spawnSync('bash', [path.join(sb, 'src/scripts/b-browser-gate.sh'), path.join(sb, 'aud')], { encoding: 'utf8', env, timeout: 30000 })
    const codes = path.join(sb, 'out/exit-codes.txt')
    const names = fs.existsSync(codes) ? fs.readFileSync(codes, 'utf8').split('\n').filter(Boolean).map(l => l.split('=')[0]) : []
    const logs = fs.existsSync(path.join(sb, 'out')) ? fs.readdirSync(path.join(sb, 'out')).filter(f => f.endsWith('.log')) : []
    return { status: r.status, stderr: r.stderr, names, logs }
  } finally { fs.rmSync(sb, { recursive: true, force: true }) }
}
const DEFAULT = ['coordinator', 'node', 'lwcrender', 'local', 'data', 'p2', 'toggle', 'interaction']
for (const s of ['bogus', 'node bogus', 'bogus node', '   ', '\t\n', '*']) {
  test(`b-browser-gate rejects stages=${JSON.stringify(s)} before any stage`, () => {
    const r = selector(s)
    assert.equal(r.status, 2, r.stderr); assert.match(r.stderr, /FAIL B_GATE_STAGES/)
    assert.deepEqual(r.logs, []); assert.ok(!r.names.includes('overall'))
  })
}
for (const [label, s] of [['unset', undefined], ['empty', '']]) {
  test(`b-browser-gate ${label} B_GATE_STAGES keeps the default list`, () => {
    const r = selector(s)
    assert.deepEqual(r.names, [...DEFAULT, 'overall']); assert.equal(r.status, 1)
  })
}
test('b-browser-gate valid single stage runs only that stage', () => {
  const r = selector(' cleanup ')
  assert.deepEqual(r.names, ['cleanup', 'overall']); assert.equal(r.status, 1)
})
for (const [label, s] of [['tab', 'node\tlocal'], ['newline', 'node\nlocal'], ['mixed', ' \tnode \n\t local\n']]) {
  test(`b-browser-gate ${label}-separated valid stages execute (stub failures witnessed)`, () => {
    const r = selector(s)
    assert.deepEqual(r.names, ['node', 'local', 'overall'], r.stderr); assert.equal(r.status, 1)
  })
}
