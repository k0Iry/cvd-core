// Browser application: configuration, DOM controls, preferences, the
// indicator menu, history/WebSocket data flow and chart orchestration.
// Plugin scheduling and accepted indicator values are owned by the Rust
// coordinator (via ../wasm-host/host-bridge.js); this file applies its read-only
// view. Descriptor/renderer/readout presentation lives in ./chart.js.
import * as LightweightCharts from "lightweight-charts"
import { createDualRangeHistogramSeries } from "@tradingview/lwc-plugin-dual-range-histogram-series"
import { activeGroupOutput, buildRegistry, readoutText, createOutputRenderer, createTimeLabelFormatters, mergeHistoryQuality, qualityDisplay, PRICE_UP_COLOR, PRICE_DOWN_COLOR, CURRENT_PRICE_COLOR, barClose, candlePoint } from "./chart.js?v=local-wasm-7"
import { WasmPluginHost, createCoordinator, createLocalCompute, openPluginDb, checkImportFile, classifyImport, sha256Hex, sameBytes, importMessage, validRecord, parseParamInput } from "/wasm-host/host-bridge.js?v=local-wasm-7"

// ============================================================ configuration
const CONFIG = {
    RENDER_WINDOW_SIZE: 3200,
    RENDER_WINDOW_BUFFER: 512,
    // Display page size only. Indicator warm-up is declared by each local
    // plugin (descriptor context) and satisfied by the browser from the shared
    // canonical cache, prepending older pages when needed (maybeWarmup).
    HISTORY_PAGE_SIZE: 1600,
    // Rebuilding every segmented-lines output touches a lot of chart state.
    // Keep realtime candles/line/histogram outputs instant, but throttle full
    // segmented refreshes.
    SEGMENTED_REALTIME_REFRESH_MS: 2000,
    EDGE_TRIGGER: 30,
    FETCH_COOLDOWN: 300,
    DELAY_POLL_MS: 30000,
}

const THEMES = {
    day: {
        bg: "#f6f8fb",
        panel: "#ffffff",
        border: "rgba(0,0,0,0.08)",
        text: "#1d2430",
        muted: "#6b768a",
        accent: "#2f80ed",
        accent2: "#f2994a",
        chartBg: "#f8f9fc",
        grid: "#e4e8ee",
    },
    night: {
        bg: "#0f1115",
        panel: "rgba(255,255,255,0.04)",
        border: "rgba(255,255,255,0.08)",
        text: "#e8ecf2",
        muted: "#8b94a7",
        accent: "#56ccf2",
        accent2: "#f2c94c",
        chartBg: "#11161f",
        grid: "#1f2530",
    },
}

const FALLBACK_EXCHANGE_META = {
    binance: {
        markets: ["spot", "usdm", "coinm"],
        symbols: ["BTC", "ETH"],
    },
    coinbase: {
        markets: ["spot", "usdc"],
        symbols: ["BTC", "ETH"],
    },
}

// Stable source identities. Metadata must match the complete descriptor before
// a new market is offered; legacy fallback deliberately contains no new USDC.
const source = (exchange, market, symbol) => {
    const category = market === "spot" || market === "spot_usdc" ? "spot" : "contract"
    const quote_asset = exchange === "coinbase" ? (market === "spot" ? "USD" : "USDC")
        : market === "coinm" ? "USD" : market.endsWith("_usdc") ? "USDC" : "USDT"
    return { exchange, market, symbol,
        native_symbol: exchange === "coinbase" ? (market === "spot" ? `${symbol}-USD` : `${symbol}_USDC-PERPETUAL`)
            : market === "coinm" ? `${symbol}USD_PERP` : `${symbol}${quote_asset}`,
        category, contract_kind: category === "spot" ? null : market === "coinm" ? "inverse" : "linear",
        quote_asset, settlement_asset: category === "spot" ? null : market === "coinm" ? symbol : quote_asset }
}
const SOURCE_INSTRUMENTS = Object.fromEntries(
    [["binance", ["spot", "spot_usdc", "usdm", "usdm_usdc", "coinm"]], ["coinbase", ["spot", "usdc"]]]
        .flatMap(([exchange, markets]) => markets.flatMap(market => ["BTC", "ETH"].map(symbol => {
            const descriptor = source(exchange, market, symbol)
            return [`${exchange}:${market}:${symbol}`, descriptor]
        }))))
