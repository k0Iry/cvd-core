import { CONFIG, DEFAULT_TITLE, FALLBACK_EXCHANGE_META, SOURCE_INSTRUMENTS, sourceKey, sourceSeries, legacySource, validInstrument, STORAGE_KEY, THEMES } from "./config.js?v=local-wasm-1"
import { elements } from "./dom.js?v=local-wasm-1"
import * as LightweightCharts from "lightweight-charts"
import { createDualRangeHistogramSeries } from "@tradingview/lwc-plugin-dual-range-histogram-series"
import { mergeHistoryQuality, qualityDisplay } from "./quality-display.mjs?v=local-wasm-1"
import { createSegmentHistoryView, segmentData, segmentSlotData, sameSegmentData } from "./segmented-series.mjs?v=local-wasm-1"
import { parseIndicatorFrame, seriesValueAt } from "./plugin-wire.mjs?v=local-wasm-1"
import { openPluginDb, checkImportFile, classifyImport, sha256Hex, sameBytes, importMessage, sanitizeParams, validRecord, parseParamInput, contextBars } from "./local-plugins.mjs?v=local-wasm-1"
import { createLocalCompute } from "./local-compute.mjs?v=local-wasm-1"
import { createCoordinator } from "./coordinator.mjs?v=local-wasm-1"
import { WasmPluginHost } from "/wasm-host/host-bridge.js?v=local-wasm-1"
import { createIndicatorStore } from "./plugin-store.mjs?v=local-wasm-1"
import { migrateLegacyPrefs } from "./legacy-prefs.mjs?v=local-wasm-1"
import { activeGroupOutput, buildRegistry, readoutText } from "./plugin-registry.mjs?v=local-wasm-1"
import { createOutputRenderer } from "./plugin-render.mjs?v=local-wasm-1"
import { createIndicatorMenu } from "./indicator-menu.mjs?v=local-wasm-1"

const {
    exchangeSel,
    symbolSel,
    marketSel,
    tfSel,
    accessKeyInput,
    aggToggle,
    pluginControlsEl,
    pluginMenuEl,
    pluginStateEl,
    screenshotBtn,
    statusEl,
    qualityStateEl,
    sourceSeriesEl,
    chartEl,
    paneLabels,
} = elements

const fallbackInstruments = Object.values(SOURCE_INSTRUMENTS).filter(legacySource)
let availableInstruments = fallbackInstruments
let exchangeMeta = { ...FALLBACK_EXCHANGE_META }
let prefsHydrated = false
const available = (exchange, market, symbol) => availableInstruments.some(item =>
    item.exchange === exchange && item.market === market && item.symbol === symbol)
function availableMarkets(exchange, symbol) {
    return (exchangeMeta[exchange]?.markets || []).filter(market => available(exchange, market, symbol))
}
function availableSymbols(exchange, market) {
    return (exchangeMeta[exchange]?.symbols || []).filter(symbol => available(exchange, market, symbol))
}

function savePrefs() {
    if (!prefsHydrated) return
    const prefs = {
        exchange: exchangeSel.value,
        symbol: symbolSel.value,
        market: marketSel.value,
        tf: tfSel.value,
        accessKey: accessKeyInput.value,
        showAgg: aggToggle.checked,
        plugins: { enabled: { ...pluginPrefs.enabled }, modes: { ...pluginPrefs.modes } },
    }
    try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs))
    } catch (error) {
        console.warn("preferences could not be saved", error)
    }
}

function loadPrefs() {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return null
    try {
        return JSON.parse(raw)
    } catch {
        return null
    }
}

function mkChart(el) {
    return LightweightCharts.createChart(el, {
        autoSize: true,
        layout: { background: { color: "#111" }, textColor: "#ddd" },
        grid: { vertLines: { color: "#222" }, horzLines: { color: "#222" } },
        timeScale: {
            borderColor: "#333",
            timeVisible: true,      // ✅ 显示分钟
            secondsVisible: false   // ✅ 不显示秒
        },
        rightPriceScale: { borderColor: "#333" },
        crosshair: {
            mode: LightweightCharts.CrosshairMode.Normal,
            vertLine: { color: "#2f80ed33", width: 1, labelBackgroundColor: "#2f80ed" },
            horzLine: { color: "#2f80ed33", width: 1, labelBackgroundColor: "#2f80ed" }
        }
    })
}

function marketLabel(exchange, market) {
    if (market === "spot_usdc") return "USDC spot"
    if (market === "usdm_usdc") return "USDC linear perpetual"
    if (market === "usdm") return "USDT linear perpetual"
    if (market === "coinm") return "USD inverse perpetual"
    if (market === "usdc" && exchange === "coinbase") return "USDC DRB perpetual"
    return market === "spot" ? `${exchange === "coinbase" ? "USD" : "USDT"} spot` : market
}
function setSelectOptions(sel, options, preferValue) {
    sel.innerHTML = options.map(v => `<option value="${v}">${sel === marketSel ? marketLabel(exchangeSel.value, v) : v}</option>`).join("")
    if (preferValue && options.includes(preferValue)) sel.value = preferValue
    else sel.value = options[0]
}

function updateSourceSeriesLabel(note = "") {
    const descriptor = SOURCE_INSTRUMENTS[sourceKey(exchangeSel.value, marketSel.value, symbolSel.value)]
    const market = marketSel.value
    const name = marketLabel(exchangeSel.value, market)
    sourceSeriesEl.textContent = `${name}${market === "usdc" && exchangeSel.value === "coinbase" ? " · independent series" : ""}${note ? " · separate series · INTX retired" : ""}`
    sourceSeriesEl.title = `${exchangeSel.value} ${descriptor?.native_symbol ?? "unsupported"}; source quote turnover ${descriptor?.quote_asset ?? "unknown"}. New USDC sources have independent history; missing history is not zero or complete. Aggregate is nominal USD/USDT/USDC quote turnover without FX conversion; Coinbase INTX history is retired. ${note}`
}

function assertSourceSeries(payload, market, exchange = exchangeSel.value, symbol = symbolSel.value) {
    const expected = sourceSeries(market)
    if (payload?.series !== expected && !(market === "spot" && payload != null && !Object.prototype.hasOwnProperty.call(payload, "series"))) {
        throw new Error(`Source series mismatch: expected ${expected}`)
    }
    for (const [field, selected] of [["exchange", exchange], ["market", market], ["symbol", symbol]]) {
        if (Object.hasOwn(payload, field) && payload[field] !== selected) throw new Error(`Source identity mismatch: ${field}`)
    }
}

function updateOptionsForExchange(preserveContract = false) {
    const ex = exchangeSel.value
    const cfg = exchangeMeta[ex] || Object.values(exchangeMeta)[0] || FALLBACK_EXCHANGE_META.binance
    const prevMarket = marketSel.value
    const prevSymbol = symbolSel.value
    const preferredMarket = preserveContract && ex === "binance" && prevMarket === "usdc" ? "usdm"
        : preserveContract && ex === "coinbase" && ["usdm", "usdm_usdc"].includes(prevMarket) ? "usdc" : prevMarket
    const markets = availableMarkets(ex, prevSymbol)
    setSelectOptions(marketSel, markets.length ? markets : cfg.markets, preferredMarket)
    setSelectOptions(symbolSel, cfg.symbols, prevSymbol)
    updateSourceSeriesLabel()
}
function updateOptionsForSymbol() {
    setSelectOptions(marketSel, availableMarkets(exchangeSel.value, symbolSel.value), marketSel.value)
    updateSourceSeriesLabel()
}
function updateOptionsForMarket() {
    const symbols = exchangeMeta[exchangeSel.value]?.symbols || []
    setSelectOptions(symbolSel, symbols, availableSymbols(exchangeSel.value, marketSel.value).includes(symbolSel.value) ? symbolSel.value : availableSymbols(exchangeSel.value, marketSel.value)[0])
    updateSourceSeriesLabel()
}

function applyPrefs(prefs) {
    if (!prefs) return
    prefs = migrateLegacyPrefs(prefs)
    const preferredExchange = Object.prototype.hasOwnProperty.call(exchangeMeta, prefs.exchange)
        ? prefs.exchange : "binance"
    setSelectOptions(exchangeSel, Object.keys(exchangeMeta), preferredExchange)
    updateOptionsForExchange()
    const markets = exchangeMeta[exchangeSel.value].markets
    const retired = exchangeSel.value === "coinbase" && prefs.market === "usdm"
    setSelectOptions(symbolSel, exchangeMeta[exchangeSel.value].symbols, prefs.symbol)
    const preferredMarket = retired && available(exchangeSel.value, "usdc", symbolSel.value) ? "usdc" : prefs.market
    setSelectOptions(marketSel, availableMarkets(exchangeSel.value, symbolSel.value), preferredMarket)
    updateOptionsForMarket()
    tfSel.value = ["1m", "5m", "15m", "30m", "1h"].includes(prefs.tf) ? prefs.tf : "1m"
    if (typeof prefs.accessKey === "string") accessKeyInput.value = prefs.accessKey
    if (typeof prefs.showAgg === "boolean") aggToggle.checked = prefs.showAgg
    pluginPrefs = sanitizePluginPrefs(prefs.plugins)
    updateOptionsForExchange()
    if (retired) updateSourceSeriesLabel(marketSel.value === "usdc"
        ? "Retired Coinbase INTX usdm preference moved to DRB USDC; old history/CVD was not carried over."
        : "Retired Coinbase INTX usdm unavailable; DRB USDC not advertised. Spot is separate.")
}

async function loadMeta() {
    try {
        const response = await fetch("/meta", { cache: "no-store" })
        if (!response.ok) throw new Error(`meta ${response.status}`)
        const payload = await response.json()
        if (payload.contract_series !== "contracts-drb-v1") throw new Error("stale meta contract series")
        const nextMeta = {}
        const hasInstruments = Array.isArray(payload.instruments) && payload.spot_series === "spot-v1"
        const advertised = hasInstruments ? payload.instruments.filter(validInstrument) : fallbackInstruments
        const nextInstruments = []
        for (const item of payload.exchanges || []) {
            if (!Object.hasOwn(FALLBACK_EXCHANGE_META, item?.exchange)) continue
            const supported = FALLBACK_EXCHANGE_META[item.exchange]
            const markets = (Array.isArray(item.markets) ? item.markets : []).filter(v =>
                (supported.markets.includes(v) || (hasInstruments && ["spot_usdc", "usdm_usdc"].includes(v) && item.exchange === "binance")))
            const symbols = (Array.isArray(item.symbols) ? item.symbols : []).filter(v => supported.symbols.includes(v))
            const pairs = advertised.filter(instrument => instrument.exchange === item.exchange &&
                markets.includes(instrument.market) && symbols.includes(instrument.symbol))
            if (pairs.length) {
                nextInstruments.push(...pairs)
                nextMeta[item.exchange] = {
                    markets: [...new Set(pairs.map(pair => pair.market))],
                    symbols: [...new Set(pairs.map(pair => pair.symbol))],
                }
            }
        }
        if (Object.keys(nextMeta).length) {
            exchangeMeta = nextMeta
            availableInstruments = nextInstruments
        }
    } catch (err) {
        console.warn("meta load failed, using fallback config", err)
    }

    setSelectOptions(exchangeSel, Object.keys(exchangeMeta), exchangeSel.value)
}

const chart = mkChart(chartEl)
let paneHeightsInitialized = false
let currentThemeName = "night"

function applyTheme(themeName) {
    currentThemeName = THEMES[themeName] ? themeName : "night"
    const t = THEMES[currentThemeName]
    const root = document.documentElement.style
    root.setProperty("--bg", t.bg)
    root.setProperty("--panel", t.panel)
    root.setProperty("--border", t.border)
    root.setProperty("--text", t.text)
    root.setProperty("--muted", t.muted)
    root.setProperty("--accent", t.accent)
    root.setProperty("--accent-2", t.accent2)

    const chartOpts = {
        layout: { background: { color: t.chartBg }, textColor: t.text },
        grid: { vertLines: { color: t.grid }, horzLines: { color: t.grid } },
        rightPriceScale: { borderColor: t.grid },
        timeScale: { borderColor: t.grid }
    }
    chart.applyOptions(chartOpts)
}


chart.timeScale().applyOptions({ rightOffset: 2, minBarSpacing: 0.5 })

function onMediaQueryChange(mq, handler) {
    if (typeof mq.addEventListener === "function") {
        mq.addEventListener("change", handler)
    } else if (typeof mq.addListener === "function") {
        // Safari < 14 exposes MediaQueryList.addListener instead of EventTarget.
        mq.addListener(handler)
    }
}

// init theme & follow system color scheme
const mq = window.matchMedia("(prefers-color-scheme: dark)")
applyTheme(mq.matches ? "night" : "day")
onMediaQueryChange(mq, e => applyTheme(e.matches ? "night" : "day"))

const PRICE_UP_COLOR = "#27ae60"
const PRICE_DOWN_COLOR = "#eb5757"
const PRICE_FLAT_COLOR = "#8b94a7"
const CURRENT_PRICE_COLOR = "#f2994a"

