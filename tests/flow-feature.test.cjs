// Frontend half of the former Flow feature tests. Field selection (single vs
// aggregate agb/ags/ad), reconciliation tolerance and "never fall back to
// another scope" now live in the public Rust flow plugin
// (cvd-indicators/indicators/flow, tests). The browser only maps declared
// SDK output values onto chart slots and readouts.
const { test } = require('node:test')
const assert = require('node:assert/strict')
const path = require('node:path')
const { pathToFileURL } = require('node:url')

const load = file => import(pathToFileURL(path.join(__dirname, '../static/js', file)).href)

test('dual_range {upper, lower} maps to the official dual-range slots around base 0', async () => {
    const { dualRangePoint } = await load('chart.js')
    assert.deepEqual(dualRangePoint(60, { upper: 125, lower: -80 }), { time: 60, values: [125, 0, -80, 0] })
})

test('invalid dual_range values become whitespace, while true zero flow remains valid', async () => {
    const { dualRangePoint } = await load('chart.js')
    for (const value of [undefined, null, 5, [1, 2], { upper: null, lower: 0 }, { upper: Number.NaN, lower: 0 }, { upper: 1, lower: Infinity }, { upper: 1 }]) {
        assert.deepEqual(dualRangePoint(1, value), { time: 1 })
    }
    const zero = dualRangePoint(5, { upper: 0, lower: -0 })
    assert.deepEqual(zero, { time: 5, values: [0, 0, 0, 0] })
    assert.equal(Object.is(zero.values[2], -0), false, 'no negative zero')
})

test('line/histogram keep zero, gap non-finite/absent values and colour by sign', async () => {
    const { linePoint, histogramPoint } = await load('chart.js')
    const style = { upColor: 'U', downColor: 'D' }
    assert.deepEqual(linePoint(1, 0), { time: 1, value: 0 })
    for (const value of [undefined, null, Number.NaN, Infinity, '3', {}, { value: 3 }]) assert.deepEqual(linePoint(1, value), { time: 1 })
    assert.deepEqual(histogramPoint(2, -4, style), { time: 2, value: -4, color: 'D' })
    assert.deepEqual(histogramPoint(2, 0, style), { time: 2, value: 0, color: 'U' })
    assert.deepEqual(histogramPoint(2, null, style), { time: 2 })
})

test('readouts derive from kind/label only; missing values read unavailable, zero reads 0', async () => {
    const { normalizeOutput, readoutText } = await load('chart.js')
    const plugin = { id: 'p' }
    const band = normalizeOutput(plugin, { key: 'o', label: 'Buy/Sell', pane: 'separate', type: 'dual_range' }, 0)
    assert.equal(readoutText(band, { upper: 125, lower: -80 }), 'Buy/Sell 125 / -80')
    assert.equal(readoutText(band, null), 'Buy/Sell unavailable / unavailable')
    const line = normalizeOutput(plugin, { key: 'l', label: 'CVD', pane: 'separate', type: 'line' }, 1)
    assert.equal(readoutText(line, 0), 'CVD 0')
    assert.equal(readoutText(line, 1.5e6), 'CVD 1.50M')
    assert.equal(readoutText(line, undefined), 'CVD unavailable')
    assert.equal(readoutText(normalizeOutput(plugin, { key: 's', pane: 'price', type: 'segmented_lines', layers: 2 }, 2), [1, 2]), '')
})

// Schema/type/range rejection is the compiled coordinator's (`approve`, SDK
// Descriptor::validate): see tests/local-plugins.test.mjs. Here: the chart
// projection of APPROVED descriptors plus display-only sanitising.
test('approved-descriptor projection: panes, groups, readout_only, widths and CSS colour sanitising', async () => {
    const { normalizeDescriptor, buildRegistry, activeGroupOutput } = await load('chart.js')
    const d = (outputs, extra = {}) => ({ id: 'p', version: '1', name: 'P', abi: 1, context: { bars: 0 }, outputs, ...extra })
    const plugin = normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'line', colors: ['url(javascript:1)'], widths: [99] }], { name: 'N'.repeat(500) }))
    assert.equal(plugin.outputs[0].style.color.startsWith('#'), true, 'unsafe colour replaced by palette')
    assert.equal(plugin.outputs[0].style.lineWidth, undefined, 'out-of-range CSS width falls back to renderer default')
    assert.equal(plugin.label.length, 60, 'display text bounded')
    const reg = buildRegistry([
        { id: 'p', status: 'ready', descriptor: d([{ key: 'x', pane: 'separate', pane_group: 'g', type: 'histogram' }, { key: 'y', pane: 'separate', pane_group: 'g', type: 'dual_range' },
            { key: 'z', pane: 'separate', pane_group: 'g', type: 'line' }, { key: 't', pane: 'separate', pane_group: 'g', type: 'line', readout_only: true }]) },
        { id: 'p', status: 'ready', descriptor: d([{ key: 'q', pane: 'price', type: 'line' }], { version: '2' }) },
        { id: 'starting.one', status: 'starting', descriptor: null },
        { id: 'mismatch', status: 'ready', descriptor: d([{ key: 'q', pane: 'price', type: 'line' }]) },
    ])
    assert.deepEqual(reg.rejected.map(r => r.message), ['duplicate indicator id p', 'descriptor id p does not match mismatch'])
    assert.deepEqual(reg.pending, [{ id: 'starting.one', status: 'starting', error: null }])
    assert.equal(reg.panes[1].outputs.find(o => o.id === 't').readoutOnly, true)
    const widths = normalizeDescriptor(d([{ key: 's', pane: 'price', type: 'segmented_lines', layers: 3, widths: [2, 4, 6] }, { key: 'l', pane: 'price', type: 'line', widths: [5] }, { key: 'e', pane: 'price', type: 'line' }]))
    assert.deepEqual(widths.outputs[0].layers.map(l => l.lineWidth), [2, 4, 6])
    assert.equal(widths.outputs[1].style.lineWidth, 5)
    assert.equal(widths.outputs[2].style.lineWidth, undefined, 'empty widths keep the native renderer default')
    assert.deepEqual(normalizeDescriptor(d([{ key: 's', pane: 'price', type: 'segmented_lines', layers: 2, widths: [3] }])).outputs[0].layers.map(l => l.lineWidth), [3, 3])
    const pane = reg.panes[1]
    assert.equal(pane.id, 'p:g')
    const group = pane.groups.get('p:g')
    assert.deepEqual(group.outputs.map(o => o.key), ['p/x', 'p/y'], 'bar-type outputs in one pane are alternatives; lines are not')
    assert.equal(activeGroupOutput(group, undefined).key, 'p/x', 'first declared is the default mode')
    assert.equal(activeGroupOutput(group, 'p/y').key, 'p/y')
    assert.equal(activeGroupOutput(group, 'p/missing').key, 'p/x')
})
