// Regressions for the retired Strength pane (RD/KD/FD/HD). The default chart
// keeps the host Price pane; every other pane comes from plugin descriptors.
const { test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const vm = require('node:vm')

const read = file => fs.readFileSync(path.join(__dirname, '..', file), 'utf8')
const html = read('static/index.html')
const app = read('static/js/app.js')
const dom = read('static/js/dom.js')
const quality = read('static/js/quality-display.mjs')
const levels = read('static/js/segmented-series.mjs')

test('page has no Strength pane, RD/KD/FD/HD toggles or cycle readout', () => {
    for (const id of ['toggle-rd', 'toggle-kd', 'toggle-fd', 'toggle-hd', 'signal-state']) {
        assert.equal(html.includes(`id="${id}"`), false, id)
        assert.equal(dom.includes(`"${id}"`), false, id)
    }
    assert.doesNotMatch(html, />\s*Strength\s*</)
    assert.doesNotMatch(html, />\s*(RD|KD|FD|HD)\s*</)
    const labels = [...html.matchAll(/class="pane-label(?: [^"]*)?"/g)]
    assert.equal(labels.length, 1, 'only the host Price pane label is static; plugin pane labels are generated')
    // Retained host controls; indicator toggles/modes are generated from descriptors.
    for (const id of ['toggle-agg', 'plugin-controls', 'plugin-state', 'quality-state', 'source-series', 'screenshot-btn']) {
        assert.ok(html.includes(`id="${id}"`), id)
    }
    for (const id of ['toggle-levels', 'flow-mode', 'flow-summary']) assert.equal(html.includes(`id="${id}"`), false, `fixed ${id} removed`)
})

test('frontend source has no retired indicator renderers, keys or readiness helpers', () => {
    for (const name of ['rdToggle', 'kdToggle', 'fdToggle', 'hdToggle', 'signalStateEl', 'strengthRenderers',
        'distanceSeries', 'distanceZeroSeries', 'distancePlusTwoSeries', 'distanceMinusTwoSeries', 'setDistanceData',
        'updateDistancePoint', 'setDistanceVisibility', 'strengthKeySet', 'cycleKeySet', 'updateSignalState',
        'createIndicatorRenderer', 'indicatorSummary', 'showRd', 'showKd', 'showFd', 'showHd']) {
        assert.equal(new RegExp(`\\b${name}\\b`).test(app), false, `app.js ${name}`)
    }
    for (const name of ['createIndicatorRenderer', 'indicatorSummary', 'indicatorPoint', 'indicatorRuns', 'indicatorState']) {
        assert.equal(new RegExp(`\\b${name}\\b`).test(quality), false, `quality-display ${name}`)
    }
    assert.doesNotMatch(levels, /singletons|strokePerRun/)
    // Source-quality observation is retained and separate.
    assert.match(quality, /export function mergeHistoryQuality/)
    assert.match(quality, /export function qualityDisplay/)
})

test('changed frontend modules share one coherent import version', () => {
    const versions = new Set([...html.matchAll(/\.js\?v=([\w-]+)/g), ...app.matchAll(/\?v=([\w-]+)/g)].map(m => m[1]))
    assert.deepEqual([...versions], ['local-wasm-3'])
})

function section(start, end) {
    const from = app.indexOf(start), to = app.indexOf(end, from)
    assert.ok(from >= 0 && to > from, `${start}..${end}`)
    return app.slice(from, to)
}

test('stored legacy Strength preferences are pruned without resetting useful settings', () => {
    const select = options => {
        let allowed = options, value = options[0]
        return {
            get value() { return value }, set value(v) { value = allowed.includes(v) ? v : '' },
            set innerHTML(markup) { allowed = [...markup.matchAll(/value="([^"]+)"/g)].map(m => m[1]); value = allowed[0] || '' },
        }
    }
    const saved = {}
    const context = vm.createContext({
        console: { warn() {} },
        exchangeSel: select(['binance', 'coinbase']), marketSel: select(['spot', 'usdm', 'usdc']),
        symbolSel: select(['BTC', 'ETH']), tfSel: select(['1m', '5m', '15m', '30m', '1h']),
        sourceSeriesEl: { textContent: '', title: '' }, accessKeyInput: { value: '' },
        aggToggle: { checked: false }, pluginPrefs: { enabled: {}, modes: {} },
        localStorage: { setItem: (k, v) => { saved[k] = v } },
    })
    const config = read('static/js/config.js').replaceAll('export const', 'const').replaceAll('export function', 'function')
    const legacyModule = read('static/js/legacy-prefs.mjs').replaceAll('export function', 'function')
    vm.runInContext(config + '\n' + legacyModule + '\n' + section('const fallbackInstruments', 'function mkChart') + '\n' + section('function marketLabel', 'const chart =') + '\n' + section('function sanitizePluginPrefs', 'function pluginEnabled'), context)
    const run = code => vm.runInContext(code, context)
    const legacy = { exchange: 'coinbase', market: 'usdc', symbol: 'ETH', tf: '5m', accessKey: 'k', showRd: true, showKd: true,
        showFd: false, showHd: true, showLevels: false, showAgg: true, flowMode: 'buy-sell' }
    run(`updateOptionsForExchange(); applyPrefs(${JSON.stringify(legacy)}); prefsHydrated = true; savePrefs()`)
    const stored = JSON.parse(saved.cvd_prefs_v1)
    assert.deepEqual(stored, { exchange: 'coinbase', symbol: 'ETH', market: 'usdc', tf: '5m', accessKey: 'k',
        showAgg: true, plugins: { enabled: { levels: false }, modes: { 'flow:flow': 'flow/buy_sell' } } })
})