const priceSeries = chart.addSeries(LightweightCharts.CandlestickSeries, {
    upColor: PRICE_UP_COLOR,
    downColor: PRICE_DOWN_COLOR,
    borderUpColor: PRICE_UP_COLOR,
    borderDownColor: PRICE_DOWN_COLOR,
    wickUpColor: PRICE_UP_COLOR,
    wickDownColor: PRICE_DOWN_COLOR,
    priceLineColor: CURRENT_PRICE_COLOR,
    priceLineWidth: 2,
    priceLineStyle: LightweightCharts.LineStyle.Dashed,
    priceLineVisible: true,
    lastValueVisible: true,
}, 0)
// Host-owned stable timeline backbone: one hidden native series in the price
// pane holding a hidden numerical-zero point for EVERY cached timestamp.
// Whitespace-only points do NOT anchor LWC's base/fulfilled logical index when
// the rendered data window slides (viewport jumps); a hidden value:0 point
// does (same invariant as the old deltaZeroSeries). The zero is internal
// time-axis scaffolding only, never an indicator value: invisible, no
// label/marker, excluded from autoscale. It never belongs
// to a removable plugin, so chart logical indices stay cache indices whether or
// not any indicator is installed/visible.
const backboneSeries = chart.addSeries(LightweightCharts.LineSeries, {
    visible: false,
    priceLineVisible: false,
    lastValueVisible: false,
    crosshairMarkerVisible: false,
    autoscaleInfoProvider: () => null,
}, 0)

// ------------------------------------------------------------ indicator plugins
let registry = buildRegistry([])
let renderers = new Map()          // output key -> renderer
let pluginPrefs = { enabled: {}, modes: {} }
let discoveryError = ""
let paneSummaryEls = []
let pluginLayoutGeneration = 0
const indicatorStore = createIndicatorStore()
const currentMode = () => useAggregateMode() ? "aggregate" : "source"
// Renderers read results by canonical bar time; base bars are never modified.
// Browser-computed results ONLY (server indicator outputs are ignored):
// `${outputKey}|${mode}` -> Map(time -> value), replaced atomically per
// accepted (scope, data revision) compute of that plugin.
let localValues = new Map()
let localLive = new Map()   // live (op2/op3) values overlaying stateless History
const storeValue = (bar, key, aggregate) => bar ? localValues.get(`${key}|${aggregate ? "aggregate" : "source"}`)?.get(bar.time) : undefined
// Newest live base WS `rev` per bar lives in the store, scoped to the
// connection epoch and pruned only with the canonical cache (no LRU): a value
// is current only if its as_of_rev >= that rev, otherwise pending.
function noteBaseRev(payload) { return indicatorStore.noteBaseRev(payload?.bar?.time, payload?.rev) }
const renderDeps = { LightweightCharts, createDualRangeHistogramSeries, outputValue: storeValue,
    segmentData, segmentSlotData, sameSegmentData, createSegmentHistoryView }

function sanitizePluginPrefs(raw) {
    const clean = { enabled: {}, modes: {} }
    for (const kind of ["enabled", "modes"]) {
        const source = raw?.[kind]
        if (!source || typeof source !== "object" || Array.isArray(source)) continue
        for (const [key, value] of Object.entries(source).slice(0, 64)) {
            if (kind === "enabled" && typeof value === "boolean") clean.enabled[key] = value
            if (kind === "modes" && typeof value === "string" && value.length <= 140) clean.modes[key] = value
        }
    }
    return clean
}

function pluginEnabled(plugin) {
    return localRecords.get(plugin.id)?.enabled === true
}

// Latest result status for the current mode (history page or live frame).
// Any fault clears and labels the outputs; nothing stale stays drawn.
function pluginFault(plugin) {
    const fault = localCompute?.fault(plugin.id)
    return fault ? { state: "fault", code: fault.code } : (localErrors.get(plugin.id) ?? null)
}

function faultText(fault) {
    return `${fault.state ?? "unavailable"}${fault.code ? " " + fault.code : ""}`
}

// Active = enabled, healthy, drawable, and selected in its exclusive group.
function outputActive(output) {
    if (output.readoutOnly) return false
    const plugin = registry.plugins.find(p => p.id === output.pluginId)
    if (!plugin || !pluginEnabled(plugin) || pluginFault(plugin)) return false
    if (!output.groupKey) return true
    const group = registry.panes[paneIndexFor(output)].groups.get(output.groupKey)
    return activeGroupOutput(group, pluginPrefs.modes[group.key]) === output
}

function paneIndexFor(output) {
    return registry.panes.find(p => p.id === output.paneId).index
}

function forEachRenderer(callback) {
    for (const renderer of renderers.values()) callback(renderer)
}

// Replace the whole registry only when the descriptor signature changes;
// renderers for removed/changed plugins are destroyed so nothing stale stays.
// readout_only outputs get no renderer (never drawn, no native series).
function applyPluginDescriptors(entries, error = "") {
    discoveryError = error
    const next = buildRegistry(entries)
    if (next.signature === registry.signature) {
        registry = next
        renderPluginControls()
        return false
    }
    forEachRenderer(renderer => renderer.destroy())
    renderers = new Map()
    registry = next
    for (const pane of registry.panes) {
        for (const output of pane.outputs) {
            if (!output.readoutOnly) renderers.set(output.key, createOutputRenderer(chart, renderDeps, output, pane.index))
        }
    }
    pluginLayoutGeneration += 1
    paneHeightsInitialized = false
    renderedMode = null
    renderPluginControls()
    syncPaneLayout()
    if (page.bars.length) queueChartRender(currentChartRange())
    return true
}

// ------------------------------------------------- local WASM plugin packages
// Only locally imported, validated packages (IndexedDB, this browser only)
// have rows. Nothing is uploaded; there is no server discovery or fallback.
let localDb = null
let localHost = null
let localCompute = null
const localRecords = new Map()     // id -> persisted record
const localErrors = new Map()      // id -> { state, code } non-compute errors (storage/load)
let importNotice = { kind: "", text: "", seq: 0, op: null }
// Registry operations (import / uninstall / params) are serialized: each runs
// to completion (storage included) before the next starts, so a late removal
// can never interleave with, or be mistaken for, a later import.
let registryQueue = Promise.resolve()
let registryOpSeq = 0
let currentOp = null
function registryOp(op, fn) {
    const run = registryQueue.then(() => { currentOp = { ...op, seq: ++registryOpSeq }; return fn() }).finally(() => { currentOp = null })
    registryQueue = run.catch(() => {})
    return run
}
let dataRevision = 0
let probeInstance = 1 << 30

function scopeNow() {
    return { exchange: exchangeSel.value, market: marketSel.value, symbol: symbolSel.value, tf: tfSel.value, mode: currentMode() }
}
// Shared canonical cache changed (or scope/mode): one demand for all plugins.
// kind "tail" = ordinary live tick (append/replace of the newest bars, served
// by LiveUpdate); anything else changes the history basis (load/prepend).
let historyRevision = 0
let localScopeKey = ""
let lastDemandBasis = null
function noteDataChanged(kind = "history") {
    // Metadata-only notifications (labels/health relabel, same scope, same
    // canonical array and exhaustion) are not a data change: no revision bump,
    // no History recompute, live overlay stays authoritative.
    const basis = { bars: page.bars, len: page.bars.length, exhausted: !!page.reachedStart, scopeKey: JSON.stringify(scopeNow()) }
    if (kind === "meta" && lastDemandBasis && lastDemandBasis.bars === basis.bars && lastDemandBasis.len === basis.len &&
        lastDemandBasis.exhausted === basis.exhausted && lastDemandBasis.scopeKey === basis.scopeKey) return
    lastDemandBasis = basis
    dataRevision += 1
    if (kind !== "tail") historyRevision += 1   // "meta" reaching here = real scope/mode/cache change
    const scope = scopeNow()
    const scopeKey = JSON.stringify(scope)
    if (localScopeKey && scopeKey !== localScopeKey) {
        // Displayed outputs are bound to the accepted scope: never reuse
        // another scope's values for matching timestamps.
        if (localValues.size) { localValues = new Map(); localLive = new Map(); forEachRenderer(r => { r.dirty = true }) }
    }
    localScopeKey = scopeKey
    if (!localCompute) return
    localCompute.setDemand({ scope, scopeKey, revision: dataRevision, historyRevision, bars: page.bars,
        invalidTimes: [...invalidPoison.keys()].sort((a, b) => a - b),
        exhausted: !!page.reachedStart, tailSnap: page.bars.slice(-64), ...canonicalRequestedRange(page.bars) })
}
// Canonical requested History range for local plugins (old PUBLIC server
// page basis): the start is the oldest bar of the pages the app requested for
// DISPLAY (initial page + user-driven older pages); warm-up pages are context only.
// It never follows a moving viewport/cache index, so window-origin-sensitive
// computations keep their origin. The explicit end is the cache end. Context
// comes from the ACTUAL cached prefix before the start (Rust planner).
let localRequestedFrom = null
// User-driven older pages, each its OWN producer request basis: [from, to]
// times (to = the bar before the newer page it was requested before). A
// bounded older page is computed with its own origin + actual prefix and
// right end; previously computed newer pages are never re-anchored.
let localOlderPages = []
let warmupPrepending = false
function lowerBound(bars, time) {
    let lo = 0, hi = bars.length
    while (lo < hi) { const mid = (lo + hi) >> 1; if (bars[mid].time < time) lo = mid + 1; else hi = mid }
    return lo
}
function canonicalRequestedRange(bars) {
    const n = bars.length
    if (!n) return { wantFromIndex: 0, wantToIndex: 0, olderRanges: [] }
    if (localRequestedFrom == null) localRequestedFrom = bars[0].time
    const olderRanges = localOlderPages.map(({ from, to }) => ({ fromIndex: lowerBound(bars, from), toIndex: lowerBound(bars, to + 1), from, to }))
        .filter(r => r.toIndex > r.fromIndex)
    return { wantFromIndex: lowerBound(bars, localRequestedFrom), wantToIndex: n, olderRanges }
}


// Invalidate derived values at bar times whose canonical input is about to
// change, so the new base is never painted with an old derived value.
function invalidateLocalAt(times) {
    for (const col of localValues.values()) for (const t of times) col.delete(t)
    for (const col of localLive.values()) for (const t of times) col.delete(t)
    // Fail closed in the interval before the coordinator re-plans. Context is
    // WARM-UP, not a finite influence bound (an output may carry state from
    // the window's first input, and per_tf overrides may exceed the default),
    // so conservatively withhold EVERY admitted output of an enabled plugin at
    // or after the earliest invalidated bucket. Outputs strictly before it
    // cannot have consumed it. The Rust coordinator then re-plans only windows
    // that do not span the bucket (substantive window policy stays in Rust).
    if (!times.length) return
    const from = Math.min(...times)
    for (const [id, record] of localRecords) {
        if (!record?.enabled) continue
        for (const map of [localValues, localLive]) for (const [key, col] of map) {
            if (!key.startsWith(`${id}/`)) continue
            for (const tm of [...col.keys()]) if (tm >= from) col.delete(tm)
        }
    }
}

function syncLocalCompute() {
    if (!localCompute) return
    localCompute.setPlugins([...localRecords.values()].filter(r => r.enabled).map(r => ({
        id: r.id, bytes: r.bytes, params: r.params, loadKey: `${r.sha256}|${JSON.stringify(r.params)}`,
    })))
}

const localCoverage = new Map()
function dropLocalValues(id) {
    localCoverage.delete(id)
    for (const key of [...localLive.keys()]) if (key.startsWith(`${id}/`)) localLive.delete(key)
    for (const key of [...localValues.keys()]) if (key.startsWith(`${id}/`)) localValues.delete(key)
}

const sameValue = (a, b) => a === b || (a !== null && b !== null && typeof a === "object" && typeof b === "object" && JSON.stringify(a) === JSON.stringify(b))

// Earliest bar time whose canonical input differs between an older demand's
// bars and the current cache (tail replacements/appends); Infinity if equal.
const barKey = b => b && JSON.stringify(b)
function changedFromTime(oldBars, bars) {
    if (!oldBars) return -Infinity   // unknown basis: present nothing
    let i = oldBars.length - 1, j = bars.length - 1, from = Infinity
    while (i >= 0 && j >= 0) {
        const a = oldBars[i], b = bars[j]
        if (a.time > b.time) { from = Math.min(from, a.time); i--; continue }
        if (b.time > a.time) { from = Math.min(from, b.time); j--; continue }
        if (a !== b && barKey(a) !== barKey(b)) from = a.time
        i--; j--
    }
    return from
}

