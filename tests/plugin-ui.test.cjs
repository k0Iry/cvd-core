// Generic plugin UI behaviour against the ACTUAL host envelopes. Cases marked
// "real fixture" consume tests/fixtures/plugin-web.json, produced by
// cvd-indicators/conformance/src/bin/web-fixture.rs: real public cdylib output
// computed by the real cvd-indicator-runner, wrapped in the host's
// /indicators, /history and typed /ws envelopes.
const { test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { harness, bar, rawBar, seed, TEST_DESCRIPTORS, entry, out } = require('./app-harness.cjs')

const FIXTURE = JSON.parse(fs.readFileSync(path.join(__dirname, 'fixtures/plugin-web.json'), 'utf8'))
const plain = value => JSON.parse(JSON.stringify(value))
const live = h => h.series.filter(item => !item.removed)
const js = file => fs.readFileSync(path.join(__dirname, '../static/js', file), 'utf8')
const respond = body => `async () => ({ ok: true, json: async () => (${JSON.stringify(body)}) })`
const frames = FIXTURE.live.indicator_frames
const frameOf = (id, seq) => frames.find(f => f.id === id && f.seq === seq)
const historyOut = (id, mode = 'source') => FIXTURE.history.indicators[id].modes[mode].output

async function realHarness(options = {}) {
    const h = harness({}, { descriptors: options.discovery ?? FIXTURE.discovery.indicators, projection: false })
    h.run(`modeConnectionsReady = true; fetch = ${respond(options.history ?? FIXTURE.history)}`)
    await h.run('loadInitial()')
    h.flushFrames(); h.writes.length = 0
    return h
}
const event = (h, message) => h.run(`handleIndicatorEvent(parseIndicatorFrame(${JSON.stringify(message)}))`)

test('bare host: no indicators -> price pane + host backbone only; no plugin series or controls', () => {
    const h = harness({}, { descriptors: [] })
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify([bar(60), bar(120), bar(180)])})`)
    h.flushFrames()
    assert.deepEqual([...new Set(live(h).map(item => item.pane))], [0])
    assert.equal(live(h).length, 2, 'candles + backbone')
    assert.deepEqual(plain(h.run('backboneSeries.data')).map(p => p.time), [60, 120, 180])
    assert.equal(h.run('pluginControlsEl.children.length'), 0)
    assert.equal(h.run('paneLabels.children.length'), 1)
})

test('discovery failure is labelled, retried with bounded backoff, and leaves a working price chart', async () => {
    const h = harness({}, { descriptors: null })
    h.run('fetch = async () => ({ ok: false, status: 503 })')
    await h.run('loadIndicators()')
    assert.match(h.run('pluginStateEl.title'), /Indicators unavailable: indicators HTTP 503/)
    assert.equal(h.run('discoveryTimer !== null'), true, 'retry scheduled')
    h.run('fetch = async () => ({ ok: true, json: async () => ({ api_version: 99, indicators: [] }) })')
    await h.run('loadIndicators()')
    assert.match(h.run('pluginStateEl.title'), /unsupported api_version 99/)
    for (let i = 0; i < 20; i++) { h.run('discoveryTimer && (clearTimeout(discoveryTimer), discoveryTimer = null); scheduleDiscoveryRefresh()') }
    assert.equal(h.run('discoveryAttempt'), h.run('DISCOVERY_RETRY_MS.length'), 'backoff is bounded')
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify([bar(60), bar(120)])})`)
    h.flushFrames()
    assert.equal(h.series[0].data.length, 2)
})