const sourceKey = (exchange, market, symbol) => `${exchange}:${market}:${symbol}`
const sourceSeries = market => market === "spot" || market === "spot_usdc" ? "spot-v1" : "contracts-drb-v1"
const legacySource = descriptor => descriptor.market !== "spot_usdc" && descriptor.market !== "usdm_usdc"
function validInstrument(item) {
    if (!item || typeof item !== "object") return false
    const expected = SOURCE_INSTRUMENTS[sourceKey(item.exchange, item.market, item.symbol)]
    return !!expected && Object.keys(expected).every(field => item[field] === expected[field])
}
const STORAGE_KEY = "cvd_prefs_v1"
const DEFAULT_TITLE = "CVD"

// ============================================================ DOM controls
const elements = Object.fromEntries(Object.entries({ exchangeSel: "exchange", symbolSel: "symbol", marketSel: "market", tfSel: "tf", accessKeyInput: "access-key", aggToggle: "toggle-agg", pluginControlsEl: "plugin-controls", pluginMenuEl: "plugin-menu", pluginStateEl: "plugin-state", screenshotBtn: "screenshot-btn", statusEl: "status", qualityStateEl: "quality-state", sourceSeriesEl: "source-series", chartEl: "chart", paneLabels: "pane-labels" }).map(([key, id]) => [key, document.getElementById(id)]))
const { exchangeSel, symbolSel, marketSel, tfSel, accessKeyInput, aggToggle, pluginControlsEl, pluginMenuEl, pluginStateEl, screenshotBtn, statusEl, qualityStateEl, sourceSeriesEl, chartEl, paneLabels } = elements

// ============================================================ preferences
// One-time migration of pre-plugin preferences (cvd_prefs_v1 written by the
// fixed-indicator UI) into the generic plugin preference shape. This is the
// ONLY place that names historical indicator ids; renderers stay generic.
// Runs only when the stored prefs have no `plugins` block yet; the next save
// writes the new shape, so it never re-applies.
const LEGACY_FLOW_MODES = { "buy-sell": "flow/buy_sell", delta: "flow/delta" }

function migrateLegacyPrefs(prefs) {
    if (!prefs || typeof prefs !== "object" || prefs.plugins) return prefs
    const plugins = { enabled: {}, modes: {} }
    if (typeof prefs.showLevels === "boolean") plugins.enabled.levels = prefs.showLevels
    if (Object.hasOwn(LEGACY_FLOW_MODES, prefs.flowMode)) plugins.modes["flow:flow"] = LEGACY_FLOW_MODES[prefs.flowMode]
    const { showLevels, flowMode, ...rest } = prefs
    return { ...rest, plugins }
}