function onLocalResult(id, mode, output, want, meta = {}) {
    if (!localRecords.get(id)?.enabled) return
    if (want && want.scopeKey !== localScopeKey) return
    // Data-basis fence: output computed on an older history basis (before a
    // prepend/load/correction) must never refill invalidated values.
    if (want && want.historyRevision !== undefined && want.historyRevision !== historyRevision) return
    // Data-revision presentation fence: a successful compute on an older
    // data revision (same history basis, later tail replace) still advances
    // the plugin's session, but its values at times whose canonical input has
    // since changed are never painted (no stale intermediate paint).
    const staleFrom = want && want.revision !== undefined && want.revision !== dataRevision ? changedFromTime(want.tailSnap, page.bars) : Infinity
    const fresh = time => time < staleFrom
    if (meta.coverage) localCoverage.set(id, { coverage: meta.coverage, needBefore: meta.needBefore ?? 0 })
    const live = meta.kind === "live"
    const next = live ? new Map(localValues) : new Map([...localValues].filter(([key]) => !(key.startsWith(`${id}/`) && key.endsWith(`|${mode}`))))
    // Changed times vs the previously accepted result: unchanged recomputes
    // (the common live-tick case) cost no chart writes; tail-only changes use
    // the incremental per-bar path; anything else re-renders the outputs.
    const changed = new Set()
    let shapeChanged = false
    for (const [key, series] of Object.entries(output.series || {})) {
        const k = `${id}/${key}|${mode}`
        const prev = localValues.get(k)
        let overlay = localLive.get(k)
        // The live overlay is only authoritative on the basis it was built on;
        // a newer corrected History supersedes an overlay from an older basis.
        if (overlay && overlay.basis !== historyRevision) { localLive.delete(k); overlay = undefined }
        if (live && !overlay) { localLive.set(k, overlay = new Map()); overlay.basis = historyRevision }
        if (live && prev) {
            // Bounded delta: mutate only the touched times of the live column.
            output.times.forEach((time, i) => {
                if (!fresh(time)) return
                const v = seriesValueAt(series, i)
                if (!(prev.has(time) && sameValue(prev.get(time), v))) changed.add(time)
                if (v === undefined) { prev.delete(time); overlay.delete(time) } else { prev.set(time, v); overlay.set(time, v) }
            })
            while (overlay.size > 1024) overlay.delete(overlay.keys().next().value)
            next.set(k, prev)
            continue
        }
        const column = new Map()
        output.times.forEach((time, i) => {
            if (!fresh(time)) return
            const v = seriesValueAt(series, i)
            if (live) { if (v !== undefined) { column.set(time, v); overlay.set(time, v) } }
            else if (v !== undefined) column.set(time, v)
        })
        // Partial-range merge: valid cached values outside the newly computed
        // canonical range are preserved (never blanked by a bounded recompute).
        if (!live && prev && meta.rangeFrom != null) for (const [t, v] of prev) if ((t < meta.rangeFrom || t > meta.rangeTo) && !column.has(t)) column.set(t, v)
        // Stateless History never overwrites the persistent live session's values.
        if (!live && overlay) for (const [t, v] of overlay) column.set(t, v)
        if (!prev) shapeChanged = true
        else {
            for (const [t, v] of column) if (!prev.has(t) || !sameValue(prev.get(t), v)) changed.add(t)
            for (const t of prev.keys()) if (!column.has(t)) changed.add(t)
        }
        next.set(k, column)
    }
    if (!live) for (const key of localValues.keys()) if (key.startsWith(`${id}/`) && key.endsWith(`|${mode}`) && !next.has(key)) shapeChanged = true
    localValues = next
    localErrors.delete(id)
    if (shapeChanged) {
        forEachRenderer(renderer => { if (renderer.output.pluginId === id) renderer.dirty = true })
        refreshPluginOutputs(new Set([id]), true)
    } else {
        // Only changes inside the rendered window need chart writes; values
        // outside it are read by the next window render (prepend/warm-up
        // recomputes then cost nothing while the user drags/zooms).
        const lo = renderedWindow.bars[0]?.time, hi = renderedWindow.bars.at(-1)?.time
        const visible = visualWorkSuspended() || lo === undefined ? [...changed] : [...changed].filter(t => t >= lo && t <= hi)
        if (visible.length) refreshPluginOutputs(new Set([id]), false, visible)
        else updateFlowReadout()
    }
    maybeWarmup()
}

function onLocalStatus(id, st) {
    if (st.state === "fault") { dropLocalValues(id); refreshPluginOutputs(new Set([id]), true) }
    else { renderPluginState(); renderIndicatorMenu() }
    if (typeof document !== "undefined" && document.body) document.body.dataset.localPlugins = JSON.stringify(localCompute?.running() ?? [])
}

// Warm-up beyond the visible page: when an enabled plugin declares more
// context than the cache holds before the oldest bar, prepend older history
// through the ONE shared history path (never a per-plugin feed).
let warmupTimer = null
function maybeWarmup() {
    if (page.reachedStart || !page.bars.length) return
    let need = 0
    for (const [id, c] of localCoverage) if (localRecords.get(id)?.enabled && c.coverage !== "ready") need = Math.max(need, c.needBefore || 1)
    if (need <= 0) return
    if (page.loadingOlder || Date.now() - lastBeforeFetchAt < CONFIG.FETCH_COOLDOWN) {
        // Bounded continuation: a quiet feed must not strand warm-up.
        if (!warmupTimer) warmupTimer = setTimeout(() => { warmupTimer = null; maybeWarmup() }, CONFIG.FETCH_COOLDOWN + 50)
        return
    }
    warmupPrepending = true
    loadMoreBefore()
}

function registryFromRecords() {
    applyPluginDescriptors([...localRecords.values()].map(r => ({ id: r.id, status: "ready", descriptor: r.descriptor, error: null })), localUnavailable)
}

async function persistRecord(record) {
    try { await localDb.put(record) } catch (error) {
        setImportNotice("error", `${record.name}: settings could not be saved (${importMessage(error).text})`)
    }
}

function setImportNotice(kind, text) {
    importNotice = { kind, text, seq: importNotice.seq + 1, op: currentOp }
    renderIndicatorMenu()
}

let localInit = null
let localUnavailable = ""   // sticky: host/storage start failure (actionable, no server fallback)
function loadIndicators() { return localInit ??= initLocalPlugins() }
async function initLocalPlugins() {
    try {
        const hostBytes = await (await fetch("/wasm-host/cvd_browser_host.wasm", { cache: "no-cache" })).arrayBuffer()
        localHost = await WasmPluginHost.create(hostBytes, { workerUrl: new URL("/wasm-host/plugin-worker.js", location.origin) })
        const coordBytes = await (await fetch("/wasm-host/cvd_browser_coordinator.wasm", { cache: "no-cache" })).arrayBuffer()
        const coord = await createCoordinator(coordBytes)
        localCompute = createLocalCompute({ host: localHost, coord, onResult: onLocalResult, onStatus: onLocalStatus, onInvalid: onLocalInvalid, log: (...a) => console.warn(...a) })
    } catch (error) {
        console.warn("local plugin host unavailable", error)
        localUnavailable = "local plugin host unavailable (unsupported or failed to load)"
    }
    try {
        localDb = await openPluginDb()
        let invalidRestored = 0
        for (const r of await localDb.all()) {
            if (!validRecord(r)) { invalidRestored++; continue }
            r.params = sanitizeParams(r.descriptor, r.params)
            localRecords.set(r.id, r)
            committedEnabled.set(r.id, r.enabled === true)
        }
        if (invalidRestored) setImportNotice("error", `${invalidRestored} stored plugin record(s) were invalid and skipped`)
    } catch (error) {
        localUnavailable ||= `local plugins unavailable: ${importMessage(error).text}`
    }
    window.__cvdLocal = { compute: () => localCompute, host: () => localHost, values: () => localValues, records: () => localRecords, revision: () => dataRevision }
    registryFromRecords()
    syncLocalCompute()
    // Seed demand from an already-loaded page (quiet feed after late init).
    if (page.bars.length) noteDataChanged()
}

// Import pipeline: name/size -> bytes -> hash -> bounded Worker load (Rust
// policy check BEFORE compile, compile/instantiate/describe/create under the
// deadline) -> displayable descriptor -> duplicate/version decision -> store.
function importLocalFile(file) { return registryOp({ type: "import", file: String(file?.name ?? "") }, () => importLocalFileNow(file)) }
async function importLocalFileNow(file) {
    if (!localHost || !localDb) { setImportNotice("error", `Import unavailable: ${localUnavailable || "local plugin host or storage failed to start"} — price data still works; reload the page to retry (this browser may not support WebAssembly Workers or IndexedDB)`); return }
    setImportNotice("busy", `Checking ${file?.name ?? "file"}…`)
    const probe = probeInstance++
    try {
        checkImportFile(file)
        const bytes = await file.arrayBuffer()
        const sha256 = await sha256Hex(bytes)
        const { descriptor } = await localHost.load(probe, bytes, "{}")
        localHost.disable(probe)
        const desc = typeof descriptor === "string" ? JSON.parse(descriptor) : descriptor
        const check = buildRegistry([{ id: desc?.id, status: "ready", descriptor: desc }])
        if (!check.plugins.length) throw Object.assign(new Error(check.rejected[0]?.message ?? "invalid"), { code: "invalid_descriptor" })
        const decision = classifyImport([...localRecords.values()], { id: desc.id, sha256 })
        if (decision.action === "same" && sameBytes(decision.previous.bytes, bytes)) { setImportNotice("info", `${desc.name || desc.id} ${desc.version} is already imported`); return }
        const previous = decision.previous
        // Replacement: validate the ACTUAL migrated params against the new
        // package (plugin create) BEFORE the durable swap; on rejection the
        // previous package stays installed and running unchanged.
        const migrated = sanitizeParams(desc, previous?.params)
        if (previous) {
            const check2 = probeInstance++
            try { await localHost.load(check2, bytes, JSON.stringify(migrated)) }
            catch (error) { throw Object.assign(new Error("replacement rejects current parameters"), { code: error?.code === "plugin_error" || error?.code === "invalid_params" ? "replace_params" : error?.code }) }
            finally { try { localHost.disable(check2) } catch (_) {} }
        }
        const record = {
            id: desc.id, version: String(desc.version), name: String(desc.name || desc.id), sha256, size: bytes.byteLength,
            fileName: file.name.slice(0, 120), bytes, descriptor: desc,
            params: migrated, enabled: previous ? previous.enabled : false, importedAt: previous?.importedAt ?? Date.now(),
        }
        const writtenEnabled = record.enabled === true
        await localDb.put(record)
        // A toggle issued while this put was in flight changed the OLD
        // record's in-memory intent; carry it over (its own queued toggle op
        // persists it next). Durable state is what was just written.
        committedEnabled.set(record.id, writtenEnabled)
        const live = localRecords.get(record.id)
        if (previous && live) record.enabled = live.enabled
        if (!previous || !live) { incarnations.delete(record.id); incarnationOf(record.id) }
        // Replace atomically: old instance stopped/released before the new
        // package becomes visible; old results can never paint.
        dropLocalValues(record.id)
        localCompute?.clearFault(record.id)
        localErrors.delete(record.id)
        localRecords.set(record.id, record)
        registryFromRecords()
        syncLocalCompute()
        setImportNotice("ok", previous ? `Replaced ${record.name} ${previous.version} → ${record.version}` : `Imported ${record.name} ${record.version} (disabled — tick to enable)`)
    } catch (error) {
        try { localHost.disable(probe) } catch (_) {}
        const msg = importMessage(error)
        setImportNotice("error", `${file?.name ?? "file"}: ${msg.text}`)
    }
}

function uninstallLocalPlugin(id) { return registryOp({ type: "uninstall", id }, () => uninstallLocalPluginNow(id)) }
async function uninstallLocalPluginNow(id) {
    const record = localRecords.get(id)
    if (!record) return
    // Durable delete first: on storage failure the row stays (truthful state).
    try { await localDb.delete(id) } catch (error) { setImportNotice("error", `${record.name}: removal not saved (${importMessage(error).text})`); return }
    localRecords.delete(id)
    incarnations.delete(id); latestToggle.delete(id); committedEnabled.delete(id)
    dropLocalValues(id)
    syncLocalCompute()
    registryFromRecords()
    setImportNotice("ok", `Removed ${record.name}`)
}

function setLocalParams(id, values) { return registryOp({ type: "params", id }, () => setLocalParamsNow(id, values)) }
async function setLocalParamsNow(id, values) {
    const record = localRecords.get(id)
    if (!record) return
    const params = sanitizeParams(record.descriptor, values)
    // Durable write FIRST: on storage failure the running generation, its
    // values and the stored params stay as they were (no false success).
    // Immutable write snapshot: a toggle may mutate `record` while this put is
    // pending; the durable cache must reflect what was actually written.
    const payload = { ...record, params }
    const writtenEnabled = payload.enabled === true
    try { await localDb.put(payload) } catch (error) {
        setImportNotice("error", `${record.name}: parameters not saved (${importMessage(error).text}) — previous parameters kept`)
        return
    }
    record.params = params
    committedEnabled.set(id, writtenEnabled)
    dropLocalValues(id)
    localCompute?.clearFault(id)
    syncLocalCompute()
    refreshPluginOutputs(new Set([id]), true)
    setImportNotice("ok", `${record.name}: parameters saved`)
}

function pluginStateSummary() {
    const notes = []
    if (discoveryError) notes.push(`Indicators unavailable: ${discoveryError}`)
    for (const item of registry.pending) notes.push(`${item.id} ${item.status}${item.error?.code ? " " + item.error.code : ""}`)
    for (const item of registry.rejected) notes.push(`${item.id} rejected: ${item.message}`)
    for (const plugin of registry.plugins) {
        const fault = pluginFault(plugin)
        if (fault && pluginEnabled(plugin)) notes.push(`${plugin.label} ${faultText(fault)}`)
    }
    return notes
}