test('missing/failed/starting indicators are labelled; starting -> ready registers and resyncs results', async () => {
    const h = harness({}, { descriptors: null, projection: false })
    const starting = { api_version: 1, indicators: [
        { id: 'cvd', status: 'starting', descriptor: null },
        { id: 'levels', status: 'failed', descriptor: null, error: { code: 'incompatible', message: '/srv/private/liblevels.so: bad abi' } },
    ] }
    h.run(`modeConnectionsReady = true; fetch = ${respond(starting)}`)
    await h.run('loadIndicators()')
    assert.deepEqual(plain(h.run('registry.pending.map(p => [p.id, p.status])')), [['cvd', 'starting'], ['levels', 'failed']])
    assert.match(h.run('pluginStateEl.title'), /cvd starting/)
    assert.match(h.run('pluginStateEl.title'), /levels failed incompatible/)
    assert.equal(h.run('pluginStateEl.title').includes('/srv/private'), false, 'no raw paths/messages')
    assert.equal(h.run('pluginControlsEl.children.find(c => c.dataset.plugin === "cvd").children[0].disabled'), true)
    assert.equal(h.run('discoveryTimer !== null'), true, 'starting schedules a bounded refresh')
    h.run(`setBars(${JSON.stringify(FIXTURE.history.bars.slice(0, 3))})`); h.flushFrames()
    const ready = { api_version: 1, indicators: [FIXTURE.discovery.indicators.find(e => e.id === 'cvd'), starting.indicators[1]] }
    h.run(`globalThis.calls = []; fetch = async url => { calls.push(String(url)); return String(url).includes('/indicators') ? (${respond(ready)})() : (${respond(FIXTURE.history)})() }`)
    await h.run('loadIndicators()')
    await h.run('indicatorResync')
    h.flushFrames()
    assert.equal(h.run('registry.plugins.map(p => p.id).join()'), 'cvd')
    assert.ok(h.run('calls.some(u => u.includes("/history"))'), 'results resynced after becoming ready')
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, historyOut('cvd').series.cvd.values.at(-1))
})

test('real fixture: CVD/Flow/Total and a generic non-LEVELS plugin render from real columnar runner output', async () => {
    const h = await realHarness()
    const bars = FIXTURE.history.bars
    assert.deepEqual(plain(h.run('registry.panes.map(p => p.id)')), ['price', 'cvd:cvd', 'flow:flow', 'example.range:range'])
    const cvd = plain(h.run('cvdSeries.data'))
    assert.deepEqual(cvd.map(p => p.time), bars.map(b => b.time), 'exact canonical times')
    assert.deepEqual(cvd.map(p => p.value), historyOut('cvd').series.cvd.values)
    assert.deepEqual(cvd.map(p => p.value), bars.map(b => b.cvd_usdt), 'projection of canonical cvd, not a recomputation')
    const delta = plain(h.run('deltaPaneSeries.data'))
    const zeroIndex = bars.findIndex(b => b.buy_volume_usdt === 0 && b.sell_volume_usdt === 0)
    assert.equal(delta[zeroIndex].value, 0)
    h.run('flowModeSel.value = "flow/buy_sell"; flowModeSel.onchange()')
    const bs = plain(h.run('buySellSeries.data'))
    const fx = historyOut('flow').series.buy_sell
    const gapIndex = fx.upper.findIndex(v => v === null)
    assert.ok(gapIndex >= 0, 'fixture contains an unreconciled bar')
    assert.deepEqual(bs[gapIndex], { time: bars[gapIndex].time }, 'gap, never zero')
    assert.deepEqual(bs[zeroIndex].values, [0, 0, 0, 0])
    assert.deepEqual(bs[0].values, [fx.upper[0], 0, fx.lower[0], 0])
    assert.match(h.run('flowSummaryEl.textContent'), /· Total \d[\d.]*K$/)
    h.run('flowModeSel.value = "flow/delta"; flowModeSel.onchange()')
    assert.match(h.run('flowSummaryEl.textContent'), /· Total \d[\d.]*K$/, 'Total shown in both visual modes')
    assert.equal(h.run('renderers.has("flow/total")'), false, 'readout_only output has no native series')
    h.emit('chart', 'crosshair', { time: bars[gapIndex].time, sourceEvent: {} })
    assert.match(h.run('flowSummaryEl.textContent'), /Buy\/Sell unavailable \/ unavailable · Total unavailable/)
    h.emit('chart', 'crosshair', { time: bars[zeroIndex].time, sourceEvent: {} })
    assert.match(h.run('flowSummaryEl.textContent'), /Delta 0 · Buy\/Sell 0 \/ 0 · Total 0/)
    const mid = plain(h.run('renderers.get("example.range/mid").seriesHandle().data'))
    assert.equal(h.run('renderers.get("example.range/mid").seriesHandle().pane'), 0)
    assert.deepEqual(mid[0], { time: bars[0].time }, 'no full window -> gap')
    assert.equal(mid.at(-1).value, historyOut('example.range').series.mid.values.at(-1))
    assert.equal(h.run('renderers.get("example.range/width").seriesHandle().pane'), 3)
    h.run('aggToggle.checked = true; refreshModeData()'); h.flushFrames()
    assert.deepEqual(plain(h.run('cvdSeries.data')).map(p => p.value), historyOut('cvd', 'aggregate').series.cvd.values)
    for (const file of ['app.js', 'plugin-registry.mjs', 'plugin-render.mjs', 'segmented-series.mjs', 'plugin-wire.mjs', 'plugin-store.mjs']) {
        const code = js(file)
        for (const id of ['"cvd/', "'cvd/", 'flow/', 'levels', 'LEVELS', 'example.range', 'buy_sell', '.lt', '.alt', '.lb', '.alb', 'cvd_usdt', 'delta_usdt', 'agb', 'ags', '1292']) {
            assert.equal(code.includes(id), false, `${file} must not contain ${id}`)
        }
    }
})

