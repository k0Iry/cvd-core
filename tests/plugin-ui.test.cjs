// Generic plugin UI behaviour against the ACTUAL host envelopes. Cases marked
// "real fixture" consume tests/fixtures/plugin-web.json, produced by
// cvd-indicators/conformance/src/bin/web-fixture.rs: real public cdylib output
// (retired) computed by the former native runner, wrapped in the host's
// /indicators, /history and typed /ws envelopes.
const { test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { harness, bar, rawBar, seed, TEST_DESCRIPTORS, entry, out } = require('./app-harness.cjs')

const FIXTURE = JSON.parse(fs.readFileSync(path.join(__dirname, 'fixtures/plugin-web.json'), 'utf8'))
const plain = value => JSON.parse(JSON.stringify(value))
const live = h => h.series.filter(item => !item.removed)
const respond = body => `async () => ({ ok: true, json: async () => (${JSON.stringify(body)}) })`

async function realHarness(options = {}) {
    const h = harness({}, { descriptors: options.discovery ?? FIXTURE.discovery.indicators, projection: false })
    h.run(`modeConnectionsReady = true; fetch = ${respond(options.history ?? FIXTURE.history)}`)
    await h.run('loadInitial()')
    h.flushFrames(); h.writes.length = 0
    return h
}

test('production app source labels and pane readouts use actual quote units, aggregate stays nominal', () => {
    const h = harness()
    h.run('modeConnectionsReady=true; availableInstruments=Object.values(SOURCE_INSTRUMENTS); setBars([{time:60,price_close:100,cvd_usdt:8,buy_volume_usdt:6,sell_volume_usdt:5,delta_usdt:1}])')
    h.flushFrames()
    const panes = h.run('registry.panes.length')
    for (const [exchange,market,unit,label] of [
        ['binance','spot','USDT','USDT spot'], ['binance','spot_usdc','USDC','USDC spot'],
        ['binance','usdm','USDT','USDT linear perpetual'], ['binance','usdm_usdc','USDC','USDC linear perpetual'],
        ['binance','coinm','USD','USD inverse perpetual'], ['coinbase','spot','USD','USD spot'], ['coinbase','usdc','USDC','USDC DRB perpetual'],
    ]) {
        h.run(`exchangeSel.value=${JSON.stringify(exchange)};marketSel.value=${JSON.stringify(market)};updateSourceSeriesLabel();updateFlowReadout()`)
        assert.match(h.run('sourceSeriesEl.textContent'),new RegExp(label))
        assert.match(h.run('flowSummaryEl.textContent'),new RegExp(`${unit} quote turnover$`))
    }
    h.run('aggToggle.checked=true;refreshModeData()'); h.flushFrames()
    assert.match(h.run('flowSummaryEl.textContent'), /nominal USD\/USDT\/USDC quote · no FX conversion$/)
    assert.equal(h.run('registry.panes.length'),panes)
});

test('bare host: no indicators -> price pane + host backbone only; no plugin series or controls', () => {
    const h = harness({}, { descriptors: [] })
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify([bar(60), bar(120), bar(180)])})`)
    h.flushFrames()
    assert.deepEqual([...new Set(live(h).map(item => item.pane))], [0])
    assert.equal(live(h).length, 2, 'candles + backbone')
    assert.deepEqual(plain(h.run('backboneSeries.data')).map(p => p.time), [60, 120, 180])
    assert.equal(h.run('__pluginRows().length'), 0, 'no plugin rows')
    assert.match(h.run('pluginControlsEl.children[0]?.textContent ?? ""'), /Indicators · 0|^$/)
    assert.equal(h.run('paneLabels.children.length'), 1)
})

test('backbone regression: hidden numerical-zero scaffolding (not whitespace), excluded from autoscale', async () => {
    const h = await realHarness()
    const data = plain(h.run('backboneSeries.data'))
    assert.equal(data.length, FIXTURE.history.bars.length)
    assert.ok(data.every(p => p.value === 0), 'whitespace points do not anchor LWC base index on window slide')
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.append_bar)})`); h.flushFrames()
    assert.deepEqual(plain(h.run('backboneSeries.data')).at(-1), { time: FIXTURE.live.append_bar.bar.time, value: 0 })
    const opts = h.run('backboneSeries.options')
    assert.equal(opts.visible, false); assert.equal(opts.autoscaleInfoProvider(), null)
})

