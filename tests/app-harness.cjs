const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')

// Synthetic discovery entries in the real host shape ({id, status,
// descriptor}) with SDK v1 descriptors. `bands` is a generic segmented_lines
// test plugin with arbitrary synthetic values, not any real algorithm. Real
// cdylib output under the real runner is covered by tests/fixtures/plugin-web.json.
const entry = descriptor => ({ id: descriptor.id, status: 'ready', descriptor: { abi: 1, context: { bars: 0 }, params: [], inputs: [], live: true, ...descriptor } })
const out = (key, label, pane, pane_group, colors, type, extra = {}) => ({ key, label, pane, pane_group, colors, widths: [], readout_only: false, type, ...extra })
const TEST_DESCRIPTORS = [
    entry({ id: 'cvd', version: '1.0.0', name: 'Cumulative Volume Delta (CVD)', outputs: [out('cvd', 'CVD', 'separate', null, ['#56ccf2'], 'line')] }),
    entry({ id: 'flow', version: '1.0.0', name: 'Flow', outputs: [
        out('delta', 'Delta', 'separate', 'flow', ['#27ae60', '#eb5757'], 'histogram'),
        out('buy_sell', 'Buy/Sell', 'separate', 'flow', ['#27ae60', '#eb5757'], 'dual_range'),
        out('total', 'Total', 'separate', 'flow', [], 'line', { readout_only: true })] }),
    entry({ id: 'bands', version: '0.0.1', name: 'Bands', outputs: [out('bands', 'Bands', 'price', null, ['#eb5757', '#27ae60'], 'segmented_lines',
        { layers: 6, tail_marker: true, widths: [2, 2, 4, 4, 6, 6] })] }),
]
// Bars are complete canonical rows; the test host projection below turns
// their fields into SDK Output columns written through the real store.
function ind(b) { return b }
function flowRe(buy, sell, delta) {
    const f = v => v === 'unavailable' ? 'unavailable' : String(v)
    const lower = sell === 'unavailable' ? 'unavailable' : (Number(sell) === 0 ? '0' : '-' + sell)
    const total = buy === 'unavailable' ? 'unavailable' : String(Number(buy) + Number(sell))
    return new RegExp(`Delta ${f(delta)} · Buy/Sell ${f(buy)} / ${lower} · Total ${total}`)
}
const PROJECTION = `
function __flow(buy, sell, delta) {
    const ok = [buy, sell, delta].every(Number.isFinite) && buy >= 0 && sell >= 0 && Math.abs(buy - sell - delta) <= Math.max(1e-6, Math.max(1, Math.abs(buy), Math.abs(sell), Math.abs(delta)) * 1e-9)
    return ok ? [buy, sell === 0 ? 0 : -sell, buy + sell] : [null, null, null]
}
// Test host: per bar, one SDK Output per (indicator, mode), like the server.
function __project(bars) {
    for (const mode of ['source', 'aggregate']) {
        const agg = mode === 'aggregate'
        const times = bars.map(b => b.time)
        const col = f => bars.map(b => { const v = f(b); return Number.isFinite(v) ? v : null })
        const keep = bars.filter(b => !(b._omit || []).includes('cvd'))
        if (keep.length) __applyLocal('cvd', mode, { times: keep.map(b => b.time), series: { cvd: { type: 'line', values: keep.map(b => { const v = agg ? b.ac : b.cvd_usdt; return Number.isFinite(v) ? v : null }) } } })
        const flows = bars.map(b => agg ? __flow(b.agb, b.ags, b.ad) : __flow(b.buy_volume_usdt, b.sell_volume_usdt, b.delta_usdt))
        __applyLocal('flow', mode, { times, series: {
            delta: { type: 'histogram', values: col(b => agg ? b.ad : b.delta_usdt) },
            buy_sell: { type: 'dual_range', upper: flows.map(f => f[0]), lower: flows.map(f => f[1]) },
            total: { type: 'line', values: flows.map(f => f[2]) } } })
        const withBands = bars.filter(b => Array.isArray(agg ? b.alt : b.lt))
        if (withBands.length) {
            const layers = Array.from({ length: 6 }, (_, k) => withBands.map(b => {
                const v = (agg ? (k % 2 ? b.alb : b.alt) : (k % 2 ? b.lb : b.lt))[1 + (k >> 1)]
                return Number.isFinite(v) ? v : null
            }))
            __applyLocal('bands', mode, { times: withBands.map(b => b.time), series: { bands: { type: 'segmented_lines', layers } } })
        }
    }
}
{ const real = setBars; setBars = (bars, ...rest) => { __project(bars); return real(bars, ...rest) } }
{ const real = applyTailBars; applyTailBars = (bars, ...rest) => { __project(bars); return real(bars, ...rest) } }
`
// Names the original assertions used, mapped onto the generic plugin UI.
const TEST_BRIDGE = `
// Local-package bridge for unit tests: descriptors applied directly become
// enabled local records; projected outputs land in the browser result map
// exactly as accepted WASM computes do (real WASM runs in the Chromium gates).
function __applyLocal(id, mode, output) {
    for (const [key, s] of Object.entries(output.series)) {
        const k = id + '/' + key + '|' + mode
        const col = localValues.get(k) ?? new Map()
        output.times.forEach((t, i) => { const v = seriesValueAt(s, i); if (v !== undefined) col.set(t, v) })
        localValues.set(k, col)
    }
}
{ const real = applyPluginDescriptors; applyPluginDescriptors = (entries, ...rest) => {
    for (const e of entries || []) if (e && e.descriptor && !localRecords.has(e.id)) localRecords.set(e.id, { id: e.id, name: e.id, enabled: true, descriptor: e.descriptor, params: {} })
    return real(entries, ...rest) } }
// Rows live in the popup (outside the toolbar); collect them at any depth.
function __pluginRows(node = pluginMenuEl, found = []) { for (const c of node.children ?? []) { if (c.dataset?.plugin) found.push(c); else __pluginRows(c, found) } return found }
const __menuEmpty = () => { const walk = n => n.dataset?.empty !== undefined ? n : (n.children ?? []).map(walk).find(Boolean); return walk(pluginMenuEl) }
const __menuKeydown = e => indicatorMenu.handleKeydown(e)
const __menuGroup = () => pluginMenuEl.children.find(c => c.attributes?.role === 'group')
const __menuPointer = e => indicatorMenu.handlePointer(e)
const __control = id => __pluginRows().find(label => label.dataset.plugin === id)?.children[0]
const __flowSelect = () => paneLabels.children.find(el => el.dataset.pane === 'flow:flow')?.children.find(el => el.dataset?.group)
Object.defineProperties(globalThis, {
    cvdSeries: { get: () => renderers.get('cvd/cvd').seriesHandle() },
    deltaPaneSeries: { get: () => renderers.get('flow/delta').seriesHandle() },
    buySellSeries: { get: () => renderers.get('flow/buy_sell').seriesHandle() },
    deltaZeroSeries: { get: () => backboneSeries },
    levelsRenderedSeries: { get: () => renderers.get('bands/bands')?.entries() ?? [] },
    levelsSlots: { get: () => renderers.get('bands/bands')?.slots() ?? [] },
    levelsToggle: { get: () => __control('bands') },
    flowModeSel: { get: () => __flowSelect() },
    flowSummaryEl: { get: () => paneSummaryEls.find(el => el.dataset.paneSummary === 'flow:flow') },
})
function updateFlowMode() { flowModeSel.onchange() }
function setLevelsData(bars) {
    if (typeof __project === 'function') __project(bars)
    const r = renderers.get('bands/bands')
    if (outputActive(r.output)) r.render(bars, useAggregateMode()); else r.clear()
}
const levelsHistorySegments = segmentHistorySegments
`