// ============================================================ indicator menu
// Compact indicator multiselect: one trigger button in the toolbar and one
// popup of native checkbox rows. The popup lives OUTSIDE the toolbar (which
// is overflow:auto for short viewports and would clip an absolute child) and
// is placed with position:fixed from the trigger's rect, clamped to the
// viewport. Rows are keyed by id and updated in place so ordinary rerenders
// keep the popup open and focus where it is. Knows nothing about specific
// plugin ids; callers pass generic row items.
//
// items: [{ id, name, meta, title, checked, disabled, fault?, params? }]
// Local packages: an always-present "Import .wasm…" control (trusted native
// file input, nothing uploaded), a status notice, and per-row Params/Remove.
function createIndicatorMenu({ container, popup, onToggle, onImport = null, onUninstall = null, onParams = null, parseParam = null, label = "Indicators" }) {
    const doc = globalThis.document
    const win = globalThis.window ?? {}
    const make = (tag, className) => { const el = doc.createElement(tag); if (className) el.className = className; return el }

    const trigger = make("button", "toolbar-btn plugin-menu-trigger")
    trigger.type = "button"
    // Disclosure button (aria-expanded/controls) for a labelled checkbox
    // group; deliberately no aria-haspopup/menu roles, whose arrow-key menu
    // interaction model this popup does not implement.
    trigger.setAttribute("aria-expanded", "false")
    trigger.setAttribute("aria-controls", popup.id || "plugin-menu")
    const triggerText = make("span", "plugin-menu-count")
    const caret = make("span", "plugin-menu-caret")
    caret.setAttribute("aria-hidden", "true")
    caret.textContent = "▾"
    trigger.append(triggerText, caret)
    container.replaceChildren(trigger)

    const heading = make("div", "plugin-menu-heading")
    heading.id = (popup.id || "plugin-menu") + "-heading"
    heading.textContent = label
    const list = make("div", "plugin-menu-list")
    list.setAttribute("role", "group")
    list.setAttribute("aria-labelledby", heading.id)
    const empty = make("p", "plugin-menu-empty")
    empty.dataset.empty = ""
    empty.textContent = onImport ? "No local indicators imported" : "No indicators available"
    popup.hidden = true
    const footer = make("div", "plugin-menu-footer")
    const notice = make("p", "plugin-menu-notice")
    notice.setAttribute("role", "status")
    notice.setAttribute("aria-live", "polite")
    let fileInput = null
    if (onImport) {
        fileInput = make("input")
        fileInput.type = "file"
        fileInput.accept = ".wasm,application/wasm"
        fileInput.className = "plugin-import-input"
        fileInput.setAttribute("aria-label", "Import local .wasm indicator")
        fileInput.onchange = () => { const f = fileInput.files?.[0]; fileInput.value = ""; if (f) onImport(f) }
        const importBtn = make("button", "toolbar-btn plugin-import-btn")
        importBtn.type = "button"
        importBtn.textContent = "Import .wasm…"
        importBtn.title = "Import a cvd-wasm v1 indicator from this computer. It stays in this browser; nothing is uploaded."
        importBtn.onclick = () => fileInput.click()
        footer.append(importBtn, fileInput)
    }
    footer.append(notice)
    popup.replaceChildren(heading, list, footer)

    const rows = new Map()
    let order = []
    let open = false

    function row(item) {
        let r = rows.get(item.id)
        if (!r) {
            const el = make("label", "plugin-menu-row")
            el.dataset.plugin = item.id
            const input = make("input")
            input.type = "checkbox"
            const name = make("span", "plugin-menu-name")
            const meta = make("span", "plugin-menu-meta")
            el.append(input, name, meta)
            r = { el, input, name, meta }
            input.onchange = () => r.onchange?.(input.checked)
            if (onUninstall) {
                const actions = make("span", "plugin-menu-actions")
                const paramsBtn = make("button", "plugin-menu-action")
                paramsBtn.type = "button"; paramsBtn.textContent = "Params"; paramsBtn.dataset.action = "params"
                const removeBtn = make("button", "plugin-menu-action")
                removeBtn.type = "button"; removeBtn.textContent = "Remove"; removeBtn.dataset.action = "remove"
                const panel = make("span", "plugin-menu-params")
                panel.hidden = true
                paramsBtn.onclick = ev => { ev.preventDefault?.(); panel.hidden = !panel.hidden; if (!panel.hidden) fillParams(r) }
                removeBtn.onclick = ev => { ev.preventDefault?.(); onUninstall(r.id) }
                actions.append(paramsBtn, removeBtn)
                el.append(actions, panel)
                Object.assign(r, { paramsBtn, removeBtn, panel })
            }
            rows.set(item.id, r)
        }
        r.id = item.id
        r.item = item
        if (r.paramsBtn) r.paramsBtn.hidden = !(item.params?.length)
        if (r.removeBtn) r.removeBtn.setAttribute("aria-label", `Remove ${item.name} from this browser`)
        if (r.el.dataset.fault !== String(!!item.fault)) r.el.dataset.fault = String(!!item.fault)
        // Write only on change: avoids churn and keeps focus stable.
        if (r.el.title !== item.title) r.el.title = item.title
        if (r.name.textContent !== item.name) r.name.textContent = item.name
        if (r.meta.textContent !== (item.meta ?? "")) r.meta.textContent = item.meta ?? ""
        if (r.input.checked !== item.checked) r.input.checked = item.checked
        if (r.input.disabled !== item.disabled) r.input.disabled = item.disabled
        r.el.dataset.state = item.disabled ? "unavailable" : "ready"
        r.onchange = item.disabled ? null : checked => onToggle(item.id, checked)
        return r.el
    }

    // Generic parameter form from descriptor ParamSpec (int/float/bool/choice).
    function fillParams(r) {
        const specs = r.item?.params ?? []
        const fields = specs.map(spec => {
            const wrap = make("label", "plugin-param")
            const caption = make("span"); caption.textContent = spec.label || spec.key
            let input
            if (spec.type === "choice") {
                input = make("select")
                for (const option of spec.options ?? []) { const o = make("option"); o.value = option; o.textContent = option; input.append(o) }
                input.value = spec.value
            } else {
                input = make("input")
                if (spec.type === "bool") { input.type = "checkbox"; input.checked = spec.value === true }
                else { input.type = "number"; input.min = spec.min; input.max = spec.max; input.step = spec.type === "int" ? "1" : "any"; input.value = String(spec.value) }
            }
            input.dataset.param = spec.key
            wrap.append(caption, input)
            return { spec, input, wrap }
        })
        const apply = make("button", "plugin-menu-action"); apply.type = "button"; apply.textContent = "Apply"; apply.dataset.action = "apply"
        const err = make("span", "plugin-param-error"); err.setAttribute("role", "alert")
        apply.onclick = ev => {
            ev.preventDefault?.()
            const values = {}
            for (const { spec, input } of fields) values[spec.key] = parseParam(spec, spec.type === "bool" ? input.checked : input.value)
            // onParams approves synchronously (Rust, strict) and throws on rejection.
            try { onParams?.(r.id, values) } catch (e) { err.textContent = String(e?.message ?? e); return }
            err.textContent = ""
        }
        r.panel.replaceChildren(...fields.map(f => f.wrap), apply, err)
    }

    function render(items, selected, status = null) {
        if (status && notice.textContent !== (status.text ?? "")) { notice.textContent = status.text ?? ""; notice.dataset.kind = status.kind ?? "" }
        const ids = items.map(item => item.id)
        const els = items.map(row)
        for (const id of [...rows.keys()]) if (!ids.includes(id)) rows.delete(id)
        const next = els.length ? els : [empty]
        if (next.length !== order.length || next.some((el, i) => el !== order[i])) {
            // Move only out-of-place nodes: re-inserting a focused row would
            // blur it, so unchanged rows stay put across discovery updates.
            if (typeof list.insertBefore === "function") {
                next.forEach((el, i) => { if (list.children[i] !== el) list.insertBefore(el, list.children[i] ?? null) })
                while (list.children.length > next.length) list.lastChild.remove()
            } else list.replaceChildren(...next)
            order = next
        }
        const text = `${label} · ${selected}`
        if (triggerText.textContent !== text) triggerText.textContent = text
        trigger.title = `${selected} of ${items.filter(item => !item.disabled).length} indicators selected`
        if (open) position()
    }

    // Fixed placement from the trigger rect, clamped to the VISIBLE region:
    // visualViewport origin (offsetLeft/Top, non-zero under pinch-zoom/pan)
    // and extent. Falls back to the layout viewport when unsupported.
    function position() {
        const vv = win.visualViewport
        const view = vv ? { x: vv.offsetLeft ?? 0, y: vv.offsetTop ?? 0, width: vv.width, height: vv.height }
            : { x: 0, y: 0, width: win.innerWidth ?? 0, height: win.innerHeight ?? 0 }
        const layoutH = win.innerHeight ?? view.height
        if (!view.width || !view.height || typeof trigger.getBoundingClientRect !== "function") return
        const p = placeIndicatorMenu(trigger.getBoundingClientRect(), view, layoutH)
        Object.assign(popup.style, {
            width: `${p.width}px`, maxHeight: `${p.maxHeight}px`, left: `${p.left}px`,
            top: p.up ? "auto" : `${p.top}px`, bottom: p.up ? `${p.bottomPx}px` : "auto",
        })
    }

    function setOpen(next, { focusTrigger = false } = {}) {
        if (open === next) return
        open = next
        popup.hidden = !open
        trigger.setAttribute("aria-expanded", String(open))
        trigger.classList.toggle?.("active", open)
        if (open) position()
        if (!open && focusTrigger) trigger.focus?.()
    }

    const inside = node => popup.contains?.(node) || trigger.contains?.(node)
    function handlePointer(event) { if (open && !inside(event.target)) setOpen(false) }
    function handleKeydown(event) {
        if (!open) return
        if (event.key === "Escape") { event.preventDefault?.(); setOpen(false, { focusTrigger: true }); return }
        if (event.key !== "Tab") return
        // First/last come from the CURRENT rendered order (not Map order).
        const enabled = order.map(rowInput).filter(input => input && !input.disabled)
        // With local-package controls, every visible focusable control in the
        // popup participates (rows, Params/Remove, Import).
        if (onImport && typeof popup.querySelectorAll === "function") {
            enabled.splice(0, enabled.length, ...[...popup.querySelectorAll("input:not([type=file]), button, select")]
                .filter(el => !el.disabled && !el.hidden && !el.closest?.("[hidden]")))
        }
        const first = enabled[0], last = enabled.at(-1)
        if (event.target === trigger) {
            // The popup sits after the toolbar in the DOM: Tab from the
            // trigger enters it; Shift+Tab, or nothing enabled, leaves
            // natively and closes.
            if (!event.shiftKey && first) { event.preventDefault?.(); first.focus(); return }
            setOpen(false)
        } else if (event.shiftKey && event.target === first) {
            event.preventDefault?.(); trigger.focus()
        } else if (!event.shiftKey && event.target === last) {
            // Continue with the toolbar control after the trigger (e.g. Agg).
            const next = nextFocusable(trigger)
            setOpen(false)
            if (next) { event.preventDefault?.(); next.focus() }
        }
    }
    const rowInput = el => [...rows.values()].find(r => r.el === el)?.input
    function nextFocusable(from) {
        if (typeof doc.querySelectorAll !== "function") return null
        const all = [...doc.querySelectorAll("button, input, select, textarea, a[href], [tabindex]")].filter(el =>
            el === from || (!el.disabled && el.tabIndex >= 0 && !popup.contains(el) && el.getClientRects().length > 0))
        const i = all.indexOf(from)
        return i >= 0 ? all[i + 1] ?? null : null
    }
    function handleFocusOut(event) { if (open && event.relatedTarget && !inside(event.relatedTarget)) setOpen(false) }

    trigger.onclick = () => setOpen(!open)
    // Document-level so Escape still closes after a discovery update removed
    // the focused row (focus falls back to <body>).
    doc.addEventListener?.("keydown", handleKeydown)
    popup.addEventListener?.("focusout", handleFocusOut)
    doc.addEventListener?.("pointerdown", handlePointer, true)
    const reposition = () => { if (open) position() }
    win.addEventListener?.("resize", reposition)
    win.visualViewport?.addEventListener?.("resize", reposition)
    win.visualViewport?.addEventListener?.("scroll", reposition) // pinch-zoom pan
    doc.addEventListener?.("scroll", reposition, true) // includes toolbar scroll

    return { render, trigger, handleKeydown, handlePointer, isOpen: () => open, close: () => setOpen(false) }
}