function renderPluginState() {
    if (!pluginStateEl) return
    const notes = pluginStateSummary()
    setText(pluginStateEl, "textContent", notes.length ? `${notes.length} indicator issue${notes.length > 1 ? "s" : ""}` : "")
    setText(pluginStateEl, "title", notes.join("\n"))
    setText(pluginStateEl.dataset, "state", notes.length ? "attention" : "quiet")
}

// Toolbar indicator multiselect: one trigger + popup of checkbox rows (one per
// ready plugin plus disabled placeholders for configured-but-not-ready ones),
// generated from discovery and reconciled in place across rerenders.
let indicatorMenu = null
function renderPluginControls() {
    renderPluginState()
    renderIndicatorMenu()
    renderPaneLabels()
}

function renderIndicatorMenu() {
    if (!pluginControlsEl || !pluginMenuEl || typeof document.createElement !== "function") return
    indicatorMenu ??= createIndicatorMenu({ container: pluginControlsEl, popup: pluginMenuEl, onToggle: setPluginEnabled,
        onImport: file => importLocalFile(file), onUninstall: id => uninstallLocalPlugin(id), onParams: (id, values) => setLocalParams(id, values),
        parseParam: parseParamInput })
    const items = [
        ...registry.plugins.map(plugin => {
            const fault = pluginFault(plugin)
            const record = localRecords.get(plugin.id)
            const state = localCompute?.runtime(plugin.id)?.state
            const live = pluginEnabled(plugin) && !fault && state === "starting" ? " · starting" : ""
            return { id: plugin.id, name: plugin.label, meta: fault ? `${plugin.version} · ${faultText(fault)}` : `${plugin.version}${live}`,
                title: `${plugin.label} ${plugin.version}${plugin.description ? " — " + plugin.description : ""} · local file ${record?.fileName ?? ""}${fault ? ` (${faultText(fault)})` : ""}`,
                checked: pluginEnabled(plugin), disabled: false, fault: !!fault,
                params: (record?.descriptor?.params ?? []).map(spec => ({ ...spec, value: record.params?.[spec.key] })) }
        }),
    ]
    indicatorMenu.render(items, registry.plugins.filter(pluginEnabled).length, importNotice)
}

// Pane labels: one per pane; exclusive groups get a generic <select>.
function renderPaneLabels() {
    if (!paneLabels || typeof document.createElement !== "function") return
    paneSummaryEls = []
    paneLabels.replaceChildren(...registry.panes.map(pane => {
        const el = document.createElement("div")
        el.className = "pane-label"
        el.dataset.pane = pane.id
        if (pane.id === "price") { el.textContent = "Price"; return el }
        el.classList.add("plugin-pane-label")
        const title = document.createElement("span")
        title.className = "pane-title"
        title.textContent = pane.label
        el.append(title)
        for (const group of pane.groups.values()) {
            const select = document.createElement("select")
            select.setAttribute("aria-label", `${group.label} mode`)
            select.dataset.group = group.key
            for (const output of group.outputs) {
                const option = document.createElement("option")
                option.value = output.key
                option.textContent = output.label
                select.append(option)
            }
            select.value = activeGroupOutput(group, pluginPrefs.modes[group.key]).key
            select.onchange = () => setGroupMode(group.key, select.value)
            el.append(select)
        }
        const summary = document.createElement("span")
        summary.className = "pane-summary"
        summary.dataset.paneSummary = pane.id
        paneSummaryEls.push(summary)
        el.append(summary)
        return el
    }))
    updateFlowReadout()
}

// Toggle applies immediately (stop/release on disable is never delayed by
// storage) and the durable write is serialized through the registry queue.
// Each toggle is an explicit operation {seq, id, incarnation, requested}: the
// write persists THAT operation's requested value onto the CURRENT record of
// the same incarnation (so a replacement swapped in meanwhile keeps the
// user's latest intent and an uninstalled/reimported plugin is never
// resurrected). On storage failure the toggle is reverted only if it is still
// the latest intent for that incarnation (generation fence, not boolean
// equality); a superseded failure never overrides newer intent. Every op ends
// in a receipt: pending -> committed (IDB transaction complete) | failed |
// superseded | dropped. The UI never claims durable success before commit.
const toggleReceipts = []
const latestToggle = new Map()     // id -> seq of latest toggle intent
const committedEnabled = new Map() // id -> last durably stored enabled
const incarnations = new Map()     // id -> token (kept by replacement, new on import/uninstall)
let incarnationSeq = 0
function incarnationOf(id) { if (!incarnations.has(id)) incarnations.set(id, ++incarnationSeq); return incarnations.get(id) }
let toggleSeq = 0
function setPluginEnabled(pluginId, enabled) {
    const record = localRecords.get(pluginId)
    if (!record || record.enabled === enabled) return
    const receipt = { seq: ++toggleSeq, id: pluginId, inc: incarnationOf(pluginId), requested: enabled, state: "pending" }
    toggleReceipts.push(receipt)
    if (toggleReceipts.length > 200) toggleReceipts.splice(0, toggleReceipts.length - 200)
    latestToggle.set(pluginId, receipt.seq)
    applyEnabled(record, enabled)
    return registryOp({ type: "toggle", id: pluginId, seq: receipt.seq }, async () => {
        const current = () => incarnations.get(pluginId) === receipt.inc ? localRecords.get(pluginId) : undefined
        let target = current()
        if (!localDb || !target) { receipt.state = "dropped"; return }
        try {
            await localDb.put({ ...target, enabled })
            committedEnabled.set(pluginId, enabled)
            receipt.state = "committed"
            // Same-plugin stale toggle failure no longer describes the current
            // outcome once a later toggle commits; other receipts untouched.
            if (importNotice.kind === "error" && importNotice.op?.type === "toggle" && importNotice.op.id === pluginId && latestToggle.get(pluginId) === receipt.seq)
                setImportNotice("ok", `${record.name}: ${enabled ? "enabled" : "disabled"} (saved)`)
        } catch (error) {
            target = current()
            if (latestToggle.get(pluginId) === receipt.seq && target) {
                receipt.state = "failed"
                const stored = committedEnabled.get(pluginId) ?? !enabled
                if (target.enabled !== stored) applyEnabled(target, stored)
                setImportNotice("error", `${record.name}: ${enabled ? "enable" : "disable"} not saved (${importMessage(error).text}) — reverted`)
            } else {
                receipt.state = "superseded"
            }
        }
    })
}
function applyEnabled(record, enabled) {
    record.enabled = enabled
    // Disable = stop compute and release the Worker/instance now; row, file
    // and params stay. Re-enable after a fault is the explicit user retry.
    localCompute?.clearFault(record.id)
    if (!enabled) dropLocalValues(record.id)
    syncLocalCompute()
    applyPluginVisibility()
}

function setGroupMode(groupKey, outputKey) {
    pluginPrefs.modes[groupKey] = outputKey
    applyPluginVisibility()
}

// Toggle/mode changes: no data reload. Hidden or page-leaving tabs defer the
// chart writes to the next visible paint.
function applyPluginVisibility() {
    updateFlowReadout()
    renderPluginState()
    renderIndicatorMenu()
    savePrefs()
    if (visualWorkSuspended()) {
        visualOptionsPending = true
        chartNeedsRender = true
        return
    }
    syncPluginSeries(renderedWindow.bars, false)
}

// Brings every renderer in line with its active state. Outputs that are
// disabled or faulted are cleared (never left showing stale lines) and hidden;
// an unselected exclusive-group output is only hidden and marked dirty, unless
// this is a dataset reset, where every inactive output is emptied because
// hidden native series still feed LWC's unified time axis. Active dirty
// outputs are filled from `bars`; `full` re-renders all active outputs.
function inactiveReason(output) {
    const plugin = registry.plugins.find(p => p.id === output.pluginId)
    if (!plugin || !pluginEnabled(plugin) || pluginFault(plugin)) return "off"
    return outputActive(output) ? null : "mode"
}

function syncPluginSeries(bars, full, reset = false) {
    const aggregate = useAggregateMode()
    forEachRenderer(renderer => {
        const reason = inactiveReason(renderer.output)
        if (reason) {
            renderer.setVisible(false)
            if (reason === "off" || reset) renderer.clear()
            else if (full) renderer.dirty = true   // window data changed while unselected
            return
        }
        if (full || renderer.dirty) renderer.render(bars, aggregate)
        renderer.setVisible(true)
    })
    lastSegmentedRefreshAt = performance.now()
}

// Incremental per-tick path for active patchable outputs; a dirty output is
// first filled from the rendered window. Segmented outputs refresh throttled.
function updatePluginPoint(bar) {
    const aggregate = useAggregateMode()
    forEachRenderer(renderer => {
        if (renderer.output.throttled || !outputActive(renderer.output)) { if (!renderer.output.throttled) renderer.dirty = true; return }
        if (renderer.dirty) { renderer.render(renderedWindow.bars, aggregate); renderer.setVisible(true) }
        else renderer.update(bar, aggregate)
    })
}

// Chart follow-up after indicator results/status changed for `ids`.
function refreshPluginOutputs(ids, statusChanged, touched = null) {
    if (statusChanged) renderPluginControls()
    else updateFlowReadout()
    if (visualWorkSuspended()) {
        // Mark affected renderers dirty so the resume repaint includes them.
        forEachRenderer(renderer => { if (ids.has(renderer.output.pluginId)) renderer.dirty = true })
        visualOptionsPending = true; chartNeedsRender = true; return
    }
    const aggregate = useAggregateMode()
    const window = renderedWindow.bars
    const firstTouched = touched?.length ? Math.min(...touched) : null
    // Incremental when only the newest rendered bar(s) changed; else re-render.
    const tailStart = window.length > 1 ? window[window.length - 2].time : window[0]?.time
    const incremental = !statusChanged && firstTouched !== null && tailStart !== undefined && firstTouched >= tailStart
    forEachRenderer(renderer => {
        if (!ids.has(renderer.output.pluginId)) return
        if (statusChanged || !incremental || renderer.output.throttled) { renderer.dirty = true; return }
        if (!outputActive(renderer.output)) { renderer.dirty = true; return }
        for (const bar of window) if (bar.time >= firstTouched) renderer.update(bar, aggregate)
    })
    syncPluginSeries(window, false)
}

let segmentedRefreshTimer = null
let segmentedRefreshFrame = null
let lastSegmentedRefreshAt = 0
// Keep ingesting data/title updates in background tabs, not chart mutations.
// Safari may suspend painting/RAF while REST and websocket events continue.
let chartNeedsRender = false
let chartRenderFrame = null
let pendingChartRange = null
let paneLayoutFrame = null
let paneLabelsFrame = null
let rangeReleaseFrame = null
let initialViewportPending = false
let pageAway = false
let visualOptionsPending = false
let renderedWindow = { start: 0, bars: [] }
let renderedMode = null
let renderedPreviousBar = null

function visualWorkSuspended() {
    return pageAway || document.hidden
}

function historyPageSize() {
    return tfSel.value === "1h" ? 480 : CONFIG.HISTORY_PAGE_SIZE
}

function applyInitialPaneHeights(totalHeight) {
    if (paneHeightsInitialized || totalHeight <= 0) return
    const panes = chart.panes()
    const wanted = registry.panes.length
    if (panes.length < wanted) return
    // Price keeps the original 58% share; indicator panes split the rest.
    if (wanted === 1) panes[0].setHeight(Math.round(totalHeight))
    else {
        panes[0].setHeight(Math.round(totalHeight * 0.58))
        const share = 0.42 / (wanted - 1)
        for (let index = 1; index < wanted; index++) panes[index].setHeight(Math.round(totalHeight * share))
    }
    paneHeightsInitialized = true
    layoutPaneLabels()
}

function layoutPaneLabels() {
    const panes = chart.panes()
    if (!paneLabels || panes.length < registry.panes.length) return

    const totalHeight = chartEl.getBoundingClientRect().height
    if (totalHeight <= 0) return

    const heights = panes.slice(0, registry.panes.length).map(p => p.getHeight())
    paneLabels.style.gridTemplateRows = heights.map(h => `${h}px`).join(" ")
}

function syncPaneLayout() {
    if (visualWorkSuspended() || paneLayoutFrame !== null) return
    paneLayoutFrame = requestAnimationFrame(() => {
        paneLayoutFrame = null
        if (visualWorkSuspended()) return
        const rect = chartEl.getBoundingClientRect()
        if (rect.width <= 0 || rect.height <= 0) return
        applyInitialPaneHeights(rect.height)
        chart.resize(rect.width, rect.height)
        if (paneLabelsFrame !== null) cancelAnimationFrame(paneLabelsFrame)
        paneLabelsFrame = requestAnimationFrame(() => {
            paneLabelsFrame = null
            if (!visualWorkSuspended()) layoutPaneLabels()
        })
    })
}

syncPaneLayout()

