import { CONFIG, DEFAULT_TITLE, FALLBACK_EXCHANGE_META, SOURCE_INSTRUMENTS, sourceKey, sourceSeries, legacySource, validInstrument, STORAGE_KEY, THEMES } from "./config.js?v=usdc-ui-1"
import { elements } from "./dom.js?v=usdc-ui-1"
import * as LightweightCharts from "lightweight-charts"
import { createDualRangeHistogramSeries } from "@tradingview/lwc-plugin-dual-range-histogram-series"
import { mergeHistoryQuality, qualityDisplay } from "./quality-display.mjs?v=usdc-ui-1"
import { createSegmentHistoryView, segmentData, segmentSlotData, sameSegmentData } from "./segmented-series.mjs?v=usdc-ui-1"
import { DISCOVERY_PATH, historyResults, parseDiscovery, parseIndicatorFrame } from "./plugin-wire.mjs?v=usdc-ui-1"
import { createIndicatorStore } from "./plugin-store.mjs?v=usdc-ui-1"
import { migrateLegacyPrefs } from "./legacy-prefs.mjs?v=usdc-ui-1"
import { activeGroupOutput, buildRegistry, readoutText } from "./plugin-registry.mjs?v=usdc-ui-1"
import { createOutputRenderer } from "./plugin-render.mjs?v=usdc-ui-1"

const {
    exchangeSel,
    symbolSel,
    marketSel,
    tfSel,
    accessKeyInput,
    aggToggle,
    pluginControlsEl,
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
let pluginStatus = {}              // "<id>:<mode>" -> { state: "error", code } from history/live results
let pluginPrefs = { enabled: {}, modes: {} }
let discoveryError = ""
let paneSummaryEls = []
let pluginLayoutGeneration = 0
const indicatorStore = createIndicatorStore()
const currentMode = () => useAggregateMode() ? "aggregate" : "source"
// Renderers read results by canonical bar time; base bars are never modified.
const storeValue = (bar, key, aggregate) => bar ? indicatorStore.get(key, aggregate ? "aggregate" : "source", bar.time) : undefined
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
    return Object.hasOwn(pluginPrefs.enabled, plugin.id) ? pluginPrefs.enabled[plugin.id] : plugin.defaultEnabled
}