test('source reload resets indicator results and status; old results never attach to new bars', async () => {
    const h = await realHarness()
    h.run(`fetch = ${respond({ ...FIXTURE.history, indicators: {} })}`)
    await h.run('reload()'); h.flushFrames()
    assert.equal(h.run('[...localValues.values()].reduce((n, c) => n + c.size, 0)'), 0)
    assert.ok(plain(h.run('cvdSeries.data')).every(p => p.value === undefined), 'same timestamps, new source: no carried-over values')
})

test('descriptor change (plugin removed/upgraded) destroys its series; same signature keeps renderers', () => {
    const h = harness(); seed(h)
    const before = h.run('renderers.get("cvd/cvd")')
    assert.equal(h.run(`applyPluginDescriptors(${JSON.stringify(TEST_DESCRIPTORS)})`), false)
    assert.equal(h.run('renderers.get("cvd/cvd")'), before)
    assert.equal(h.run(`applyPluginDescriptors(${JSON.stringify(TEST_DESCRIPTORS.filter(d => d.id !== 'bands'))})`), true)
    h.flushFrames()
    assert.equal(live(h).filter(item => item.pane === 0).length, 2, 'only candles + backbone remain on price')
    assert.deepEqual(plain(h.run('cvdSeries.data')).map(p => p.time), [60, 120], 'new renderer refilled from the cache')
})

test('plugin preferences persist generically and invalid entries are dropped', () => {
    const h = harness(); seed(h)
    h.run('prefsHydrated = true; globalThis.saved = null; localStorage.setItem = (k, v) => { saved = v }')
    h.run('flowModeSel.value = "flow/buy_sell"; flowModeSel.onchange()')
    assert.deepEqual(JSON.parse(h.run('saved')).plugins.modes, { 'flow:flow': 'flow/buy_sell' })
    assert.deepEqual(plain(h.run('sanitizePluginPrefs({ enabled: { a: true, b: "x" }, modes: { g: "k", h: 3 } })')), { enabled: { a: true }, modes: { g: 'k' } })
    h.run('pluginPrefs.modes["flow:flow"] = "flow/gone"; applyPluginVisibility()')
    assert.equal(h.run('deltaPaneSeries.options.visible'), true)
})

test('renderer object count stays bounded under heavy fragmentation', () => {
    const h = harness(); seed(h)
    const bars = Array.from({ length: 3000 }, (_, i) => {
        const gap = i % 3 === 0
        return { ...rawBar((i + 1) * 60, 100 + i), lt: gap ? [null, null, null, null] : [null, 1, 2, 3], lb: gap ? [null, null, null, null] : [null, -1, -2, -3] }
    })
    h.run(`setBars(${JSON.stringify(bars)})`); h.flushFrames()
    assert.ok(h.run('renderers.get("bands/bands").seriesCount()') <= 12)
    assert.ok(live(h).length <= 2 + 1 + 2 + 12)
})

test('generic non-LEVELS descriptor-only plugin: own panes, toggle and widths without ID-specific code', () => {
    const extra = entry({ id: 'example.other', version: '0.1.0', name: 'Other', outputs: [
        out('a', 'A', 'separate', 'one', ['#f2994a'], 'line', { widths: [5] }), out('b', 'B', 'separate', 'two', [], 'histogram')] })
    const h = harness({}, { descriptors: [...TEST_DESCRIPTORS, extra] })
    seed(h)
    const a = h.run('renderers.get("example.other/a").seriesHandle()')
    assert.equal(a.options.lineWidth, 5)
    assert.deepEqual(plain(a.data), [{ time: 60 }, { time: 120 }], 'no results yet: whitespace')
    h.run('__applyLocal("example.other", "source", { times: [120], series: { a: { type: "line", values: [0] }, b: { type: "histogram", values: [3] } } }); refreshPluginOutputs(new Set(["example.other"]), false)')
    assert.deepEqual(plain(a.data), [{ time: 60 }, { time: 120, value: 0 }])
    h.run('(() => { const i = __pluginRows().find(c => c.dataset.plugin === "example.other").children[0]; i.checked = false; i.onchange() })()')
    assert.deepEqual(plain(a.data), [])
})

