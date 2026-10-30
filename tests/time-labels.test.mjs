// Display-only time labels: canonical bucket-END Bar.time -> bar OPEN label.
import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { execFileSync } from 'node:child_process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { TF_SECONDS, barOpenTime, createTimeLabelFormatters } from '../static/js/time-labels.mjs'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
// Producer contract (aggregator.rs / make_live_bar): ((id+1)*tf_ms-1)/1000.
const canonicalEnd = (openSec, tf) => Math.floor(((Math.floor(openSec / TF_SECONDS[tf]) + 1) * TF_SECONDS[tf] * 1000 - 1) / 1000)
const utc = s => Date.UTC(...s) / 1000
const TIME = 3, TIME_SEC = 4

test('every supported tf maps canonical bucket end to bucket open', () => {
    assert.deepEqual(Object.keys(TF_SECONDS), ['1m', '5m', '15m', '30m', '1h'])
    const base = utc([2026, 9, 30, 9, 0, 0])
    for (const tf of Object.keys(TF_SECONDS)) {
        for (let k = 0; k < 50; k++) {
            const open = base + k * TF_SECONDS[tf]
            assert.equal(barOpenTime(canonicalEnd(open, tf), tf), open, `${tf} ${k}`)
            assert.equal(barOpenTime(open, tf), open, 'idempotent on open time')
        }
    }
    assert.equal(barOpenTime(123, 'bogus'), 123)
})

test('09:22 forming 15m bar labels 09:15, not 09:29 (synthetic deterministic clock)', () => {
    const now = utc([2026, 9, 30, 9, 22, 0])
    const end = canonicalEnd(now, '15m')
    assert.equal(new Date(end * 1000).toISOString(), '2026-10-30T09:29:59.000Z')
    const f = createTimeLabelFormatters(() => '15m')
    assert.equal(f.tickMarkFormatter(end, TIME), '09:15')
    assert.equal(f.tickMarkFormatter(end, TIME_SEC), '09:15:00')
    assert.equal(f.timeFormatter(end), "30 Oct '26   09:15")
})

test('hour/day rollover and date tick types keep LWC defaults', () => {
    const f = createTimeLabelFormatters(() => '1h')
    const midnight = utc([2026, 9, 31, 0, 0, 0])
    assert.equal(f.timeFormatter(canonicalEnd(midnight, '1h')), "31 Oct '26   00:00")
    assert.equal(f.timeFormatter(canonicalEnd(midnight - 3600, '1h')), "30 Oct '26   23:00")
    for (const type of [0, 1, 2]) assert.equal(f.tickMarkFormatter(canonicalEnd(midnight, '1h'), type), null)
    const g = createTimeLabelFormatters(() => '1m')
    assert.equal(g.tickMarkFormatter(canonicalEnd(utc([2026, 9, 30, 10, 0, 0]), '1m'), TIME), '10:00')
    // Dublin DST end (2026-10-25 01:00 UTC): labels stay UTC, no host/local offset.
    assert.equal(g.tickMarkFormatter(canonicalEnd(utc([2026, 9, 25, 1, 0, 0]), '1m'), TIME), '01:00')
})

test('formatter reads the timeframe live (no stale tf capture)', () => {
    let tf = '15m'
    const f = createTimeLabelFormatters(() => tf)
    const t = canonicalEnd(utc([2026, 9, 30, 9, 22, 0]), '1h')
    assert.equal(f.tickMarkFormatter(t, TIME), '09:45')
    tf = '1h'
    assert.equal(f.tickMarkFormatter(t, TIME), '09:00')
})

test('app wires formatters without touching data timestamps; module ships in assembled static', () => {
    const app = fs.readFileSync(path.join(ROOT, 'static/js/app.js'), 'utf8')
    assert.match(app, /import \{ createTimeLabelFormatters \} from "\.\/time-labels\.mjs\?v=local-wasm-2"/)
    assert.match(app, /localization: \{ timeFormatter: timeLabels\.timeFormatter \}/)
    assert.match(app, /tickMarkFormatter: timeLabels\.tickMarkFormatter/)
    assert.match(app, /timelineTimes = bars\.map\(b => b\.time\)/)
    const dir = fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'tl-'))
    try {
        const wasm = path.join(dir, 'x.wasm')
        fs.writeFileSync(wasm, Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]))
        const out = path.join(dir, 'static')
        execFileSync('sh', [path.join(ROOT, 'scripts/assemble-static.sh'), path.join(ROOT, 'static'), wasm, wasm, out], { cwd: dir, stdio: 'pipe' })
        const shipped = path.join(out, 'js/time-labels.mjs')
        assert.deepEqual(fs.readFileSync(shipped), fs.readFileSync(path.join(ROOT, 'static/js/time-labels.mjs')))
        return import(pathToFileURL(shipped).href).then(m => {
            assert.equal(m.createTimeLabelFormatters(() => '15m').tickMarkFormatter(canonicalEnd(utc([2026, 9, 30, 9, 22, 0]), '15m'), TIME), '09:15')
        }).finally(() => fs.rmSync(dir, { recursive: true, force: true }))
    } catch (e) { fs.rmSync(dir, { recursive: true, force: true }); throw e }
})