test('real fixture live: indicator frame before its base bar stays pending, never extends the axis, then merges', async () => {
    const h = await realHarness()
    const before = h.run('backboneSeries.data.length')
    event(h, frameOf('cvd', 0))
    event(h, frameOf('cvd', 1))
    h.flushFrames()
    assert.equal(h.run('backboneSeries.data.length'), before, 'plugin data never extends the host time axis')
    assert.equal(plain(h.run('cvdSeries.data')).some(p => p.time === FIXTURE.live.append_bar.bar.time), false)
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.append_bar)})`); h.flushFrames()
    assert.deepEqual(plain(h.run('cvdSeries.data')).at(-1), { time: FIXTURE.live.append_bar.bar.time, value: frameOf('cvd', 1).output.series.cvd.values[0] },
        'pending result merged once base exists')
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.replace_bar)})`)
    event(h, frameOf('cvd', 2)); h.flushFrames()
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, FIXTURE.live.replace_bar.bar.cvd_usdt, 'replace, not accumulate')
    event(h, frameOf('cvd', 1)); h.flushFrames()
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, FIXTURE.live.replace_bar.bar.cvd_usdt, 'delayed older seq cannot overwrite')
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

test('real fixture as_of_rev: base rev newer than indicator revision -> pending gap, never stale value; matching rev restores', async () => {
    const h = await realHarness()
    const t = FIXTURE.live.append_bar.bar.time
    event(h, frameOf('cvd', 0))
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.append_bar)})`)
    event(h, frameOf('cvd', 1)); h.flushFrames()
    const appended = frameOf('cvd', 1).output.series.cvd.values[0]
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, appended, 'as_of_rev 1 == base rev 1: current')
    // Forming bar corrected (rev 2) before the matching indicator frame.
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.replace_bar)})`); h.flushFrames()
    const pending = plain(h.run('cvdSeries.data')).at(-1)
    assert.equal(pending.time, t)
    assert.equal(pending.value, undefined, 'stale rev-1 value not shown as current for rev-2 base')
    assert.equal(h.run('backboneSeries.data.length'), FIXTURE.history.bars.length + 1, 'pending never changes the canonical axis')
    event(h, frameOf('cvd', 2)); h.flushFrames()
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, frameOf('cvd', 2).output.series.cvd.values[0], 'matching revision restores')
    // Malformed revision frame on the socket path fails closed (resync), never paints.
    h.run(`globalThis.__resync = 0; resyncIndicators = () => { __resync++ }`)
    event(h, frameOf('cvd', 2)) // duplicate seq ignored
    assert.throws(() => event(h, { ...frameOf('cvd', 2), seq: 9, as_of_rev: -1, output: { times: [t], series: { cvd: { type: 'line', values: [-123] } } } }), /as_of_rev/)
    h.flushFrames()
    assert.notEqual(plain(h.run('cvdSeries.data')).at(-1).value, -123)
})