// Base WS rev fence (connection epoch) and retired server indicator* frames,
// on the real app.js code (plugin-store/plugin-wire were deleted).
test('base rev fence: same-epoch lower rev ignored, rev 0 kept, reconnect accepts low rev, reload clears', async () => {
    const h = await realHarness()
    const A = FIXTURE.live.append_bar, t = A.bar.time
    const bar = d => ({ ...A.bar, price_close: A.bar.price_close + d })
    const apply = (rev, d) => { h.run(`applyRealtimePayload(${JSON.stringify({ ...A, rev, bar: bar(d) })})`); h.flushFrames() }
    const close = () => plain(h.run('page.bars')).at(-1).price_close
    apply(0, 0); assert.equal(close(), bar(0).price_close)
    apply(0, 1); assert.equal(close(), bar(1).price_close, 'repeated rev 0 is not lower: accepted')
    assert.deepEqual(plain(h.run(`baseRevs.get(${t})`)), { epoch: h.run('baseEpoch'), rev: 0 })
    apply(5, 2); assert.equal(close(), bar(2).price_close)
    apply(4, 3); assert.equal(close(), bar(2).price_close, 'same-epoch lower rev is stale')
    assert.equal(h.run(`baseSuperseded(${t}, 4)`), true)
    assert.equal(h.run(`baseSuperseded(${t}, 5)`), false)
    assert.equal(h.run(`noteBaseRev({ bar: { time: ${t} }, rev: -1 }); noteBaseRev({ bar: { time: ${t} }, rev: 2.5 }); baseRevs.get(${t}).rev`), 5, 'negative/non-integer revs never recorded')
    const epoch = h.run('baseEpoch')
    h.run('globalThis.WebSocket = class { constructor() { WebSocket.last = this; this.readyState = 1 } send() {} close() { this.closed = true } }; WebSocket.OPEN = 1; WebSocket.CLOSING = 2; WebSocket.CLOSED = 3; realtimeEnabled = true; connectWs()')
    assert.equal(h.run('baseEpoch'), epoch + 1, 'reconnect starts a new connection epoch')
    assert.equal(h.run(`baseSuperseded(${t}, 1)`), false)
    // The base fence restarts per connection; the separate producer-epoch
    // validity authority (validAuth, unchanged) still governs row admission.
    h.run(`noteBaseRev({ bar: { time: ${t} }, rev: 1 })`)
    assert.deepEqual(plain(h.run(`baseRevs.get(${t})`)), { epoch: epoch + 1, rev: 1 }, 'after reconnect a lower rev replaces the old-epoch rev')
    assert.equal(h.run(`baseSuperseded(${t}, 0)`), true, 'and fences within the new epoch')
    h.run(`pruneBaseRevs(${t + 1})`); assert.equal(h.run('baseRevs.size'), 0, 'pruned with the canonical cache')
    apply(7, 5); assert.equal(h.run('baseRevs.size'), 1)
    h.run(`fetch = ${respond({ ...FIXTURE.history, indicators: {} })}`)
    await h.run('reload()'); h.flushFrames()
    assert.equal(h.run('baseRevs.size'), 0, 'reload clears base revs')
    assert.equal(h.run(`baseSuperseded(${t}, 1)`), false)
})

test('valid and malformed indicator* WS frames are ignored; base and non-indicator frames still reach source checks', async () => {
    const h = await realHarness()
    h.run('globalThis.WebSocket = class { constructor() { WebSocket.last = this; this.readyState = 1 } send() {} close() { this.closed = true } }; WebSocket.OPEN = 1; WebSocket.CLOSING = 2; WebSocket.CLOSED = 3; realtimeEnabled = true; connectWs()')
    h.run('globalThis.__checks = 0; const real = assertSourceSeries; assertSourceSeries = (...a) => { __checks++; return real(...a) }')
    const send = m => h.run(`WebSocket.last.onmessage({ data: ${JSON.stringify(JSON.stringify(m))} })`)
    const before = plain(h.run('page.bars'))
    for (const m of [FIXTURE.live.indicator_frames[0], { type: 'indicator', id: 'cvd', mode: 'source', generation: 1, seq: 'x', output: {} },
        { type: 'indicator_status' }, { type: 'indicator_reset', api_version: 99 }, { type: 'indicator', market: 'other', bar: { time: 1 } }]) send(m)
    assert.equal(h.run('__checks'), 0, 'indicator* frames never reach base handling')
    assert.deepEqual(plain(h.run('page.bars')), before)
    assert.notEqual(h.run('WebSocket.last.closed === true'), true, 'base feed stays open')
    send(FIXTURE.live.append_bar); send({ type: 'bar', ...FIXTURE.live.append_bar }); send(null); send({ ...FIXTURE.live.append_bar, market: 'not-this-market' })
    assert.equal(h.run('__checks'), 4, 'base, non-indicator typed, null and wrong-source frames all take the source check')
})