new ResizeObserver(() => {
    // LWC's autoSize observer schedules pane sizing for the next paint. Reading
    // heights immediately leaves our interactive flow label at its old offset.
    if (visualWorkSuspended() || paneLabelsFrame !== null) return
    paneLabelsFrame = requestAnimationFrame(() => {
        paneLabelsFrame = null
        if (!visualWorkSuspended()) layoutPaneLabels()
    })
}).observe(chartEl)

function releaseProgrammaticRangeChange() {
    if (rangeReleaseFrame !== null) cancelAnimationFrame(rangeReleaseFrame)
    rangeReleaseFrame = requestAnimationFrame(() => {
        rangeReleaseFrame = null
        programmaticRangeChange = false
    })
}

function currentChartRange() {
    // The hidden canvas still has its old indices after history is prepended.
    // Later REST/WS merges must preserve the already-adjusted pending viewport.
    // backboneSeries spans every cached timestamp (the stable timeline
    // backbone), so chart logical indices are cache indices directly.
    return pendingChartRange ?? chart.timeScale().getVisibleLogicalRange()
}

function chartRangeForCache(range) {
    return range
}

// Timestamp-only snapshot of the backbone membership; resynced only when any
// timestamp changes (full compare, no allocation when unchanged).
let timelineTimes = []
function syncTimeline(bars) {
    if (bars.length === timelineTimes.length) {
        let index = 0
        while (index < bars.length && bars[index].time === timelineTimes[index]) index++
        if (index === bars.length) return false
    }
    timelineTimes = bars.map(b => b.time)
    backboneSeries.setData(timelineTimes.map(time => ({ time, value: 0 })))
    return true
}

// Maps a logical range expressed against `previous` onto `next` by keeping
// the timestamp under range.from (plus its fractional/overscroll offset) and
// the existing span. Binary lookup only; no full scans.
function reanchorRange(previous, next, range) {
    if (!range || previous === next || !previous.length || !next.length) return range
    const anchor = Math.max(0, Math.min(previous.length - 1, Math.floor(range.from)))
    const index = lowerBoundByTime(next, previous[anchor].time)
    if (index === anchor) return range
    const from = index + (range.from - anchor)
    return { from, to: from + (range.to - range.from) }
}

function selectRenderWindow(range) {
    const count = page.bars.length
    const width = range ? Math.max(1, range.to - range.from + 1) : historyPageSize()
    const size = Math.min(count, Math.max(CONFIG.RENDER_WINDOW_SIZE, Math.ceil(width) + 2 * CONFIG.RENDER_WINDOW_BUFFER))
    const previousStart = renderedWindow.bars.length ? barIndexByTime(page.bars, renderedWindow.bars[0].time) : -1
    const margin = CONFIG.RENDER_WINDOW_BUFFER / 2
    const previousSize = renderedWindow.bars.length
    if (range && previousStart >= 0 && previousSize >= size && previousSize <= size + CONFIG.RENDER_WINDOW_BUFFER &&
        range.from >= previousStart + (previousStart > 0 ? margin : 0) &&
        range.to <= previousStart + previousSize - 1 - (previousStart + previousSize < count ? margin : 0)) {
        return { start: previousStart, end: previousStart + previousSize }
    }
    const center = range ? (range.from + range.to) / 2 : count - size / 2
    const start = Math.max(0, Math.min(count - size, Math.floor(center - size / 2)))
    return { start, end: start + size }
}

function refreshRenderWindow(range) {
    const next = selectRenderWindow(range)
    if (!chartNeedsRender && next.start === renderedWindow.start && next.end - next.start === renderedWindow.bars.length) return
    queueChartRender(range)
}

function queueChartRender(keepRange = null) {
    chartNeedsRender = true
    if (keepRange) pendingChartRange = keepRange
    if (visualWorkSuspended() || chartRenderFrame !== null) return
    chartRenderFrame = requestAnimationFrame(() => {
        chartRenderFrame = null
        if (visualWorkSuspended()) return
        const range = currentChartRange()
        setBars(page.bars, range)
    })
}

function cancelVisualWork() {
    clearScheduledSegmentedRefresh()
    for (const frame of [chartRenderFrame, paneLayoutFrame, paneLabelsFrame, rangeReleaseFrame]) {
        if (frame !== null) cancelAnimationFrame(frame)
    }
    chartRenderFrame = paneLayoutFrame = paneLabelsFrame = rangeReleaseFrame = null
    programmaticRangeChange = false
}

function resetViewport() {
    chart.clearCrosshairPosition()
    chart.timeScale().fitContent()
    priceSeries.priceScale().applyOptions({
        autoScale: true,
        mode: LightweightCharts.PriceScaleMode.Normal,
    })
    forEachRenderer(renderer => renderer.priceScales().forEach(scale => scale.applyOptions({
        autoScale: true,
        mode: LightweightCharts.PriceScaleMode.Normal,
    })))
}

let programmaticRangeChange = false
let lastBeforeFetchAt = 0
let lastAfterFetchAt = 0
let ws = null
let pingTimer = null
let delayedPollTimer = null
let wsGeneration = 0
let historyGeneration = 0
let historyController = new AbortController()
let realtimeEnabled = true
let updatesSuspended = false
let modeConnectionsReady = false
const page = {
    bars: [],
    oldestTime: null,
    newestTime: null,
    confirmedTime: null,
    loadingOlder: false,
    loadingNewer: false,
    reachedStart: false,
    reachedEnd: false
}

function emptyHistoryPayload() {
    return {
        bars: [],
        has_more_before: false,
        has_more_after: false,
        oldest_time: null,
        newest_time: null,
        realtime_enabled: false,
    }
}

function currentAccessKey() {
    return accessKeyInput.value.trim()
}

function useAggregateMode() {
    return aggToggle.checked
}

let hoveredFlowTime = null

function hoveredBar() {
    return hoveredFlowTime === null ? page.bars.at(-1) : page.bars[barIndexByTime(page.bars, hoveredFlowTime)]
}

function setText(element, key, value) {
    if (element[key] !== value) element[key] = value
}

function updateQualityReadout(bar = hoveredBar()) {
    const badge = qualityDisplay(bar, useAggregateMode(), Date.now(), hoveredFlowTime === null || bar === page.bars.at(-1), { exchange: exchangeSel.value, market: marketSel.value })
    setText(qualityStateEl, "textContent", badge.label)
    setText(qualityStateEl, "title", badge.tooltip)
    setText(qualityStateEl.dataset, "state", ['Quality ·', 'Quality unknown', 'Source ·', 'Source unknown'].includes(badge.label) ? 'quiet' : 'attention')
}
// One page-level label timer, never chart redraw or a per-request timer.
let qualityTimer = null
function startQualityTimer() {
    if (!page.bars.at(-1)?.quality && qualityTimer) { clearInterval(qualityTimer); qualityTimer = null }
    if (!qualityTimer && page.bars.at(-1)?.quality) qualityTimer = setInterval(() => { if (!pageAway && !document.hidden) updateQualityReadout() }, 15000)
}

// Per-pane readout of the hovered (or latest) bar for the pane's active
// outputs, from descriptor readout channels. Faulted outputs say so instead
// of showing stale or zero values.
function paneSummaryText(pane, bar) {
    const parts = []
    for (const output of pane.outputs) {
        const plugin = registry.plugins.find(p => p.id === output.pluginId)
        if (!plugin || !pluginEnabled(plugin) || output.kind === "segmented_lines") continue
        const fault = pluginFault(plugin)
        if (fault) { parts.push(`${output.label} unavailable (${fault.state})`); continue }
        parts.push(readoutText(output, bar ? storeValue(bar, output.key, useAggregateMode()) : undefined))
    }
    const text = parts.join(" · ")
    const quote = SOURCE_INSTRUMENTS[sourceKey(exchangeSel.value, marketSel.value, symbolSel.value)]?.quote_asset
    if (!text || !quote || !pane.outputs.some(output => ["cvd", "flow"].includes(output.pluginId))) return text
    const unit = useAggregateMode() ? "nominal USD/USDT/USDC quote · no FX conversion" : `${quote} quote turnover`
    return `${text} · ${unit}`
}

function updateFlowReadout() {
    startQualityTimer()
    const bar = hoveredBar()
    for (const el of paneSummaryEls) {
        const pane = registry.panes.find(p => p.id === el.dataset.paneSummary)
        if (pane) setText(el, "textContent", paneSummaryText(pane, bar))
    }
    updateQualityReadout(bar)
}

function sanePriceField(value, fallback) {
    const price = Number(value)
    if (!Number.isFinite(price) || price <= 0) return fallback
    return price
}

function closeForBar(bar) {
    const currentClose = Number(bar.price_close)
    if (useAggregateMode()) return sanePriceField(bar.apc, currentClose)
    return currentClose
}

function openForBar(bar) {
    const close = closeForBar(bar)
    return sanePriceField(useAggregateMode() ? bar.apo : bar.po, close)
}

function highForBar(bar) {
    const close = closeForBar(bar)
    const open = openForBar(bar)
    const high = sanePriceField(useAggregateMode() ? bar.aph : bar.ph, close)
    const candidates = [high, open, close].filter(price => Number.isFinite(price) && price > 0)
    return candidates.length ? Math.max(...candidates) : NaN
}

function lowForBar(bar) {
    const close = closeForBar(bar)
    const open = openForBar(bar)
    const low = sanePriceField(useAggregateMode() ? bar.apl : bar.pl, close)
    const candidates = [low, open, close].filter(price => Number.isFinite(price) && price > 0)
    return candidates.length ? Math.min(...candidates) : NaN
}

function candleColorForBar(open, close, previousClose) {
    // Lightweight Charts normally colors candles by close-vs-open. For this
    // dashboard the more useful signal is price direction versus the previous
    // candle; otherwise a candle can be green while sitting lower than the
    // previous bar, which reads as random on tiny intraday candles.
    if (Number.isFinite(previousClose)) {
        if (close > previousClose) return PRICE_UP_COLOR
        if (close < previousClose) return PRICE_DOWN_COLOR
        return PRICE_FLAT_COLOR
    }
    if (Number.isFinite(open) && Number.isFinite(close)) {
        if (close > open) return PRICE_UP_COLOR
        if (close < open) return PRICE_DOWN_COLOR
    }
    return PRICE_FLAT_COLOR
}

function pricePointForBar(bar, previousBar = null) {
    const close = closeForBar(bar)
    const open = openForBar(bar)
    const high = highForBar(bar)
    const low = lowForBar(bar)
    const previousClose = previousBar ? closeForBar(previousBar) : NaN
    const color = candleColorForBar(open, close, previousClose)
    return {
        time: bar.time,
        open: Number.isFinite(open) ? open : close,
        high: Number.isFinite(high) ? high : close,
        low: Number.isFinite(low) ? low : close,
        close,
        color,
        borderColor: color,
        wickColor: color,
    }
}

function pricePointsForBars(bars, previousBar = null) {
    return bars.map((bar, index) => pricePointForBar(bar, index > 0 ? bars[index - 1] : previousBar))
}

function clearScheduledSegmentedRefresh() {
    if (segmentedRefreshTimer) {
        clearTimeout(segmentedRefreshTimer)
        segmentedRefreshTimer = null
    }
    if (segmentedRefreshFrame !== null) {
        cancelAnimationFrame(segmentedRefreshFrame)
        segmentedRefreshFrame = null
    }
}

function refreshSegmentedNow() {
    clearScheduledSegmentedRefresh()
    if (visualWorkSuspended()) {
        visualOptionsPending = true
        chartNeedsRender = true
        return
    }
    const aggregate = useAggregateMode()
    forEachRenderer(renderer => {
        if (!renderer.output.throttled) return
        if (outputActive(renderer.output)) renderer.render(renderedWindow.bars, aggregate)
        else renderer.clear()
    })
    lastSegmentedRefreshAt = performance.now()
}

function anyThrottledActive() {
    for (const renderer of renderers.values()) if (renderer.output.throttled && outputActive(renderer.output)) return true
    return false
}

function scheduleSegmentedRefresh() {
    if (!page.bars.length || !anyThrottledActive()) return
    if (visualWorkSuspended()) {
        chartNeedsRender = true
        return
    }
    if (segmentedRefreshTimer || segmentedRefreshFrame !== null) return

    const elapsed = performance.now() - lastSegmentedRefreshAt
    const delay = Math.max(0, CONFIG.SEGMENTED_REALTIME_REFRESH_MS - elapsed)
    const queueFrame = () => {
        segmentedRefreshTimer = null
        if (visualWorkSuspended()) {
            chartNeedsRender = true
            return
        }
        segmentedRefreshFrame = requestAnimationFrame(() => {
            segmentedRefreshFrame = null
            refreshSegmentedNow()
        })
    }

    if (delay === 0) {
        queueFrame()
    } else {
        segmentedRefreshTimer = setTimeout(queueFrame, delay)
    }
}

function refreshModeData(keepRange = null) {
    // Indicator health is per mode (source/aggregate): relabel on mode change.
    renderPluginControls()
    noteDataChanged("meta")   // data basis bumps only if scope/mode/cache actually changed
    if (!page.bars.length) return
    const currentRange = keepRange ?? currentChartRange()
    setBars(page.bars, currentRange)
    savePrefs()
}