// Pure placement math (unit-tested with synthetic viewports). rect: trigger
// client rect; view: visible region {x, y, width, height} in layout-viewport
// coordinates; layoutH: layout viewport height (for CSS `bottom`).
function placeIndicatorMenu(rect, view, layoutH) {
    const gap = 4, margin = 8
    const vLeft = view.x, vTop = view.y, vRight = view.x + view.width, vBottom = view.y + view.height
    const width = Math.min(340, view.width - margin * 2)
    const below = vBottom - Math.max(rect.bottom, vTop) - gap - margin
    const above = Math.min(rect.top, vBottom) - vTop - gap - margin
    const up = below < 180 && above > below
    const maxHeight = Math.max(80, Math.min(420, up ? above : below, view.height - margin * 2))
    const left = Math.round(Math.min(Math.max(vLeft + margin, rect.left), vRight - width - margin))
    const top = Math.round(Math.max(vTop + margin, Math.min(rect.bottom + gap, vBottom - margin - maxHeight)))
    const anchor = Math.min(vBottom - margin, Math.max(vTop + margin + maxHeight, rect.top - gap))
    return { up, width, maxHeight, left, top, bottomPx: Math.round(layoutH - anchor) }
}

// ============================================================ application

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
// Bars carry the canonical inclusive bucket-end second; label them by bar
// open time (display only). labelTf is the tf the chart data was rendered
// with, falling back to the selector before the first render.
let labelTf = null
const timeLabels = createTimeLabelFormatters(() => labelTf ?? tfSel.value)
chart.applyOptions({ localization: { timeFormatter: timeLabels.timeFormatter } })
chart.timeScale().applyOptions({ tickMarkFormatter: timeLabels.tickMarkFormatter })

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
const currentMode = () => useAggregateMode() ? "aggregate" : "source"
// Renderers read results by canonical bar time; base bars are never modified.
// Browser-computed results ONLY (server indicator outputs are ignored).
// READ-ONLY projection of the Rust coordinator's accepted values
// (`${outputKey}|${mode}` -> Map(time -> value)) for synchronous LWC reads.
// Merge, history/live precedence, stale cutoff, changed times, invalidation
// and drop are decided in Rust (sched.rs); only applyLocalView writes here.
let localValues = new Map()
function applyLocalView(view) {
    if (!view) return
    if (view.clear) { localValues = new Map(); forEachRenderer(r => { r.dirty = true }) }
    for (const k of view.drop ?? []) localValues.delete(k)
    for (const [k, { set, del }] of Object.entries(view.cols ?? {})) {
        let col = localValues.get(k)
        if (!col) { if (!set.length) continue; localValues.set(k, col = new Map()) }
        for (const t of del) col.delete(t)
        for (const [t, v] of set) col.set(t, v)
    }
}
const storeValue = (bar, key, aggregate) => bar ? localValues.get(`${key}|${aggregate ? "aggregate" : "source"}`)?.get(bar.time) : undefined
// Newest live base WS `rev` per bar in this connection epoch (revs are per
// socket): a lower-rev frame for the same bar is stale. Pruned only with the
// canonical cache, cleared on reload; reconnect starts a new epoch.
const baseRevs = new Map()   // time -> { epoch, rev }
let baseEpoch = 0
function noteBaseRev(payload) {
    const time = payload?.bar?.time, rev = payload?.rev
    if (!Number.isSafeInteger(rev) || rev < 0 || !Number.isSafeInteger(time)) return
    const cur = baseRevs.get(time)
    if (!(cur?.epoch === baseEpoch && cur.rev >= rev)) baseRevs.set(time, { epoch: baseEpoch, rev })
}
function baseSuperseded(time, rev) {
    const b = baseRevs.get(time)
    return !!b && b.epoch === baseEpoch && Number.isSafeInteger(rev) && rev < b.rev
}
function pruneBaseRevs(time) {
    if (Number.isSafeInteger(time)) for (const t of [...baseRevs.keys()]) if (t < time) baseRevs.delete(t)
}
const renderDeps = { LightweightCharts, createDualRangeHistogramSeries, outputValue: storeValue }

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
let localCoord = null
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
    // A scope change clears the accepted values in Rust (view.clear via onView).
    publishLocalDemand(scope, scopeKey)
}
function publishLocalDemand(scope = scopeNow(), scopeKey = JSON.stringify(scope)) {
    if (!localCompute) return
    localCompute.setDemand({ scope, scopeKey, revision: dataRevision, historyRevision, bars: page.bars,
        invalidTimes: [...invalidPoison.keys()].sort((a, b) => a - b),
        exhausted: !!page.reachedStart, ...canonicalRequestedRange(page.bars) })
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
// Warm-up pages ([from, to] times) fetched as context only. Once the user's
// view exposes one, it becomes a requested older page with its own producer
// basis (exactly what a user-driven fetch of it would have been), so valid
// cached candles never stay blank merely because they arrived as warm-up.
let localWarmupPages = []
function promoteExposedWarmup(fromIndex) {
    if (!localWarmupPages.length || !page.bars.length) return
    const t = page.bars[Math.max(0, Math.min(page.bars.length - 1, Math.floor(fromIndex)))].time
    const exposed = localWarmupPages.filter(p => p.to >= t)
    if (!exposed.length) return
    localWarmupPages = localWarmupPages.filter(p => p.to < t)
    localOlderPages.push(...exposed)
    publishLocalDemand()
}
function canonicalRequestedRange(bars) {
    const n = bars.length
    if (!n) return { wantFromIndex: 0, wantToIndex: 0, olderRanges: [] }
    if (localRequestedFrom == null) localRequestedFrom = bars[0].time
    const olderRanges = localOlderPages.map(({ from, to }) => ({ fromIndex: lowerBoundByTime(bars, from), toIndex: lowerBoundByTime(bars, to + 1), from, to }))
        .filter(r => r.toIndex > r.fromIndex)
    return { wantFromIndex: lowerBoundByTime(bars, localRequestedFrom), wantToIndex: n, olderRanges }
}


// Invalidate derived values at bar times whose canonical input is about to
// change, so the new base is never painted with an old derived value. The
// withdrawal policy (bucket + every enabled plugin at/after the earliest
// bucket) is Rust's; the diff is applied synchronously, before the write.
function invalidateLocalAt(times) {
    if (times.length && localCompute) applyLocalView(localCompute.invalidate(times))
}

function syncLocalCompute() {
    if (!localCompute) return
    localCompute.setPlugins([...localRecords.values()].filter(r => r.enabled).map(r => ({
        id: r.id, bytes: r.bytes, params: r.params, loadKey: `${r.sha256}|${JSON.stringify(r.params)}`,
    })))
}

const localCoverage = new Map()
// Package replace / params change / disable: Rust drops the accepted values.
function dropLocalValues(id) {
    localCoverage.delete(id)
    if (localCompute) applyLocalView(localCompute.dropValues(id))
}

// Rust-approved batch for one accepted compute: apply to the projection,
// then paint (rendering scope only: shape change = rebuild, else the changed
// times inside the rendered window).
function onLocalResult(id, mode, view, want, meta = {}) {
    if (meta.coverage) localCoverage.set(id, { coverage: meta.coverage, needBefore: meta.needBefore ?? 0 })
    applyLocalView(view)
    localErrors.delete(id)
    if (view.shape || view.drop?.length) {
        forEachRenderer(renderer => { if (renderer.output.pluginId === id) renderer.dirty = true })
        refreshPluginOutputs(new Set([id]), true)
    } else {
        const lo = renderedWindow.bars[0]?.time, hi = renderedWindow.bars.at(-1)?.time
        const visible = visualWorkSuspended() || lo === undefined ? view.changed : view.changed.filter(t => t >= lo && t <= hi)
        if (visible.length) refreshPluginOutputs(new Set([id]), false, visible)
        else updateFlowReadout()
    }
    maybeWarmup()
}
// Standalone Rust view changes (fault/membership drop, scope clear).
function onLocalView(id, view) {
    applyLocalView(view)
    if (id != null) refreshPluginOutputs(new Set([id]), true)
}

function onLocalStatus(id, st) {
    if (st.state === "fault") { localCoverage.delete(id); refreshPluginOutputs(new Set([id]), true) }
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

// The ONE descriptor/params authority: the Rust coordinator (SDK
// Descriptor::validate + resolve_params), called only at restore, import/
// replace and form-edit boundaries. Lenient = restore/migration (unknown keys
// dropped, invalid fields defaulted); strict = form edit (throws). Without a
// coordinator nothing is approved (fail closed).
function approveMeta(descriptor, params, strict = false) {
    if (!localCoord) throw Object.assign(new Error("local plugin coordinator unavailable"), { code: "invalid_descriptor" })
    return localCoord.approve(descriptor, params, strict)
}

function registryFromRecords() {
    applyPluginDescriptors([...localRecords.values()].map(r => ({ id: r.id, status: "ready", descriptor: r.descriptor, error: null })), localUnavailable)
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
        const coordBytes = await (await fetch("/wasm-host/cvd_browser_coordinator.wasm", { cache: "no-cache" })).arrayBuffer()
        localCoord = await createCoordinator(coordBytes)
        const hostBytes = await (await fetch("/wasm-host/cvd_browser_host.wasm", { cache: "no-cache" })).arrayBuffer()
        localHost = await WasmPluginHost.create(hostBytes, { workerUrl: new URL("/wasm-host/plugin-worker.js", location.origin) })
        localCompute = createLocalCompute({ host: localHost, coord: localCoord, onResult: onLocalResult, onStatus: onLocalStatus, onInvalid: onLocalInvalid, onView: onLocalView, log: (...a) => console.warn(...a) })
    } catch (error) {
        console.warn("local plugin host unavailable", error)
        localUnavailable = "local plugin host unavailable (unsupported or failed to load)"
    }
    try {
        localDb = await openPluginDb()
        let invalidRestored = 0
        for (const r of await localDb.all()) {
            // Browser envelope/identity (validRecord), then Rust approval of the
            // stored descriptor + params. No Worker is started to inspect it.
            try {
                if (!validRecord(r)) throw new Error("invalid record")
                const approved = approveMeta(r.descriptor, r.params)
                if (approved.descriptor.id !== r.id) throw new Error("descriptor id mismatch")
                r.descriptor = approved.descriptor
                r.params = approved.params
            } catch (_) { invalidRestored++; continue }
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
        let approved = approveMeta(desc, null)
        const decision = classifyImport([...localRecords.values()], { id: approved.descriptor.id, sha256 })
        if (decision.action === "same" && sameBytes(decision.previous.bytes, bytes)) { setImportNotice("info", `${desc.name || desc.id} ${desc.version} is already imported`); return }
        const previous = decision.previous
        // Replacement: validate the ACTUAL migrated params against the new
        // package (plugin create) BEFORE the durable swap; on rejection the
        // previous package stays installed and running unchanged.
        if (previous) approved = approveMeta(desc, previous.params)
        const migrated = approved.params
        if (previous) {
            const check2 = probeInstance++
            try { await localHost.load(check2, bytes, JSON.stringify(migrated)) }
            catch (error) { throw Object.assign(new Error("replacement rejects current parameters"), { code: error?.code === "plugin_error" || error?.code === "invalid_params" ? "replace_params" : error?.code }) }
            finally { try { localHost.disable(check2) } catch (_) {} }
        }
        const record = {
            id: desc.id, version: String(desc.version), name: String(desc.name || desc.id), sha256, size: bytes.byteLength,
            fileName: file.name.slice(0, 120), bytes, descriptor: approved.descriptor,
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
    let params
    // Re-approved strictly against the CURRENT descriptor (a queued replacement
    // may have changed it); rejection writes nothing and changes nothing.
    try { params = approveMeta(record.descriptor, values, true).params } catch (error) {
        setImportNotice("error", `${record.name}: ${String(error?.message ?? error)} — previous parameters kept`)
        return
    }
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
        onImport: file => importLocalFile(file), onUninstall: id => uninstallLocalPlugin(id),
        onParams: (id, values) => { const record = localRecords.get(id); if (record) setLocalParams(id, approveMeta(record.descriptor, values, true).params) },
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
        // A dirty renderer is filled from the current window by syncPluginSeries
        // below; never attempt an incremental update on its stale contents.
        if (statusChanged || !incremental || renderer.output.throttled || renderer.dirty || !outputActive(renderer.output)) { renderer.dirty = true; return }
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

const closeForBar = bar => barClose(bar, useAggregateMode())
const pricePointForBar = (bar, previousBar = null) => candlePoint(bar, previousBar, useAggregateMode())
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
    pruneBaseRevs(page.oldestTime)
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
    labelTf = tfSel.value
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
// The withdrawal itself was applied in Rust (synchronously against the current demand).
function onLocalInvalid(id, mode, want, range, view) {
    applyLocalView(view)
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
    const index = lowerBoundByTime(bars, time)
    return bars[index]?.time === time ? index : -1
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
    applyRealtimeBase(payload)
}

function applyRealtimeBase(payload) {
    assertSourceSeries(payload, marketSel.value)
    const b = payload?.bar
    if (!b?.time) return
    // A live base frame predating an accepted historical replacement of this
    // bar is stale: displaying it would detach the bar from its results.
    if (baseSuperseded(b.time, payload?.rev)) return
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
    noteBaseRev(payload)

    const last = page.bars[page.bars.length - 1]
    const sameShown = (last?.time === b.time && equalBarData(last, b)) ||
        (last && b.time < last.time && equalBarData(page.bars[barIndexByTime(page.bars, b.time)], b))
    if (sameShown) return
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
    pruneBaseRevs(page.oldestTime)
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
    localWarmupPages = []
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
        if (newBars.length) (forWarmup ? localWarmupPages : localOlderPages).push({ from: newBars[0].time, to: newBars.at(-1).time })

        // setMergedBars reanchors the current range by timestamp (no pre-shift)
        const reachedEnd = page.reachedEnd
        applyHistoryPage({ ...data, bars: merged }, currentChartRange(), older)
        page.reachedEnd = reachedEnd
        if (forWarmup) { const r = chart.timeScale().getVisibleLogicalRange(); if (r) promoteExposedWarmup(r.from) }
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
    promoteExposedWarmup(r.from)

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
    baseEpoch++
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
            if (typeof payload?.type === "string" && payload.type.startsWith("indicator")) return
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
    baseRevs.clear()
    baseEpoch++
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