// Latest result status for the current mode (history page or live frame).
// Any fault clears and labels the outputs; nothing stale stays drawn.
function pluginFault(plugin) {
    return pluginStatus[`${plugin.id}:${currentMode()}`] ?? null
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

// Discovery: once at startup, then a bounded backoff refresh while any
// configured indicator is still starting/unavailable, or when a live event
// names an indicator we do not have ready. Never permanent invisibility.
const DISCOVERY_RETRY_MS = [1000, 2000, 4000, 8000, 15000, 30000]
let discoveryAttempt = 0
let discoveryTimer = null
let discoveryInFlight = null

function scheduleDiscoveryRefresh(reset = false) {
    if (reset) discoveryAttempt = 0
    if (discoveryTimer || discoveryAttempt >= DISCOVERY_RETRY_MS.length) return
    discoveryTimer = setTimeout(() => {
        discoveryTimer = null
        loadIndicators().catch(() => {})
    }, DISCOVERY_RETRY_MS[discoveryAttempt++])
}

async function loadIndicators() {
    if (discoveryInFlight) return discoveryInFlight
    discoveryInFlight = (async () => {
        let changed = false
        try {
            const response = await fetch(DISCOVERY_PATH, { cache: "no-store" })
            if (!response.ok) throw new Error(`indicators HTTP ${response.status}`)
            changed = applyPluginDescriptors(parseDiscovery(await response.json()))
        } catch (error) {
            console.warn("indicator discovery failed; price-only chart", error)
            changed = applyPluginDescriptors([], String(error?.message ?? error))
        }
        if (registry.pending.some(p => p.status === "starting" || p.status === "unavailable") || discoveryError) scheduleDiscoveryRefresh()
        else discoveryAttempt = 0
        // Newly ready indicators need results for the cached page.
        if (changed && page.bars.length && modeConnectionsReady) resyncIndicators()
        return changed
    })()
    try { return await discoveryInFlight } finally { discoveryInFlight = null }
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

// Toolbar toggles: one per ready plugin plus disabled placeholders for
// configured-but-not-ready ones, generated from discovery.
function renderPluginControls() {
    renderPluginState()
    if (!pluginControlsEl || typeof document.createElement !== "function") return
    const pill = (id, text, title, checked, disabled, onchange) => {
        const label = document.createElement("label")
        label.className = "toggle-pill"
        label.dataset.plugin = id
        label.title = title
        const input = document.createElement("input")
        input.type = "checkbox"
        input.checked = checked
        input.disabled = disabled
        if (onchange) input.onchange = () => onchange(input.checked)
        label.append(input, ` ${text}`)
        return label
    }
    pluginControlsEl.replaceChildren(
        ...registry.plugins.map(plugin => {
            const fault = pluginFault(plugin)
            return pill(plugin.id, plugin.label, `${plugin.label} ${plugin.version}${plugin.description ? " — " + plugin.description : ""}${fault ? ` (${faultText(fault)})` : ""}`,
                pluginEnabled(plugin), false, checked => setPluginEnabled(plugin.id, checked))
        }),
        ...registry.pending.map(item => pill(item.id, `${item.id} (${item.status})`, `${item.id}: ${item.status}${item.error?.code ? " " + item.error.code : ""}`, false, true, null)),
    )
    renderPaneLabels()
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

function setPluginEnabled(pluginId, enabled) {
    pluginPrefs.enabled[pluginId] = enabled
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

function setPluginFault(id, mode, fault) {
    const key = `${id}:${mode}`
    const before = pluginStatus[key]
    if (fault) pluginStatus = { ...pluginStatus, [key]: { state: fault.state ?? "unavailable", code: fault.code ?? "" } }
    else if (before) { pluginStatus = { ...pluginStatus }; delete pluginStatus[key] }
    return JSON.stringify(before ?? null) !== JSON.stringify(pluginStatus[key] ?? null)
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

// History page (generation-checked by fetchHistory): store columnar results
// and per-mode status. Pages are stateless snapshots of their bar range.
function applyHistoryIndicators(payload) {
    const ids = new Set()
    let statusChanged = false
    const newest = page.newestTime
    // Base bars and derived results are ONE admission decision: a history bar
    // whose canonical tuple differs from the displayed one will replace it in
    // mergeHistoryBars, so prior live proof for that time is invalidated and
    // this response's results become authoritative for it. Unchanged tuples
    // keep their verified live results.
    const changed = historyChangedTimes(payload?.bars || [])
    for (const time of changed) indicatorStore.supersedeBase(time)
    for (const result of historyResults(payload)) {
        ids.add(result.id)
        if (result.ok) {
            indicatorStore.applyHistory(result.id, result.mode, result.output, newest, changed)
            statusChanged = setPluginFault(result.id, result.mode, null) || statusChanged
        } else {
            indicatorStore.dropStream(result.id, result.mode)
            statusChanged = setPluginFault(result.id, result.mode, { state: "unavailable", code: result.error.code }) || statusChanged
        }
    }
    if (ids.size) refreshPluginOutputs(ids, statusChanged)
}

function historyChangedTimes(bars) {
    const changed = new Set()
    for (const bar of bars) {
        const index = barIndexByTime(page.bars, bar.time)
        if (index < 0) continue
        const existing = page.bars[index]
        if (!equalBarData(existing, mergeHistoryQuality(existing, bar))) changed.add(bar.time)
    }
    return changed
}

// Live typed frames from the current socket only (caller fences connection).
function handleIndicatorEvent(event) {
    if (event.type === "reset") {
        // Hub resynchronised or this viewer lagged: drop live state older than
        // the new generation and refetch results for the cached page.
        indicatorStore.reset(event.generation)
        forEachRenderer(renderer => { renderer.dirty = true })
        resyncIndicators()
        return
    }
    const known = registry.plugins.some(p => p.id === event.id)
    if (!known) { scheduleDiscoveryRefresh(true); return }
    if (event.type === "status") {
        if (!indicatorStore.applyStatus(event)) return
        const changed = setPluginFault(event.id, event.mode, event.ok ? null : { state: "unavailable", code: event.error.code })
        refreshPluginOutputs(new Set([event.id]), changed)
        return
    }
    const result = indicatorStore.applyFrame(event, page.newestTime)
    if (!result.accepted) return
    const changed = setPluginFault(event.id, event.mode, null)
    refreshPluginOutputs(new Set([event.id]), changed, result.touched)
    if (result.gap) resyncIndicators()
}

// Refetch the newest page (with indicators) to rebuild results after a reset,
// sequence gap or newly ready indicator. Bounded: one in flight.
let indicatorResync = null
function resyncIndicators() {
    if (indicatorResync || !modeConnectionsReady) return indicatorResync
    const generation = historyGeneration
    indicatorResync = (async () => {
        try {
            const data = await fetchHistory(historyPageSize())
            if (generation !== historyGeneration) return
            const merged = mergeHistoryBars(data.bars || [])
            const reachedStart = page.reachedStart
            applyHistoryPage({ ...data, bars: merged }, currentChartRange(), data.bars || [])
            page.reachedStart = reachedStart
        } catch (error) { reportHistoryError(error) }
        finally { indicatorResync = null }
    })()
    return indicatorResync
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

function setBars(bars, keepRange = null) {
    keepRange = keepRange ?? pendingChartRange ?? (initialViewportPending ? null : currentChartRange())
    page.bars = bars
    page.oldestTime = page.bars[0]?.time ?? null
    indicatorStore.pruneBefore(page.oldestTime)
    page.newestTime = page.bars[page.bars.length - 1]?.time ?? null
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
    applyHistoryIndicators(data)
    return data
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
    setMergedBars(data.bars || [], keepRange)
    recordConfirmedHistory(confirmedBars)
    page.reachedStart = !data.has_more_before
    page.reachedEnd = !data.has_more_after
    realtimeEnabled = !!data.realtime_enabled
    statusEl.textContent = !page.bars.length && ["spot_usdc", "usdm_usdc"].includes(marketSel.value)
        ? "no stored history yet" : realtimeEnabled ? "live" : "delay 1h"
    updateDocumentTitle()
}

function applyRealtimePayload(payload) {
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
            setBars(page.bars, currentChartRange())
        }
        return
    }
    applyTailBars([b])
    if (statusEl.textContent === "no stored history yet") statusEl.textContent = "live · history starting"
}

function applyTailBars(bars, keepRange = null) {
    const suspended = visualWorkSuspended() || chartNeedsRender
    const paintsTail = renderedWindow.start + renderedWindow.bars.length === page.bars.length
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

async function loadMoreBefore() {
    if (page.loadingOlder || page.reachedStart) return
    if (!page.oldestTime) return
    const now = Date.now()
    if (now - lastBeforeFetchAt < CONFIG.FETCH_COOLDOWN) return
    lastBeforeFetchAt = now

    page.loadingOlder = true
    const generation = historyGeneration
    try {
        const data = await fetchHistory(historyPageSize(), page.oldestTime, null)
        const older = data.bars || []
        if (!older.length) { page.reachedStart = true; return }

        const newBars = older.filter(b => b.time < page.oldestTime)
        const merged = [...newBars, ...page.bars]

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
        const data = await fetchHistory(historyPageSize(), null, page.confirmedTime ?? page.newestTime)
        const newer = data.bars || []
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
        const data = await fetchHistory(historyPageSize(), null, page.newestTime)
        const newer = data.bars || []
        if (!newer.length) {
            page.reachedEnd = !data.has_more_after
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
        const next = bars[bars.length - 1]?.time
        if (next == null || (after != null && next <= after)) throw new Error("History cursor did not advance")
        after = next
    }
    for (const payload of [...buffered.values()].sort((left, right) => left.bar.time - right.bar.time)) {
        if (payload.bar.time > closedThrough) applyRealtimePayload(payload)
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
            // Typed indicator frames are independent of base bar frames: a
            // bad/unknown indicator frame never blocks or closes base data.
            let event = null
            try { event = parseIndicatorFrame(payload) } catch (error) {
                console.warn("invalid indicator frame", error)
                resyncIndicators()
                return
            }
            if (event) { handleIndicatorEvent(event); return }
            assertSourceSeries(payload, requestedMarket, requestedExchange, requestedSymbol)
            if (!payload?.bar?.time) return
            if (syncing) {
                buffered.set(payload.bar.time, payload)
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
    if (Object.keys(pluginStatus).length) { pluginStatus = {}; renderPluginControls() }
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