function updateDocumentTitle() {
    const last = page.bars[page.bars.length - 1]
    const lastClose = last ? closeForBar(last) : NaN
    if (!last || !Number.isFinite(lastClose)) {
        document.title = DEFAULT_TITLE
        return
    }

    const prev = page.bars.length > 1 ? page.bars[page.bars.length - 2] : null
    const prevClose = prev ? closeForBar(prev) : NaN
    const arrow = !Number.isFinite(prevClose)
        ? "•"
        : lastClose > prevClose
            ? "▲"
            : lastClose < prevClose
                ? "▼"
                : "•"
    const mode = realtimeEnabled ? "live" : "delay"
    document.title = `${symbolSel.value} ${arrow} ${lastClose.toFixed(2)} · ${tfSel.value} · ${mode}`
}

function downloadChartScreenshot() {
    if (typeof chart.takeScreenshot !== "function") {
        console.warn("chart screenshot API unavailable")
        return
    }

    const canvas = chart.takeScreenshot()
    const link = document.createElement("a")
    const stamp = new Date().toISOString().replace(/[:.]/g, "-")
    link.href = canvas.toDataURL("image/png")
    link.download = `cvd-${exchangeSel.value}-${marketSel.value}-${symbolSel.value}-${tfSel.value}-${stamp}.png`
    link.click()
}

function setBars(bars, keepRange = null, dataKind = null) {
    keepRange = keepRange ?? pendingChartRange ?? (initialViewportPending ? null : currentChartRange())
    // Re-render of the SAME canonical array (mode relabel/layout) is metadata-only.
    const sameArray = bars === page.bars
    page.bars = bars
    page.oldestTime = page.bars[0]?.time ?? null
    indicatorStore.pruneBefore(page.oldestTime)
    page.newestTime = page.bars[page.bars.length - 1]?.time ?? null
    noteDataChanged(dataKind ?? (sameArray ? "meta" : "history"))
    updateDocumentTitle()
    updateFlowReadout()
    if (visualWorkSuspended()) {
        queueChartRender(keepRange)
        return
    }
    // Dataset reset (reload/source/timeframe; deferred to first visible paint
    // when hidden): drop native hover before series data disappears, or LWC
    // repaints the stale crosshair against removed points ("Value is null").
    // Inactive plugin outputs (unselected group mode, disabled, faulted) are
    // always emptied by syncPluginSeries: hidden native series still feed
    // LWC's unified time axis, so old-grid rows must not survive a reset.
    const datasetReset = !page.bars.length || initialViewportPending
    if (datasetReset) chart.clearCrosshairPosition()
    if (chartRenderFrame !== null) cancelAnimationFrame(chartRenderFrame)
    chartRenderFrame = null
    pendingChartRange = null
    chartNeedsRender = false
    programmaticRangeChange = true

    const next = selectRenderWindow(keepRange)
    const visibleBars = page.bars.slice(next.start, next.end)
    const previousBar = page.bars[next.start - 1] ?? null
    const mode = `${useAggregateMode()}:${tfSel.value}:${pluginLayoutGeneration}`
    const needsData = initialViewportPending || mode !== renderedMode || previousBar !== renderedPreviousBar ||
        visibleBars.length !== renderedWindow.bars.length || visibleBars.some((bar, index) => bar !== renderedWindow.bars[index])
    renderedWindow = { start: next.start, bars: visibleBars }
    renderedMode = mode
    renderedPreviousBar = previousBar
    if (needsData) {
        priceSeries.setData(pricePointsForBars(visibleBars, previousBar))
        clearScheduledSegmentedRefresh()
        syncPluginSeries(visibleBars, true, datasetReset)
    }
    const timelineChanged = syncTimeline(page.bars)
    if (visualOptionsPending) {
        // Toggles changed while hidden: apply once now that painting resumed.
        visualOptionsPending = false
        if (!needsData) syncPluginSeries(renderedWindow.bars, false)
    }

    // A pure data-window recenter leaves the timeline untouched; re-applying
    // the range there would fight the native drag gesture.
    // Apply the requested range only when it differs from the native one: an
    // explicit setBars(bars, range) still moves the viewport, while a native
    // drag recenter (requested == current) never fights the gesture.
    if (keepRange && (timelineChanged || !sameRange(keepRange, chart.timeScale().getVisibleLogicalRange()))) chart.timeScale().setVisibleLogicalRange(keepRange)
    if (initialViewportPending) {
        initialViewportPending = false
        resetViewport()
    }
    releaseProgrammaticRangeChange()
    syncPaneLayout()
}

async function fetchHistory(limit, before = null, after = null) {
    const generation = historyGeneration
    const signal = historyController.signal
    const requestedExchange = exchangeSel.value
    const requestedSymbol = symbolSel.value
    const requestedMarket = marketSel.value
    if (!available(requestedExchange, requestedMarket, requestedSymbol)) throw new Error("Unsupported source selection")
    const u = new URL("/history", location.origin)
    u.searchParams.set("exchange", requestedExchange)
    u.searchParams.set("symbol", requestedSymbol)
    u.searchParams.set("market", requestedMarket)
    u.searchParams.set("tf", tfSel.value)
    u.searchParams.set("limit", limit)
    const key = currentAccessKey()
    if (key) u.searchParams.set("key", key)
    // Legacy USDT spot omits series for older servers; new spot is explicit.
    if (requestedMarket !== "spot") u.searchParams.set("series", sourceSeries(requestedMarket))
    if (before != null) u.searchParams.set("before", before)
    if (after != null) u.searchParams.set("after", after)
    const r = await fetch(u, { signal })
    if (!r.ok) throw new Error(`History HTTP ${r.status}`)
    const data = await r.json()
    if (signal.aborted || generation !== historyGeneration) throw new DOMException("Stale history", "AbortError")
    assertSourceSeries(data, requestedMarket, requestedExchange, requestedSymbol)
    absorbHistoryValidity(data)
    if (data.foreign_epoch) {
        // Rejected whole BEFORE any merge: rows, exhaustion flags and cursors
        // of another producer incarnation are not authority for this cache.
        const error = new DOMException("Foreign producer epoch", "AbortError")
        error.foreignEpoch = true
        throw error
    }
    // Server-computed `indicators` blocks (legacy backend, removed in lane E)
    // are deliberately ignored: only browser WASM outputs are displayed.
    return data
}

// ---- Canonical validity (producer -> browser boundary) ----
// Authority is the producer's data revision, never request order or wall
// time. `rev` is comparable only inside one producer epoch (process).
// invalidPoison: time -> { rev, epoch } of the newest known invalidation
//   (`/history.validity` mark, WS `invalidate`, or rev 0 for a DB-only
//   invalid row). Poisoned buckets are removed from the shared cache (never
//   zero-filled) and passed to the Rust coordinator, which refuses History/
//   live windows spanning them.
// validAuth: time -> { rev, epoch } of the newest proven valid REPAIR, so an
//   older invalid observation (slow HTTP page, buffered frame) cannot
//   re-poison after the poison entry itself was deleted. Bounded.
//   Recorded for EVERY proven (rev > 0) valid observation of the current
//   epoch, not only repairs, so a delayed invalid rev10 cannot follow a
//   valid rev12 even when no poison existed.
// A valid observation clears poison only if it is a strictly newer revision
// of the same epoch, or a revision-bound (rev > 0) observation of a newer
// epoch. An unmarked DB row (rev 0) never clears poison: the DB/outbox can lag
// live invalidation. After a producer restart a poison from the old epoch is
// kept (fail closed) until a new-epoch revision repairs it or the page reloads.
// Epoch authority: the epoch is an opaque process identity (NOT sortable).
// The established incarnation changes only when a NEW WebSocket connection
// (a newer wsGeneration) delivers a different epoch, or on the first
// observation after resetValidity. Any other observation carrying a different
// epoch (delayed HTTP page, buffered frame) is FOREIGN: its rows are filtered
// fail-closed but it never mutates poison/validAuth or the incarnation.
let invalidPoison = new Map()
let validAuth = new Map()
let producerEpoch = null
let epochGeneration = null
const VALID_AUTH_CAP = 8192
function resetValidity() { invalidPoison = new Map(); validAuth = new Map(); producerEpoch = null; epochGeneration = null }
// Called by the CURRENT socket's onmessage only (generation already checked).
function adoptConnectionEpoch(payload, generation) {
    if (!Number.isSafeInteger(payload?.epoch)) return
    if (payload.epoch === producerEpoch) { epochGeneration = generation; return }
    if (producerEpoch == null || epochGeneration !== generation) { producerEpoch = payload.epoch; epochGeneration = generation }
}
// Authority epoch of an observation, or undefined when it is foreign.
function frameEpoch(payload) {
    if (!Number.isSafeInteger(payload?.epoch)) return producerEpoch
    if (producerEpoch == null) producerEpoch = payload.epoch
    return payload.epoch === producerEpoch ? producerEpoch : undefined
}
function noteValidAuth(t, rev, epoch) {
    if (!(rev > 0)) return
    const va = validAuth.get(t)
    if (va && va.epoch === epoch && va.rev >= rev) return
    validAuth.delete(t)
    validAuth.set(t, { rev, epoch })
    if (validAuth.size > VALID_AUTH_CAP) validAuth.delete(validAuth.keys().next().value)
}
// true when the invalid observation (rev, epoch) is authoritative now.
function acceptInvalid(t, rev, epoch) {
    if (epoch === undefined) return false                   // foreign: never poisons
    const va = validAuth.get(t)
    if (va && va.epoch === epoch && va.rev >= rev) return false
    if (va && va.epoch === epoch && rev === 0) return false  // same-epoch DB-only fact older than a live repair
    const p = invalidPoison.get(t)
    if (p && p.epoch === epoch && p.rev >= rev) return false
    invalidPoison.set(t, { rev, epoch })
    validAuth.delete(t)
    return true
}
// true when the valid observation (rev, epoch) repairs/keeps bucket t.
// Admission verdict, decided BEFORE any cache/value/series mutation: a
// rejected observation never overwrites, deletes, clears or un-poisons.
function acceptValid(t, rev, epoch) {
    if (epoch === undefined) return false                   // foreign: never mutates current rows
    const va = validAuth.get(t)
    if (va && va.epoch === epoch && rev < va.rev) return false   // older than the newest proven row
    const p = invalidPoison.get(t)
    if (p) {
        if (p.epoch === epoch ? rev <= p.rev : !(rev > 0)) return false
        invalidPoison.delete(t)
    }
    noteValidAuth(t, rev, epoch)
    return true
}
function removeCacheTimes(times) {
    const drop = new Set(times)
    invalidateLocalAt(times)
    const previous = page.bars
    const kept = previous.filter(b => !drop.has(b.time))
    if (kept.length !== previous.length) setBars(kept, reanchorRange(previous, kept, currentChartRange()), "history")
    else noteDataChanged("history")
}
// Applied to every /history response before any merge.
function absorbHistoryValidity(data) {
    const epoch = frameEpoch(data)
    let changed = false
    const drop = []
    const marks = new Map()
    for (const m of data.validity ?? []) if (Number.isInteger(m?.time) && Number.isSafeInteger(m?.rev)) marks.set(m.time, m)
    for (const [t, m] of marks) {
        if (m.valid) { const was = invalidPoison.has(t); if (acceptValid(t, m.rev, epoch) && was) changed = true }
        else if (acceptInvalid(t, m.rev, epoch)) { drop.push(t); changed = true }
    }
    const pageInvalid = new Set()
    for (const t of data.invalid_times ?? []) {
        if (!Number.isInteger(t)) continue
        pageInvalid.add(t)
        if (marks.has(t)) continue
        if (acceptInvalid(t, 0, epoch)) { drop.push(t); changed = true }
    }
    // A row is kept only if its bucket is not poisoned (an unmarked row cannot
    // repair a known invalidation; a marked valid repair was accepted above).
    // A foreign-epoch page additionally never contributes rows it reports
    // invalid nor rows for currently poisoned buckets.
    // Row admission: a foreign-epoch page contributes NO rows (it can neither
    // overwrite nor fill current data) and its exhaustion flags are not
    // authority (no false exhaustion; the truthful coverage cursor is kept).
    // A current-epoch row is admitted only if its bucket is not poisoned and
    // it is not older than the newest proven row authority of this epoch (an
    // unmarked DB row is rev 0 and cannot overwrite a proven cached live row).
    if (epoch === undefined) {
        data.bars = []
        data.has_more_before = true
        data.has_more_after = true
        data.foreign_epoch = true
    } else {
        data.bars = (data.bars || []).filter(b => {
            if (invalidPoison.has(b.time)) return false
            const va = validAuth.get(b.time)
            if (!va || va.epoch !== epoch) return true
            const m = marks.get(b.time)
            return (m?.valid ? m.rev : 0) >= va.rev || barIndexByTime(page.bars, b.time) < 0
        })
    }
    if (drop.some(t => barIndexByTime(page.bars, t) >= 0)) removeCacheTimes(drop)
    else if (changed) noteDataChanged("history")
}
function applyRealtimeInvalidation(payload) {
    const t = payload.invalid_time
    if (!Number.isInteger(t) || t <= 0) throw new Error("invalid invalidate frame")
    const epoch = frameEpoch(payload)
    // Foreign-epoch invalidation is rejected: it records nothing and never
    // deletes a current row (a rejected observation cannot mutate).
    if (epoch === undefined) return
    if (!acceptInvalid(t, payload.rev ?? 0, epoch)) return
    removeCacheTimes([t])
}
// Withdraw a plugin's values on a window the coordinator refused.
function onLocalInvalid(id, mode, want, range) {
    if (!localRecords.get(id)?.enabled) return
    if (want && (want.scopeKey !== localScopeKey || want.historyRevision !== historyRevision)) return
    const inRange = t => t >= range.from && t <= range.to
    for (const map of [localValues, localLive]) for (const [key, col] of map) {
        if (!key.startsWith(`${id}/`) || !key.endsWith(`|${mode}`)) continue
        for (const t of [...col.keys()]) if (inRange(t)) col.delete(t)
    }
    refreshPluginOutputs(new Set([id]), true)
}