// Run the real application module with deterministic DOM/chart/RAF boundaries.
// This checks lifecycle work, not Safari's compositor or actual chart drawing.
function harness(checked = {}, options = {}) {
    const frames = new Map(), timers = new Map(), events = new Map()
    let nextId = 0
    const writes = []
    const series = []
    let range = { from: 0, to: 20 }
    const noop = () => {}
    const timeScale = {
        applyOptions: noop, fitContent: noop,
        getVisibleLogicalRange: () => ({ ...range }),
        setVisibleLogicalRange: value => { range = { ...value }; writes.push('range') },
        subscribeVisibleLogicalRangeChange: callback => events.set('chart:range', callback),
    }
    const chart = {
        applyOptions: noop, timeScale: () => timeScale,
        subscribeCrosshairMove: callback => events.set('chart:crosshair', callback),
        clearCrosshairPosition: () => { writes.push('clearCrosshair'); events.get('chart:crosshair')?.({}) },
        panes: () => Array.from({ length: 3 }, () => ({ setHeight: noop, getHeight: () => 100 })),
        resize: () => writes.push('resize'),
        removeSeries: item => { writes.push('removeSeries'); item.removed = true },
        addSeries: (type, options = {}, pane = 0) => { const item = {
            type, pane, options: { ...options }, priceLines: [], createPriceLine(line) { this.priceLines.push(line); return line },
            data: [], setData(data) { this.data = [...data]; writes.push('setData') }, update(point) { if (this.data.at(-1)?.time === point.time) this.data[this.data.length - 1] = point; else this.data.push(point); writes.push('update') },
            applyOptions(next) { Object.assign(this.options, next); writes.push('applyOptions') }, priceScale: () => ({ applyOptions: noop }),
        }; series.push(item); return item },
        // Custom series keep the real pane view so tests can drive its renderer.
        addCustomSeries: (view, options = {}, pane = 0) => { const item = chart.addSeries('Custom', { ...view.defaultOptions(), ...options }, pane); item.view = view; return item },
    }
    const element = (tag = 'div') => ({ tagName: tag, value: '', checked: false, disabled: false, hidden: false, type: '', id: '', style: {}, dataset: {}, textContent: '', title: '', className: '',
        children: [], attributes: {}, classList: { add() {}, toggle() {} },
        setAttribute(name, value) { this.attributes[name] = value },
        removeAttribute(name) { delete this.attributes[name] },
        append(...items) { this.children.push(...items) },
        replaceChildren(...items) { this.children = items },
        contains(node) { return node === this || this.children.some(c => typeof c === 'object' && c.contains?.(node)) },
        focus() { document.activeElement = this },
        getBoundingClientRect: () => ({ width: 900, height: 600 }), addEventListener: (event, callback) => events.set('element:' + event, callback) })
    const elements = Object.fromEntries([
        'exchangeSel', 'symbolSel', 'marketSel', 'tfSel', 'accessKeyInput',
        'aggToggle', 'pluginControlsEl', 'pluginMenuEl', 'pluginStateEl',
        'screenshotBtn', 'statusEl', 'qualityStateEl', 'sourceSeriesEl', 'chartEl', 'paneLabels',
    ].map(key => [key, element()]))
    Object.assign(elements.exchangeSel, { value: 'binance' })
    Object.assign(elements.symbolSel, { value: 'BTC' })
    Object.assign(elements.marketSel, { value: 'spot' })
    Object.assign(elements.tfSel, { value: '30m' })
    for (const [key, value] of Object.entries(checked)) elements[key].checked = value
    const listen = prefix => (event, callback) => events.set(prefix + event, callback)
    const document = { hidden: false, title: '', activeElement: null, documentElement: { style: { setProperty: noop } },
        addEventListener: listen('document:'), createElement: element }
    const context = vm.createContext({
        elements, document, console, URL, AbortController, DOMException,
        location: { origin: 'https://example.test', host: 'example.test', protocol: 'https:' },
        localStorage: { getItem: () => null, setItem: noop },
        performance: { now: () => 10000 },
        window: { matchMedia: () => ({ matches: false, addEventListener: noop }), addEventListener: listen('window:') },
        ResizeObserver: class { constructor(callback) { events.set('resize:chart', callback) } observe() {} },
        LightweightCharts: { createChart: () => chart, CrosshairMode: { Normal: 0 }, LineSeries: 'Line', CandlestickSeries: 'Candlestick',
            BaselineSeries: 'Baseline', HistogramSeries: 'Histogram', customSeriesDefaultOptions: { visible: true, lastValueVisible: true, priceLineVisible: true },
            LineStyle: { Dashed: 2 }, PriceScaleMode: { Normal: 0 } },
        createDualRangeHistogramSeries: (target, options, pane) => chart.addSeries('DualRange', options, pane),
        requestAnimationFrame: callback => { frames.set(++nextId, callback); return nextId },
        cancelAnimationFrame: id => frames.delete(id),
        setTimeout: callback => { timers.set(++nextId, callback); return nextId },
        clearTimeout: id => timers.delete(id),
        setInterval: callback => { timers.set(++nextId, callback); return nextId },
        clearInterval: id => timers.delete(id),
    })
    const read = file => fs.readFileSync(path.join(__dirname, '../static/js', file), 'utf8')
    const strip = code => code.replace(/^import .*\n/gm, '').replaceAll('export const', 'const').replaceAll('export async function', 'async function').replaceAll('export function', 'function')
    const source = read('app.js').replace(/^import .*\n/gm, '').replace(/\ninit\(\)\s*$/, '\n')
    const modules = ['config.js', 'quality-display.mjs', 'segmented-series.mjs', 'plugin-wire.mjs', 'plugin-store.mjs', 'legacy-prefs.mjs', 'plugin-registry.mjs', 'plugin-render.mjs', 'indicator-menu.mjs', 'time-labels.mjs', 'local-plugins.mjs', 'coordinator.mjs', 'local-compute.mjs'].map(file => strip(read(file))).join('\n')
    vm.runInContext(modules + '\n' + source + '\nconst realEnsureModeConnections = ensureModeConnections; ensureModeConnections = () => {};\n' + (options.projection === false ? '' : PROJECTION) + TEST_BRIDGE, context)
    if (options.descriptors !== null) vm.runInContext(`applyPluginDescriptors(${JSON.stringify(options.descriptors ?? TEST_DESCRIPTORS)})`, context)
    const run = code => vm.runInContext(code, context)
    function flushFrames() {
        let batches = 0
        while (frames.size) {
            assert.ok(++batches < 20, 'animation frame loop must settle')
            const batch = [...frames.values()]; frames.clear()
            batch.forEach(callback => callback())
        }
    }
    function emit(target, name, payload = {}) { return events.get(target + ':' + name)?.(payload) }
    flushFrames(); writes.length = 0
    return { run, document, writes, series, frames, timers, flushFrames, emit }
}

function bar(time, price = 100) { return ind(rawBar(time, price)) }
function rawBar(time, price = 100) {
    return ({ time, price_close: price, po: price, ph: price + 1, pl: price - 1,
        cvd_usdt: time, buy_volume_usdt: 6, sell_volume_usdt: 5, delta_usdt: 1,
        lt: [null, 110, 120, 130], lb: [null, 90, 80, 70] })
}
function seed(h) {
    h.run('modeConnectionsReady = true')
    h.run(`setBars(${JSON.stringify([bar(60), bar(120)])})`)
    h.flushFrames(); h.writes.length = 0
}


module.exports = { harness, bar, rawBar, ind, seed, flowRe, TEST_DESCRIPTORS, entry, out }