test('runner restart: new session at seq 0 is accepted; old session and older generation frames are fenced', async () => {
    const h = await realHarness()
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.append_bar)})`)
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.replace_bar)})`)
    const f2 = frameOf('cvd', 2)
    event(h, f2)
    const time = f2.output.times[0]
    const value = v => ({ times: [time], series: { cvd: { type: 'line', values: [v] } } })
    const restarted = { ...f2, session: f2.session + 100, seq: 0, output: value(42) }
    event(h, restarted); h.flushFrames()
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, 42)
    event(h, { ...f2, seq: 3, output: value(-1) }); h.flushFrames()
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, 42, 'old session seq 3 cannot overwrite the new session')
    event(h, { ...restarted, generation: restarted.generation - 1, session: 999, seq: 50, output: value(-2) }); h.flushFrames()
    assert.equal(plain(h.run('cvdSeries.data')).at(-1).value, 42, 'older hub generation is fenced')
})

test('indicator_status unavailable clears and labels; ok + next frame restores; hidden makes zero chart writes', async () => {
    const h = await realHarness()
    const f0 = frameOf('cvd', 0)
    event(h, { type: 'indicator_status', api_version: 1, id: 'cvd', mode: 'source', generation: f0.generation, session: f0.session, status: 'unavailable', error: { code: 'crashed', message: 'segfault at /x' } })
    h.flushFrames()
    assert.deepEqual(plain(h.run('cvdSeries.data')), [], 'no stale CVD line')
    assert.match(h.run('pluginStateEl.title'), /Cumulative Volume Delta \(CVD\) unavailable crashed/)
    assert.equal(h.run('pluginStateEl.title').includes('/x'), false)
    assert.equal(plain(h.run('deltaPaneSeries.data')).length, FIXTURE.history.bars.length, 'other indicators unaffected')
    h.document.hidden = true; h.emit('document', 'visibilitychange'); h.writes.length = 0
    // Matching base bootstrap (rev observed) precedes the recovered result.
    h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.append_bar)})`); h.writes.length = 0
    event(h, { type: 'indicator_status', api_version: 1, id: 'cvd', mode: 'source', generation: f0.generation, session: f0.session + 1, status: 'ok' })
    event(h, { ...f0, session: f0.session + 1 })
    assert.deepEqual(h.writes.filter(w => ['setData', 'update', 'removeSeries', 'addSeries', 'applyOptions'].includes(w)), [], 'hidden: zero native chart writes')
    h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    const cvd = plain(h.run('cvdSeries.data'))
    assert.equal(cvd.find(p => p.time === f0.output.times[0]).value, f0.output.series.cvd.values[0])
    assert.equal(cvd[0].value, undefined, 'history dropped with the failed session: gap, not stale')
    assert.equal(h.run('pluginStateEl.textContent'), '')
})

test('indicator_reset drops live state and refetches results; unknown id triggers discovery refresh', async () => {
    const h = await realHarness()
    h.run(`globalThis.calls = []; fetch = async url => { calls.push(String(url)); return (${respond(FIXTURE.history)})() }`)
    const g = frameOf('cvd', 0).generation + 1
    event(h, { type: 'indicator_reset', api_version: 1, generation: g })
    assert.equal(h.run('indicatorStore.stats().points'), 0)
    await h.run('indicatorResync'); h.flushFrames()
    assert.ok(h.run('calls.some(u => u.includes("/history"))'))
    assert.equal(plain(h.run('cvdSeries.data')).filter(p => p.value !== undefined).length, FIXTURE.history.bars.length)
    event(h, { ...frameOf('cvd', 0), output: { times: [FIXTURE.history.bars.at(-1).time], series: { cvd: { type: 'line', values: [-5] } } } }); h.flushFrames()
    assert.notEqual(plain(h.run('cvdSeries.data')).at(-1).value, -5, 'pre-reset generation fenced')
    h.run('discoveryTimer && (clearTimeout(discoveryTimer), discoveryTimer = null)')
    event(h, { ...frameOf('cvd', 0), generation: g, id: 'newly.installed' })
    assert.equal(h.run('discoveryTimer !== null'), true)
})

test('malformed indicator frame resyncs instead of closing the base feed', async () => {
    const h = await realHarness()
    h.run(`globalThis.calls = 0; fetch = async () => { calls++; return (${respond(FIXTURE.history)})() }`)
    h.run('globalThis.WebSocket = class { constructor() { WebSocket.last = this; this.readyState = 1 } send() {} close() { this.closed = true } }; WebSocket.OPEN = 1; WebSocket.CLOSING = 2; WebSocket.CLOSED = 3; realtimeEnabled = true; connectWs()')
    h.run(`WebSocket.last.onmessage({ data: JSON.stringify({ type: 'indicator', id: 'cvd', mode: 'source', generation: 1, seq: 'x', output: {} }) })`)
    assert.equal(h.run('WebSocket.last.closed === true'), false, 'base feed stays open')
    await h.run('indicatorResync')
    assert.ok(h.run('calls') >= 1)
})

test('history with an unavailable mode or without results never fakes zeros', async () => {
    const page = { ...FIXTURE.history, indicators: { ...FIXTURE.history.indicators, cvd: { version: '1.0.0', modes: { source: { status: 'unavailable', error: { code: 'timeout', message: 'slow' } } } } } }
    const h = await realHarness({ history: page })
    assert.deepEqual(plain(h.run('cvdSeries.data')), [])
    assert.match(h.run('pluginStateEl.title'), /unavailable timeout/)
    const none = await realHarness({ history: { ...FIXTURE.history, indicators: {} } })
    assert.ok(plain(none.run('cvdSeries.data')).every(p => p.value === undefined), 'no results -> whitespace only')
})

test('source reload resets indicator results and status; old results never attach to new bars', async () => {
    const h = await realHarness()
    h.run(`fetch = ${respond({ ...FIXTURE.history, indicators: {} })}`)
    await h.run('reload()'); h.flushFrames()
    assert.equal(h.run('indicatorStore.stats().points'), 0)
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
    h.run('indicatorStore.applyHistory("example.other", "source", { times: [120], series: { a: { type: "line", values: [0] }, b: { type: "histogram", values: [3] } } }); refreshPluginOutputs(new Set(["example.other"]), false)')
    assert.deepEqual(plain(a.data), [{ time: 60 }, { time: 120, value: 0 }])
    h.run('(() => { const i = pluginControlsEl.children.find(c => c.dataset.plugin === "example.other").children[0]; i.checked = false; i.onchange() })()')
    assert.deepEqual(plain(a.data), [])
})

// Coherence: basic public CVD, whenever shown, equals the displayed canonical
// bar's cvd_usdt (base bars and derived results are one admission decision).
function assertCoherent(h, label) {
    const bars = plain(h.run('page.bars'))
    const byTime = new Map(bars.map(b => [b.time, b]))
    let shown = 0
    for (const p of plain(h.run('cvdSeries.data'))) {
        if (p.value === undefined) continue
        shown++
        assert.ok(byTime.has(p.time), `${label}: missing bar ${p.time} (bars ${bars.length} last ${bars.at(-1)?.time})`)
        assert.equal(p.value, byTime.get(p.time).cvd_usdt, `${label}: CVD@${p.time} equals displayed cvd_usdt`)
    }
    return shown
}
const historyFor = (bar, value) => ({ ...FIXTURE.history, bars: [bar], indicators: { cvd: { version: FIXTURE.history.indicators.cvd.version, modes: {
    source: { status: 'ok', output: { times: [bar.time], series: { cvd: { type: 'line', values: [value] } } } } } } } })
async function overlap(h, body) {
    h.run(`(() => { const d = ${JSON.stringify(body)}; applyHistoryIndicators(d); applyHistoryPage({ ...d, bars: mergeHistoryBars(d.bars) }, currentChartRange(), d.bars) })()`)
    h.flushFrames()
}
const lastCvd = h => plain(h.run('cvdSeries.data')).at(-1)

test('coherence: HTTP overlap same tuple keeps live; revised tuple replaces base AND result; stale live cannot revive', async () => {
    const h = await realHarness()
    const A = FIXTURE.live.append_bar, t = A.bar.time
    h.run(`applyRealtimePayload(${JSON.stringify(A)})`); event(h, frameOf('cvd', 1)); h.flushFrames()
    const v1 = frameOf('cvd', 1).output.series.cvd.values[0]
    assert.equal(lastCvd(h).value, v1); assertCoherent(h, 'live')
    // same tuple: verified live proof retained
    await overlap(h, historyFor(A.bar, v1))
    assert.equal(lastCvd(h).value, v1, 'same tuple keeps live value'); assertCoherent(h, 'same tuple')
    // revised tuple (parent repro): base and result switch together
    const revised = { ...A.bar, cvd_usdt: A.bar.cvd_usdt + 100 }
    await overlap(h, historyFor(revised, revised.cvd_usdt))
    assert.equal(plain(h.run('page.bars')).at(-1).cvd_usdt, revised.cvd_usdt)
    assert.equal(lastCvd(h).value, revised.cvd_usdt, 'revised history result shown, not old live 100-style value')
    assertCoherent(h, 'revised')
    // delayed live base (same rev) and delayed result (as_of_rev <= superseded) are stale
    h.run(`applyRealtimePayload(${JSON.stringify(A)})`)
    event(h, { ...frameOf('cvd', 1), seq: 5, output: { times: [t], series: { cvd: { type: 'line', values: [-777] } } } }); h.flushFrames()
    assert.equal(plain(h.run('page.bars')).at(-1).cvd_usdt, revised.cvd_usdt, 'stale live base cannot replace accepted history')
    assert.equal(lastCvd(h).value, revised.cvd_usdt); assertCoherent(h, 'delayed stale')
    // output-before-base: newer result pending until its base is displayed
    const R = FIXTURE.live.replace_bar
    event(h, { ...frameOf('cvd', 2), seq: 6 }); h.flushFrames()
    assert.equal(lastCvd(h).value, undefined, 'newer result pending while historical tuple displayed')
    assertCoherent(h, 'output before base')
    h.run(`applyRealtimePayload(${JSON.stringify(R)})`); h.flushFrames()
    assert.equal(lastCvd(h).value, R.bar.cvd_usdt, 'matching accepted base restores live authority'); assertCoherent(h, 'restored')
})

test('coherence: history response arriving after newer live stays coherent with the displayed bar', async () => {
    const h = await realHarness()
    const A = FIXTURE.live.append_bar, R = FIXTURE.live.replace_bar
    h.run(`applyRealtimePayload(${JSON.stringify(A)})`); event(h, frameOf('cvd', 1))
    h.run(`applyRealtimePayload(${JSON.stringify(R)})`); event(h, frameOf('cvd', 2)); h.flushFrames()
    assert.equal(lastCvd(h).value, R.bar.cvd_usdt); assertCoherent(h, 'newer live')
    await overlap(h, historyFor(A.bar, A.bar.cvd_usdt))
    assert.ok(assertCoherent(h, 'late history') > 0)
    const shown = plain(h.run('page.bars')).at(-1).cvd_usdt
    const v = lastCvd(h).value
    assert.ok(v === undefined || v === shown)
})

// Metadata-only authority transition: a matching base with IDENTICAL canonical
// values but a newer rev must make the pending result renderable (visible and
// hidden->return), while a true duplicate stays a no-op.
for (const hidden of [false, true]) test(`coherence: same-value result before base becomes visible on matching base rev (${hidden ? 'hidden' : 'visible'})`, async () => {
    const h = await realHarness()
    const A = FIXTURE.live.append_bar, R = FIXTURE.live.replace_bar, t = A.bar.time
    h.run(`applyRealtimePayload(${JSON.stringify(A)})`); event(h, frameOf('cvd', 1)); h.flushFrames()
    const revised = { ...A.bar, cvd_usdt: A.bar.cvd_usdt + 100 }
    await overlap(h, historyFor(revised, revised.cvd_usdt))
    assert.equal(lastCvd(h).value, revised.cvd_usdt)
    const f = frameOf('cvd', 2)
    event(h, { ...f, seq: 6, output: { times: [t], series: { cvd: { type: 'line', values: [revised.cvd_usdt] } } } }); h.flushFrames()
    assert.equal(lastCvd(h).value, undefined, 'pending before matching base')
    if (hidden) { h.document.hidden = true; h.emit('document', 'visibilitychange'); h.writes.length = 0 }
    h.run(`applyRealtimePayload(${JSON.stringify({ ...R, rev: f.as_of_rev, bar: revised })})`); h.flushFrames()
    if (hidden) {
        assert.deepEqual(h.writes.filter(w => ['setData', 'update', 'removeSeries', 'addSeries', 'applyOptions'].includes(w)), [], 'hidden: zero writes')
        h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    }
    assert.equal(h.run(`indicatorStore.get('cvd/cvd','source',${t})`), revised.cvd_usdt)
    assert.equal(lastCvd(h).value, revised.cvd_usdt, 'renderer repainted on authority transition'); assertCoherent(h, 'metadata-only')
    h.writes.length = 0
    h.run(`applyRealtimePayload(${JSON.stringify({ ...R, rev: f.as_of_rev, bar: revised })})`); h.flushFrames()
    assert.deepEqual(h.writes.filter(w => ['setData', 'update'].includes(w)), [], 'true duplicate is a no-op')
})

test('real app: live -> future result -> base keeps visible CVD equal to displayed cvd_usdt (visible + hidden restore)', async () => {
    for (const hidden of [false, true]) {
        const h = await realHarness()
        const check = msg => {
            const last = plain(h.run('cvdSeries.data')).at(-1)
            const base = plain(h.run('page.bars.at(-1)'))
            if (last.time === base.time && last.value !== undefined) assert.equal(last.value, base.cvd_usdt, msg)
            return last
        }
        h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.append_bar)})`)
        event(h, frameOf('cvd', 1)); h.flushFrames()
        assert.equal(check('rev1').value, FIXTURE.live.append_bar.bar.cvd_usdt)
        if (hidden) { h.document.hidden = true; h.emit('document', 'visibilitychange') }
        event(h, frameOf('cvd', 2)); h.flushFrames()
        if (!hidden) assert.equal(check('future result pending').value, undefined, 'future result is pending (gap), not shown')
        h.run(`applyRealtimePayload(${JSON.stringify(FIXTURE.live.replace_bar)})`); h.flushFrames()
        if (hidden) { h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames() }
        assert.equal(check('after matching base').value, FIXTURE.live.replace_bar.bar.cvd_usdt, 'repainted on observation')
    }
})