function reportHistoryError(error) {
    if (error.name === "AbortError") return
    statusEl.textContent = "history error"
    console.warn("history request failed", error)
}

function recordConfirmedHistory(bars) {
    if (!bars.length) return
    page.confirmedTime = Math.max(page.confirmedTime ?? -Infinity, bars[bars.length - 1].time)
}

function equalBarData(left, right) {
    if (left === right) return true
    if (left == null || right == null || typeof left !== "object" || typeof right !== "object") return false
    if (Array.isArray(left) !== Array.isArray(right)) return false
    const keys = Object.keys(left)
    return keys.length === Object.keys(right).length &&
        keys.every(key => Object.hasOwn(right, key) && equalBarData(left[key], right[key]))
}

function barIndexByTime(bars, time) {
    let low = 0
    let high = bars.length - 1
    while (low <= high) {
        const middle = Math.floor((low + high) / 2)
        const found = bars[middle].time
        if (found === time) return middle
        if (found < time) low = middle + 1
        else high = middle - 1
    }
    return -1
}

function lowerBoundByTime(bars, time) {
    let low = 0
    let high = bars.length
    while (low < high) {
        const middle = (low + high) >> 1
        if (bars[middle].time < time) low = middle + 1
        else high = middle
    }
    return low
}

// Returns page.bars itself when nothing changed, otherwise a new array: the
// untouched prefix is copied by reference and only the suffix starting at the
// earliest changed time is re-keyed and sorted (prepend falls back to whole).
function mergeHistoryBars(bars) {
    let changes = null
    let earliest = Infinity
    for (const bar of bars) {
        const existing = changes?.has(bar.time) ? changes.get(bar.time) : page.bars[barIndexByTime(page.bars, bar.time)]
        const incoming = mergeHistoryQuality(existing, bar)
        if (equalBarData(existing, incoming)) continue
        if (!changes) changes = new Map()
        changes.set(bar.time, incoming)
        if (bar.time < earliest) earliest = bar.time
    }
    if (!changes) return page.bars
    const start = lowerBoundByTime(page.bars, earliest)
    const suffix = new Map()
    for (let index = start; index < page.bars.length; index++) suffix.set(page.bars[index].time, page.bars[index])
    for (const [time, bar] of changes) suffix.set(time, bar)
    return page.bars.slice(0, start).concat([...suffix.values()].sort((left, right) => left.time - right.time))
}

// keepRange is in the CURRENT (pre-merge) cache basis, captured when the
// merge is applied; it is reanchored by timestamp onto the merged cache.
// (setBars(bars, range) instead takes a range already in the new basis.)
function setMergedBars(bars, keepRange = null) {
    const previous = page.bars
    keepRange = reanchorRange(previous, bars, keepRange)
    if (bars === previous && !initialViewportPending) return
    if (initialViewportPending || !previous.length || bars.length < previous.length ||
        previous.some((bar, index) => index < previous.length - 1 && bar !== bars[index]) ||
        bars[previous.length - 1]?.time !== previous.at(-1).time) {
        setBars(bars, keepRange)
        return
    }
    const tail = bars.slice(previous.length - 1)
    if (tail[0] === previous.at(-1)) tail.shift()
    if (!tail.length) return
    applyTailBars(tail, keepRange)
    if (keepRange && !visualWorkSuspended() && !chartNeedsRender) {
        programmaticRangeChange = true
        chart.timeScale().setVisibleLogicalRange(chartRangeForCache(keepRange))
        releaseProgrammaticRangeChange()
    }
}

function applyHistoryPage(data, keepRange = null, confirmedBars = data.bars || []) {
    // Exhaustion flags first: the local-plugin demand published by the merge
    // must carry the page's truthful `exhausted` state (terminal coverage).
    page.reachedStart = !data.has_more_before
    page.reachedEnd = !data.has_more_after
    setMergedBars(data.bars || [], keepRange)
    recordConfirmedHistory(confirmedBars)
    realtimeEnabled = !!data.realtime_enabled
    statusEl.textContent = !page.bars.length && ["spot_usdc", "usdm_usdc"].includes(marketSel.value)
        ? "no stored history yet" : realtimeEnabled ? "live" : "delay 1h"
    updateDocumentTitle()
}

function applyRealtimePayload(payload) {
    if (payload?.action === "invalidate") { assertSourceSeries(payload, marketSel.value); applyRealtimeInvalidation(payload); return }
    scopeRepaintPending = false
    applyRealtimeBase(payload)
    // Observation bound advanced: pending results for ANY bar in the scope
    // may now be current. Repaint after the base bar is displayed (the
    // hidden-safe path marks renderers dirty instead).
    if (scopeRepaintPending) {
        scopeRepaintPending = false
        const ids = new Set()
        forEachRenderer(renderer => ids.add(renderer.output.pluginId))
        if (ids.size) refreshPluginOutputs(ids, false, null)
    }
}

let scopeRepaintPending = false
function applyRealtimeBase(payload) {
    assertSourceSeries(payload, marketSel.value)
    const b = payload?.bar
    if (!b?.time) return
    // A live base frame predating an accepted historical replacement of this
    // bar is stale: displaying it would detach the bar from its results.
    if (indicatorStore.baseSuperseded(b.time, payload?.rev)) return
    // Validity: a frame older than an invalidation of this bucket is stale;
    // a newer one is the validated repair (re-inserted, History recomputed).
    const epoch = frameEpoch(payload)
    const wasPoisoned = invalidPoison.has(b.time)
    if (!acceptValid(b.time, payload.rev ?? 0, epoch)) return
    if (wasPoisoned) {
        const lastTime = page.bars.at(-1)?.time
        if (lastTime != null && b.time < lastTime) {
            noteBaseRev(payload)
            setBars(mergeHistoryBars([b]), currentChartRange(), "history")
            return
        }
        noteBaseRev(payload)
        applyTailBars([b])
        noteDataChanged("history")
        return
    }
    // Revision of the base frame now shown for this bar (as_of_rev fence).
    const authorityChanged = noteBaseRev(payload)
    if (authorityChanged === "scope") scopeRepaintPending = true

    const last = page.bars[page.bars.length - 1]
    const sameShown = (last?.time === b.time && equalBarData(last, b)) ||
        (last && b.time < last.time && equalBarData(page.bars[barIndexByTime(page.bars, b.time)], b))
    if (sameShown) {
        // Identical bar but a newer base revision: pending results may now be
        // current, so repaint affected outputs (true duplicates stay no-ops).
        if (authorityChanged === true) {
            const ids = new Set()
            forEachRenderer(renderer => ids.add(renderer.output.pluginId))
            if (ids.size) refreshPluginOutputs(ids, false, [b.time])
        }
        return
    }
    if (last && b.time < last.time) {
        const index = barIndexByTime(page.bars, b.time)
        if (index >= 0 && !equalBarData(page.bars[index], b)) {
            page.bars[index] = b
            // In-place correction of an older canonical bar: a real data change.
            setBars(page.bars, currentChartRange(), "history")
        }
        return
    }
    applyTailBars([b])
    if (statusEl.textContent === "no stored history yet") statusEl.textContent = "live · history starting"
}

function applyTailBars(bars, keepRange = null) {
    const suspended = visualWorkSuspended() || chartNeedsRender
    const paintsTail = renderedWindow.start + renderedWindow.bars.length === page.bars.length
    invalidateLocalAt(bars.filter(b => page.bars.at(-1)?.time === b.time).map(b => b.time))
    for (const b of bars) {
        if (page.bars.at(-1)?.time === b.time) page.bars[page.bars.length - 1] = b
        else page.bars.push(b)
        if (suspended || !paintsTail) continue
        if (renderedWindow.bars.at(-1)?.time === b.time) renderedWindow.bars[renderedWindow.bars.length - 1] = b
        else renderedWindow.bars.push(b)
        const previousBar = page.bars.length > 1 ? page.bars[page.bars.length - 2] : null
        priceSeries.update(pricePointForBar(b, previousBar))
        updatePluginPoint(b)
    }
    if (!suspended) syncTimelineTail(bars)

    page.newestTime = page.bars[page.bars.length - 1]?.time ?? null
    page.oldestTime = page.bars[0]?.time ?? null
    indicatorStore.pruneBefore(page.oldestTime)
    noteDataChanged("tail")
    updateDocumentTitle()
    updateFlowReadout()
    if (suspended) {
        queueChartRender(keepRange)
        return
    }
    if (paintsTail) {
        scheduleSegmentedRefresh()
        refreshRenderWindow(keepRange ?? currentChartRange())
    }
}

async function loadInitial() {
    statusEl.textContent = "loading…"
    page.reachedStart = false
    page.reachedEnd = false
    const data = await fetchHistory(historyPageSize())
    initialViewportPending = true
    localRequestedFrom = (data.bars || [])[0]?.time ?? null
    localOlderPages = []
    applyHistoryPage({ ...data, bars: mergeHistoryBars(data.bars || []) }, null, data.bars || [])
}

function syncTimelineTail(bars) {
    // Append-only membership change: extend the backbone without resubmitting
    // the full cache (realtime ticks while viewing history keep the viewport).
    for (const b of bars) {
        if (timelineTimes[timelineTimes.length - 1] === b.time) continue
        timelineTimes.push(b.time)
        backboneSeries.update({ time: b.time, value: 0 })
    }
}

function sameRange(a, b) {
    return !!a && !!b && Math.abs(a.from - b.from) < 1e-6 && Math.abs(a.to - b.to) < 1e-6
}

function shiftRange(range, delta) {
    if (!range || delta === 0) return range
    return { from: range.from + delta, to: range.to + delta }
}

// Fetch a history page, stepping over ALL-INVALID pages by the producer's
// truthful raw scan coverage (coverage_from / coverage_to). Returns the first
// page with renderable rows, the exhausted page, or (bounded) a page marked
// `stalled` that must NOT be read as exhaustion. The persistent page cursor
// stays at the cached bar edge, so a later repair of a skipped invalid bucket
// is still fetched (its validity mark lies inside the next coverage).
const HISTORY_SKIP_PAGES = 64
async function fetchHistorySkippingInvalid(limit, before, after) {
    const older = after == null
    let cursor = older ? before : after
    for (let i = 0; ; i++) {
        const data = await fetchHistory(limit, older ? cursor : null, older ? null : cursor)
        if ((data.bars || []).length) return data
        if (!(older ? data.has_more_before : data.has_more_after)) return data
        const next = older ? data.coverage_from : data.coverage_to
        if (!Number.isInteger(next) || (cursor != null && (older ? next >= cursor : next <= cursor)))
            throw new Error("History cursor did not advance")
        if (i + 1 >= HISTORY_SKIP_PAGES) return { ...data, stalled: true }
        cursor = older ? next : next
    }
}
let olderRetryTimer = null
function retryOlderSoon(forWarmup) {
    if (olderRetryTimer) return
    olderRetryTimer = setTimeout(() => { olderRetryTimer = null; if (forWarmup) warmupPrepending = true; loadMoreBefore() }, CONFIG.FETCH_COOLDOWN + 50)
}

async function loadMoreBefore() {
    const forWarmup = warmupPrepending
    warmupPrepending = false
    if (page.loadingOlder || page.reachedStart) return
    if (!page.oldestTime) return
    const now = Date.now()
    if (now - lastBeforeFetchAt < CONFIG.FETCH_COOLDOWN) return
    lastBeforeFetchAt = now

    page.loadingOlder = true
    const generation = historyGeneration
    try {
        const data = await fetchHistorySkippingInvalid(historyPageSize(), page.oldestTime, null)
        const older = data.bars || []
        // A bounded run of all-invalid pages is NOT exhaustion: continue soon.
        if (data.stalled) { retryOlderSoon(forWarmup); return }
        // Quiet exhaustion: publish the terminal demand now (no later tick needed).
        if (!older.length) { page.reachedStart = true; noteDataChanged(); return }

        const newBars = older.filter(b => b.time < page.oldestTime)
        const merged = [...newBars, ...page.bars]
        // A user-driven older page extends the canonical requested range; a
        // warm-up page only supplies context (old server basis: context rows
        // are not part of the requested page).
        if (!forWarmup && newBars.length) localOlderPages.push({ from: newBars[0].time, to: newBars.at(-1).time })

        // setMergedBars reanchors the current range by timestamp (no pre-shift)
        const reachedEnd = page.reachedEnd
        applyHistoryPage({ ...data, bars: merged }, currentChartRange(), older)
        page.reachedEnd = reachedEnd
    } catch (error) { reportHistoryError(error) }
    finally { if (generation === historyGeneration) page.loadingOlder = false }
}

async function loadMoreAfter() {
    if (page.loadingNewer || page.reachedEnd) return
    if (!page.newestTime) return
    const now = Date.now()
    if (now - lastAfterFetchAt < CONFIG.FETCH_COOLDOWN) return
    lastAfterFetchAt = now

    page.loadingNewer = true
    const generation = historyGeneration
    try {
        const data = await fetchHistorySkippingInvalid(historyPageSize(), null, page.confirmedTime ?? page.newestTime)
        const newer = data.bars || []
        if (data.stalled) return                    // not exhaustion; next edge/tick continues
        if (!newer.length) { page.reachedEnd = true; return }
        const merged = mergeHistoryBars(newer)

        // may insert missed interior buckets; setMergedBars reanchors by timestamp
        const reachedStart = page.reachedStart
        applyHistoryPage({ ...data, bars: merged }, currentChartRange(), newer)
        page.reachedStart = reachedStart
    } catch (error) { reportHistoryError(error) }
    finally { if (generation === historyGeneration) page.loadingNewer = false }
}

chart.timeScale().subscribeVisibleLogicalRangeChange(r => {
    if (programmaticRangeChange) return
    if (!r || !page.bars.length) return
    refreshRenderWindow(r)

    if (r.from <= CONFIG.EDGE_TRIGGER) loadMoreBefore()

    const maxIndex = page.bars.length - 1
    const rightIndex = Math.min(r.to, maxIndex)
    const rightGap = maxIndex - rightIndex

    if (rightGap <= CONFIG.EDGE_TRIGGER) loadMoreAfter()
})

chart.subscribeCrosshairMove(param => {
    // setData/fitContent can replay an old cursor without a pointer event.
    // After a dataset reset, stay on the latest bar until the user moves again.
    if (param?.time == null) hoveredFlowTime = null
    else if (param.sourceEvent || hoveredFlowTime !== null) hoveredFlowTime = param.time
    updateFlowReadout()
})

function stopDelayedPolling() {
    if (delayedPollTimer) {
        clearInterval(delayedPollTimer)
        delayedPollTimer = null
    }
}

function stopRealtimeSocket() {
    wsGeneration += 1
    if (pingTimer) {
        clearInterval(pingTimer)
        pingTimer = null
    }
    if (ws) {
        const socket = ws
        ws = null
        socket.close()
    }
}

async function pollDelayedUpdates() {
    if (page.loadingNewer) return

    page.loadingNewer = true
    const generation = historyGeneration
    try {
        const data = await fetchHistorySkippingInvalid(historyPageSize(), null, page.newestTime)
        const newer = data.bars || []
        if (!newer.length) {
            page.reachedEnd = !data.has_more_after && !data.stalled
            return
        }

        const merged = mergeHistoryBars(newer)
        const reachedStart = page.reachedStart
        applyHistoryPage({ ...data, bars: merged }, currentChartRange(), newer)
        page.reachedStart = reachedStart
    } catch (error) {
        reportHistoryError(error)
    } finally {
        if (generation === historyGeneration) page.loadingNewer = false
    }
}

async function catchUpHistory(generation, buffered) {
    let after = page.confirmedTime == null ? null : page.confirmedTime - 1
    let closedThrough = page.confirmedTime ?? -Infinity
    for (;;) {
        const data = await fetchHistory(historyPageSize(), null, after)
        if (generation !== wsGeneration) return false
        const bars = data.bars || []
        if (bars.length) {
            setMergedBars(mergeHistoryBars(bars), currentChartRange())
            recordConfirmedHistory(bars)
            closedThrough = Math.max(closedThrough, bars[bars.length - 1].time)
        }
        if (!data.has_more_after) break
        // An all-invalid page still advances by its truthful scan coverage.
        // (browser-side poison filtering can empty a page the producer filled).
        const next = Number.isInteger(data.coverage_to) ? Math.max(data.coverage_to, bars.at(-1)?.time ?? -Infinity) : bars.at(-1)?.time
        if (next == null || (after != null && next <= after)) throw new Error("History cursor did not advance")
        after = next
    }
    const frameTime = p => p.action === "invalidate" ? p.invalid_time : p.bar.time
    for (const payload of [...buffered.values()].sort((left, right) => frameTime(left) - frameTime(right))) {
        // Invalidations always apply (the catch-up page may predate them).
        // A valid frame at/below the cursor is applied only when it is a
        // candidate repair of a poisoned bucket; applyRealtimeBase then
        // accepts it only if its revision is newer than the invalidation.
        if (payload.action === "invalidate" || frameTime(payload) > closedThrough || invalidPoison.has(frameTime(payload))) applyRealtimePayload(payload)
    }
    page.reachedEnd = true
    return true
}

function ensureModeConnections() {
    if (updatesSuspended || !modeConnectionsReady) return

    if (realtimeEnabled) {
        stopDelayedPolling()
        if (!ws || ws.readyState === WebSocket.CLOSING || ws.readyState === WebSocket.CLOSED) {
            connectWs()
        }
        return
    }

    stopRealtimeSocket()
    if (!delayedPollTimer) {
        delayedPollTimer = setInterval(() => {
            pollDelayedUpdates().catch(err => console.warn("delayed poll failed", err))
        }, CONFIG.DELAY_POLL_MS)
    }
}

function connectWs() {
    stopRealtimeSocket()
    const requestedExchange = exchangeSel.value
    const requestedSymbol = symbolSel.value
    const requestedMarket = marketSel.value
    if (!available(requestedExchange, requestedMarket, requestedSymbol)) throw new Error("Unsupported source selection")
    const proto = location.protocol === "https:" ? "wss" : "ws"
    const url = new URL(`${proto}://${location.host}/ws`)
    url.searchParams.set("exchange", requestedExchange)
    url.searchParams.set("symbol", requestedSymbol)
    url.searchParams.set("market", requestedMarket)
    url.searchParams.set("tf", tfSel.value)
    if (requestedMarket !== "spot") url.searchParams.set("series", sourceSeries(requestedMarket))
    const key = currentAccessKey()
    if (key) url.searchParams.set("key", key)

    const generation = ++wsGeneration
    // Live fences are per connection; results already stored stay valid.
    indicatorStore.resetFences()
    const socket = new WebSocket(url)
    ws = socket
    let syncing = true
    const buffered = new Map()
    let historySyncTimer = null

    const syncHistory = async () => {
        syncing = true
        try {
            if (!await catchUpHistory(generation, buffered)) return
            syncing = false
            buffered.clear()
            statusEl.textContent = !page.bars.length && ["spot_usdc", "usdm_usdc"].includes(requestedMarket)
                ? "no stored history yet" : "live"
        } catch (error) {
            if (generation !== wsGeneration) return
            // A foreign-epoch page (producer restarted; this connection has not
            // yet proven the new epoch with a frame) is rejected whole. Stay in
            // the truthful "resyncing" state: frames keep buffering and the
            // first one adopts the new epoch; retry catch-up on a bounded timer
            // tied to this connection instead of recycling the socket.
            if (error?.foreignEpoch) {
                statusEl.textContent = "resyncing"
                setTimeout(() => { if (generation === wsGeneration && syncing) syncHistory() }, 1000)
                return
            }
            reportHistoryError(error)
            socket.close()
        }
    }

    socket.onopen = async () => {
        if (generation !== wsGeneration) return
        statusEl.textContent = "syncing"
        if (pingTimer) clearInterval(pingTimer)
        pingTimer = setInterval(() => {
            if (ws?.readyState === WebSocket.OPEN) ws.send("ping")
        }, 15000)
        await syncHistory()
        if (generation !== wsGeneration || syncing) return
        historySyncTimer = setInterval(() => {
            if (!syncing) syncHistory()
        }, CONFIG.DELAY_POLL_MS)
    }

    socket.onmessage = ev => {
        if (generation !== wsGeneration) return
        try {
            const payload = JSON.parse(ev.data)
            // Legacy server indicator frames are ignored (browser computes).
            let event = null
            try { event = parseIndicatorFrame(payload) } catch (_) { return }
            if (event) return
            assertSourceSeries(payload, requestedMarket, requestedExchange, requestedSymbol)
            const frameTime = payload?.action === "invalidate" ? payload.invalid_time : payload?.bar?.time
            if (!frameTime) return
            adoptConnectionEpoch(payload, generation)
            if (syncing) {
                // Keep the newest revision per bucket (an older frame never displaces it).
                const held = buffered.get(frameTime)
                if (!held || (payload.rev ?? 0) >= (held.rev ?? 0)) buffered.set(frameTime, payload)
                if (buffered.size > 10000) socket.close()
            } else applyRealtimePayload(payload)
        } catch (error) {
            console.warn("invalid realtime message", error)
            socket.close()
        }
    }

    socket.onclose = () => {
        if (historySyncTimer) clearInterval(historySyncTimer)
        if (generation !== wsGeneration) return
        wsGeneration += 1
        const reconnectGeneration = wsGeneration
        statusEl.textContent = "reconnecting"
        if (pingTimer) { clearInterval(pingTimer); pingTimer = null }
        ws = null
        if (realtimeEnabled) {
            setTimeout(() => {
                if (reconnectGeneration === wsGeneration && realtimeEnabled && !updatesSuspended) connectWs()
            }, 3000)
        }
    }
}

async function reload() {
    // A hover belongs to the old source/timeframe, not the new dataset. This
    // state reset is safe while hidden; the visual crosshair clears on paint.
    hoveredFlowTime = null
    // Plugin status/results belong to the old source/timeframe.
    indicatorStore.reset()
    resetValidity()
    modeConnectionsReady = false
    historyController.abort()
    historyController = new AbortController()
    const generation = ++historyGeneration
    stopDelayedPolling()
    stopRealtimeSocket()
    cancelVisualWork()
    pendingChartRange = null
    initialViewportPending = false
    page.bars = []
    timelineTimes = []
    page.oldestTime = null
    page.newestTime = null
    page.confirmedTime = null
    page.loadingOlder = false
    page.loadingNewer = false
    setBars([])
    lastAfterFetchAt = 0
    lastBeforeFetchAt = 0
    try {
        await loadInitial()
        if (generation === historyGeneration) {
            modeConnectionsReady = true
            ensureModeConnections()
        }
    } catch (error) {
        if (generation === historyGeneration) reportHistoryError(error)
    }
}

function suspendUpdates() {
    if (updatesSuspended) return
    updatesSuspended = true
    stopDelayedPolling()
    stopRealtimeSocket()
    cancelVisualWork()
    statusEl.textContent = realtimeEnabled ? "paused" : "delay 1h"
}

async function resumeUpdates() {
    if (visualWorkSuspended() || !modeConnectionsReady) return
    updatesSuspended = false
    ensureModeConnections()
    // All return events share one paint, including a BFCache restoration.
    queueChartRender()
}

function keepUpdatesActive() {
    updatesSuspended = false
    ensureModeConnections()
}

async function persistAndReload() {
    savePrefs()
    await reload()
}

exchangeSel.onchange = () => {
    updateOptionsForExchange(true)
    persistAndReload()
}
symbolSel.onchange = () => { updateOptionsForSymbol(); persistAndReload() }
marketSel.onchange = () => { updateOptionsForMarket(); persistAndReload() }
tfSel.onchange = () => persistAndReload()
accessKeyInput.oninput = () => savePrefs()
accessKeyInput.onchange = () => persistAndReload()
aggToggle.onchange = () => {
    refreshModeData()
}
screenshotBtn.onclick = () => downloadChartScreenshot()

document.addEventListener("visibilitychange", () => {
    // Keep network + title live, but cancel visual work that Safari may defer.
    keepUpdatesActive()
    if (visualWorkSuspended()) {
        chartNeedsRender = true
        cancelVisualWork()
    } else {
        resumeUpdates().catch(err => console.warn("resume failed", err))
    }
})

window.addEventListener("focus", () => {
    keepUpdatesActive()
    resumeUpdates().catch(err => console.warn("focus resume failed", err))
})

window.addEventListener("pageshow", () => {
    pageAway = false
    startQualityTimer()
    updateQualityReadout()
    keepUpdatesActive()
    resumeUpdates().catch(err => console.warn("pageshow resume failed", err))
})

window.addEventListener("pagehide", () => {
    pageAway = true
    clearInterval(qualityTimer)
    qualityTimer = null
    chartNeedsRender = true
    cancelVisualWork()
})

async function init() {
    const prefs = loadPrefs()
    await loadMeta()
    applyPrefs(prefs)
    if (!prefs) updateOptionsForExchange()
    await loadIndicators()
    prefsHydrated = true
    savePrefs()
    reload()
}

init()
