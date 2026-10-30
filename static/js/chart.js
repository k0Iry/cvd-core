// Chart and readout presentation (no DOM, no network, no plugin policy).
// Values arrive as the app's read-only projection of the Rust coordinator's
// accepted results; this module only turns descriptors and values into
// panes, LWC series writes, readout text and display-only time labels.
// LightweightCharts and the dual-range plugin are injected by the app
// (`deps`), so this file stays importable by Node tests.

// ------------------------------------------------------------ Descriptor registry
// Generic, descriptor-driven indicator registry. Descriptors arrive already
// APPROVED by the Rust coordinator (`approve`: SDK Descriptor::validate) at
// restore/import; this projection adds no schema/type/range policy of its own,
// only display sanitising (text length, CSS colour/width, label fallbacks).
// No plugin ID is special here: panes, controls, exclusive modes, styles and
// readouts all derive from the descriptor.
//
// UI policies (generic, documented in docs/frontend-plugins.md):
//  * pane "price" overlays the host Price pane; pane "separate" gets one pane
//    per (plugin, pane_group ?? output key).
//  * In a separate pane, bar-type outputs (histogram, dual_range) are
//    alternatives: if there are two or more, the pane label shows a mode
//    select and only the selected one is drawn (overlapping bar series are
//    unreadable). Line outputs always draw.
//  * colors: line [stroke]; histogram [up, down]; dual_range [upper, lower];
//    segmented_lines one colour per layer (cycled).
//  * widths (SDK OutputSpec.widths): empty = renderer default (native LWC
//    line width for lines, 1 for segmented layers); one value = whole output;
//    one per layer for segmented_lines.
//  * readout_only line outputs are never drawn: they only appear in their
//    pane's readout (e.g. a Total next to whichever visual mode is active).

const COLOR_RE = /^(#[0-9a-fA-F]{3,8}|rgba?\([0-9., ]+\))$/
const DEFAULT_COLORS = ["#56ccf2", "#f2994a", "#bb6bd9", "#27ae60", "#eb5757", "#f2c94c"]

function text(value, fallback, max = 80) {
    return typeof value === "string" && value.trim() ? value.trim().slice(0, max) : fallback
}
function widthAt(widths, index, fallback) {
    const value = Array.isArray(widths) && widths.length ? widths[widths.length === 1 ? 0 : index] : undefined
    return Number.isInteger(value) && value >= 1 && value <= 16 ? value : fallback
}
function colorAt(colors, index, fallback) {
    const value = Array.isArray(colors) && colors.length ? colors[index % colors.length] : undefined
    return typeof value === "string" && COLOR_RE.test(value) ? value : fallback
}

export function normalizeOutput(plugin, raw, order) {
    const group = raw.pane_group == null ? null : String(raw.pane_group)
    const colors = raw.colors
    const readoutOnly = raw.readout_only === true
    const output = {
        key: `${plugin.id}/${raw.key}`,
        pluginId: plugin.id,
        id: raw.key,
        order,
        kind: raw.type,
        label: text(raw.label, raw.key),
        paneId: raw.pane === "price" ? "price" : `${plugin.id}:${group ?? raw.key}`,
        paneGroup: group,
        style: {
            color: colorAt(colors, 0, DEFAULT_COLORS[order % DEFAULT_COLORS.length]),
            upColor: colorAt(colors, 0, "#27ae60"),
            downColor: colorAt(colors, 1, "#eb5757"),
            lineWidth: widthAt(raw.widths, 0, undefined),
        },
        readoutOnly,
        layers: null,
        tailMarker: false,
        // Full re-render cadence for outputs that cannot be patched per tick.
        throttled: raw.type === "segmented_lines",
    }
    if (raw.type === "segmented_lines") {
        output.tailMarker = raw.tail_marker === true
        output.layers = Array.from({ length: raw.layers }, (_, index) => ({
            index, color: colorAt(colors, index, DEFAULT_COLORS[index % DEFAULT_COLORS.length]), lineWidth: widthAt(raw.widths, index, 1),
        }))
    }
    return output
}

export function normalizeDescriptor(raw) {
    const plugin = {
        id: raw.id,
        version: text(raw.version, "", 40),
        label: text(raw.name, raw.id, 60),
        description: text(raw.description, "", 240),
        defaultEnabled: true,
        outputs: [],
    }
    plugin.outputs = raw.outputs.map((output, index) => normalizeOutput(plugin, output, index))
    return plugin
}

const isBarKind = output => !output.readoutOnly && (output.kind === "histogram" || output.kind === "dual_range")

// `entries` are parsed discovery items { id, status, descriptor, error }.
// Returns { plugins, pending, rejected, panes, signature }:
//  * plugins: status "ready" with a valid descriptor (whose id must match);
//  * pending: configured but not drawable yet/anymore (starting, failed,
//    disabled, unavailable) -> labelled controls, refreshed by the app;
//  * rejected: ready but invalid descriptors (never half-registered).
export function buildRegistry(entries) {
    const plugins = []
    const pending = []
    const rejected = []
    const seen = new Set()
    for (const entry of Array.isArray(entries) ? entries : []) {
        if (!entry) continue
        if (seen.has(entry.id)) { rejected.push({ id: entry.id, message: `duplicate indicator id ${entry.id}` }); continue }
        seen.add(entry.id)
        if (entry.status !== "ready" || !entry.descriptor) {
            pending.push({ id: entry.id, status: entry.status === "ready" ? "unavailable" : entry.status, error: entry.error ?? null })
            continue
        }
        try {
            const plugin = normalizeDescriptor(entry.descriptor)
            if (plugin.id !== entry.id) throw new Error(`descriptor id ${plugin.id} does not match ${entry.id}`)
            plugins.push(plugin)
        } catch (error) {
            rejected.push({ id: entry.id, message: String(error?.message ?? error) })
        }
    }
    const panes = [{ id: "price", label: "Price", outputs: [], groups: new Map() }]
    const paneById = new Map([["price", panes[0]]])
    for (const plugin of plugins) {
        const separate = new Set(plugin.outputs.filter(o => o.paneId !== "price").map(o => o.paneId))
        for (const output of plugin.outputs) {
            let pane = paneById.get(output.paneId)
            if (!pane) {
                const label = separate.size > 1 && output.paneGroup ? `${plugin.label} · ${output.paneGroup}` : plugin.label
                pane = { id: output.paneId, label, outputs: [], groups: new Map() }
                panes.push(pane); paneById.set(pane.id, pane)
            }
            pane.outputs.push(output)
        }
    }
    for (const pane of panes) {
        if (pane.id === "price") continue
        const alternatives = pane.outputs.filter(isBarKind)
        if (alternatives.length > 1) {
            const key = pane.id
            pane.groups.set(key, { key, label: pane.label, outputs: alternatives })
            for (const output of alternatives) output.groupKey = key
        }
    }
    panes.forEach((pane, index) => { pane.index = index })
    const signature = JSON.stringify(plugins.map(p => [p.id, p.version, p.outputs.map(o => [o.id, o.kind, o.paneId, o.readoutOnly, o.layers, o.style])]))
    return { plugins, pending, rejected, panes, signature }
}

// The active output of an exclusive group: persisted choice, else first declared.
export function activeGroupOutput(group, choice) {
    return group.outputs.find(o => o.key === choice) ?? group.outputs[0]
}

function formatCompact(value) {
    if (!Number.isFinite(value)) return "unavailable"
    const absolute = Math.abs(value)
    if (absolute >= 1e9) return `${(value / 1e9).toFixed(2)}B`
    if (absolute >= 1e6) return `${(value / 1e6).toFixed(2)}M`
    if (absolute >= 1e3) return `${(value / 1e3).toFixed(2)}K`
    return value.toLocaleString(undefined, { maximumFractionDigits: 2 })
}

// Readout of one output value for the hovered/latest bar, by kind only.
// segmented_lines has no compact readout (native tails carry last values).
export function readoutText(output, value) {
    const num = v => formatCompact(typeof v === "number" ? v : NaN)
    if (output.kind === "segmented_lines") return ""
    if (output.kind === "dual_range") {
        const ok = value && typeof value === "object"
        return `${output.label} ${num(ok ? value.upper : NaN)} / ${num(ok ? value.lower : NaN)}`
    }
    return `${output.label} ${num(value)}`
}

// ------------------------------------------------------------ Renderers
// Generic renderer adapters, one per declarative output kind. A renderer owns
// a bounded set of chart series for exactly one output:
//   line            -> 1 native LineSeries
//   histogram       -> 1 native HistogramSeries (+ optional zero price line)
//   dual_range      -> separate pane: 1 dual-range histogram custom series
//                      around base 0 (+ zero line); price pane: 2 native
//                      lines (upper/lower band edges)
//   segmented_lines -> per layer at most 2 series: one custom series for all
//                      earlier segments + one native tail series that alone
//                      carries the last-value label / crosshair marker when
//                      the descriptor sets tail_marker.
// Missing values (absent key, null, non-finite) are whitespace, never zero.
// Gaps are never bridged. No per-gap series fan-out.

function compactPriceFormat() {
    return {
        type: "custom",
        formatter: p => {
            const a = Math.abs(p)
            if (a >= 1e9) return (p / 1e9).toFixed(2) + "B"
            if (a >= 1e6) return (p / 1e6).toFixed(2) + "M"
            if (a >= 1e3) return (p / 1e3).toFixed(2) + "K"
            return p.toFixed(0)
        },
    }
}

function scalarOf(value) {
    return typeof value === "number" && Number.isFinite(value) ? value : null
}

export function linePoint(time, value) {
    const v = scalarOf(value)
    return v === null ? { time } : { time, value: v }
}

export function histogramPoint(time, value, style) {
    const v = scalarOf(value)
    return v === null ? { time } : { time, value: v, color: v >= 0 ? style.upColor : style.downColor }
}

// dual_range value { upper, lower } (signed, relative to base 0) maps to the
// official dual-range slots [upOuter, upInner, downOuter, downInner]. A true
// zero stays zero (never negative zero, never a gap).
export function dualRangePoint(time, value) {
    const upper = scalarOf(value?.upper), lower = scalarOf(value?.lower)
    if (upper === null || lower === null) return { time }
    return { time, values: [upper === 0 ? 0 : upper, 0, lower === 0 ? 0 : lower, 0] }
}

function bandPoints(time, value) {
    const upper = scalarOf(value?.upper), lower = scalarOf(value?.lower)
    return [upper === null ? { time } : { time, value: upper }, lower === null ? { time } : { time, value: lower }]
}

// One occupancy state per renderer: `last` = newest written time, null = empty.
// LWC's plain series.update() only appends or replaces the LAST point; an
// existing older point (penultimate re-close, tail catch-up) must be written
// with historicalUpdate=true or LWC throws "Cannot update oldest data".
// Measured (N-simplify-review.md): full-window setData p95 15-25 ms at
// 1600-3200 bars vs <0.5 ms for this path, so older-point patches stay here.
function tailUpdate(series, point, last) {
    series.update(point, last !== null && point.time < last)
    return last === null || point.time > last ? point.time : last
}

function zeroLine(series) {
    if (typeof series.createPriceLine !== "function") return
    series.createPriceLine({ price: 0, color: "rgba(107, 118, 138, 0.7)", lineWidth: 1, lineStyle: 0, axisLabelVisible: false, title: "" })
}

function seriesRenderer(chart, deps, output, paneIndex) {
    const { LightweightCharts: LWC, createDualRangeHistogramSeries, outputValue } = deps
    const style = output.style
    // Lines keep LWC's native last-value label + price line (as the original
    // CVD pane did); bar-type outputs suppress both.
    const common = { priceLineVisible: output.kind === "line", lastValueVisible: output.kind === "line", visible: false }
    let series
    let toPoint
    if (output.kind === "line") {
        series = chart.addSeries(LWC.LineSeries, { ...common, color: style.color, ...(style.lineWidth ? { lineWidth: style.lineWidth } : {}),
            priceFormat: style.format === "volume" ? { type: "volume" } : compactPriceFormat() }, paneIndex)
        toPoint = (bar, aggregate) => linePoint(bar.time, outputValue(bar, output.key, aggregate))
    } else if (output.kind === "histogram") {
        series = chart.addSeries(LWC.HistogramSeries, { ...common, priceFormat: { type: "volume" } }, paneIndex)
        toPoint = (bar, aggregate) => histogramPoint(bar.time, outputValue(bar, output.key, aggregate), style)
    } else {
        series = createDualRangeHistogramSeries(chart, { ...common, scaleMode: "price", baseValue: 0,
            colors: { upOuter: style.upColor, upInner: style.upColor, downOuter: style.downColor, downInner: style.downColor },
            borderColor: null, highlightHovered: false, priceFormat: { type: "volume" } }, paneIndex)
        toPoint = (bar, aggregate) => dualRangePoint(bar.time, outputValue(bar, output.key, aggregate))
    }
    if (output.kind !== "line") zeroLine(series)
    let shown = false
    let last = null
    return {
        output,
        dirty: true,
        render(bars, aggregate) { series.setData(bars.map(bar => toPoint(bar, aggregate))); last = bars.at(-1)?.time ?? null; this.dirty = false },
        update(bar, aggregate) { last = tailUpdate(series, toPoint(bar, aggregate), last) },
        clear() { if (last !== null) series.setData([]); last = null; this.dirty = true },
        setVisible(visible) { if (shown !== visible) { shown = visible; series.applyOptions({ visible }) } },
        priceScales: () => [series.priceScale()],
        seriesCount: () => 1,
        seriesHandle: () => series,
        destroy() { chart.removeSeries(series) },
    }
}

function bandRenderer(chart, deps, output, paneIndex) {
    const { LightweightCharts: LWC, outputValue } = deps
    const options = { color: output.style.upColor, lineWidth: 1, priceLineVisible: false, lastValueVisible: false, crosshairMarkerVisible: false, visible: false }
    const edges = [chart.addSeries(LWC.LineSeries, options, paneIndex), chart.addSeries(LWC.LineSeries, { ...options, color: output.style.downColor }, paneIndex)]
    let shown = false
    let last = null
    const split = (bars, aggregate) => {
        const columns = [[], []]
        for (const bar of bars) bandPoints(bar.time, outputValue(bar, output.key, aggregate)).forEach((point, i) => columns[i].push(point))
        return columns
    }
    return {
        output,
        dirty: true,
        render(bars, aggregate) { split(bars, aggregate).forEach((data, i) => edges[i].setData(data)); last = bars.at(-1)?.time ?? null; this.dirty = false },
        update(bar, aggregate) { const prev = last; bandPoints(bar.time, outputValue(bar, output.key, aggregate)).forEach((point, i) => { last = tailUpdate(edges[i], point, prev) }) },
        clear() { if (last !== null) edges.forEach(edge => edge.setData([])); last = null; this.dirty = true },
        setVisible(visible) { if (shown !== visible) { shown = visible; edges.forEach(edge => edge.applyOptions({ visible })) } },
        priceScales: () => [],
        seriesCount: () => 2,
        seriesHandle: () => edges[0],
        destroy() { edges.forEach(edge => chart.removeSeries(edge)) },
    }
}

function segmentedRenderer(chart, deps, output, paneIndex) {
    const { LightweightCharts: LWC, outputValue } = deps
    let slots = []

    function syncEntry(slot, kind, data, line) {
        let entry = slot[kind]
        if (!data || !data.length) {
            if (entry) chart.removeSeries(entry.series)
            slot[kind] = null
            return
        }
        if (!entry) {
            const marker = kind === "tail" && output.tailMarker
            const options = { color: line.color, lineWidth: line.lineWidth, priceLineVisible: false, lastValueVisible: marker }
            const series = kind === "tail"
                ? chart.addSeries(LWC.LineSeries, { ...options, crosshairMarkerVisible: marker }, paneIndex)
                : chart.addCustomSeries(createSegmentHistoryView(LWC.customSeriesDefaultOptions), options, paneIndex)
            entry = slot[kind] = { series, data: [], kind, marker, color: line.color, lineWidth: line.lineWidth }
        }
        if (!sameSegmentData(entry.data, data)) {
            entry.series.setData(data)
            entry.data = data
        }
    }

    function clearAll() {
        for (const slot of slots) for (const kind of ["history", "tail"]) if (slot?.[kind]) chart.removeSeries(slot[kind].series)
        slots = []
    }

    return {
        output,
        dirty: true,
        render(bars, aggregate) {
            for (const line of output.layers) {
                const values = bars.map(bar => {
                    const value = outputValue(bar, output.key, aggregate)
                    const v = Array.isArray(value) ? value[line.index] : null
                    return typeof v === "number" && Number.isFinite(v) ? v : null
                })
                const { history, tail } = segmentSlotData(segmentData(bars, values))
                const slot = slots[line.index] ??= { history: null, tail: null }
                syncEntry(slot, "history", history, line)
                syncEntry(slot, "tail", tail, line)
            }
            this.dirty = false
        },
        update() { this.dirty = true },
        clear() { clearAll(); this.dirty = true },
        setVisible(visible) { if (!visible) clearAll() },
        priceScales: () => [],
        seriesCount: () => slots.reduce((n, slot) => n + (slot?.history ? 1 : 0) + (slot?.tail ? 1 : 0), 0),
        slots: () => slots,
        entries: () => slots.flatMap(slot => slot ? [slot.history, slot.tail].filter(Boolean) : []),
        destroy() { clearAll() },
    }
}

export function createOutputRenderer(chart, deps, output, paneIndex) {
    if (output.kind === "segmented_lines") return segmentedRenderer(chart, deps, output, paneIndex)
    if (output.kind === "dual_range" && output.paneId === "price") return bandRenderer(chart, deps, output, paneIndex)
    return seriesRenderer(chart, deps, output, paneIndex)
}

// ------------------------------------------------------------ Segmented-line geometry
// Generic geometry for `segmented-lines` plugin outputs. No indicator
// formula lives here: values come from the plugin output as delivered by the
// host. See docs/frontend-plugins.md.
//
// Per declared line the drawable segments (>= 2 consecutive finite points) are
// split into:
//   - tail: the final segment, drawn by a native LineSeries that alone carries
//     the last-value label and crosshair marker (unchanged native semantics);
//   - history: every earlier segment, flattened into ONE custom series whose
//     items carry an explicit `start` flag at the first point of each segment.
// Gaps are never bridged and no values are invented: a history item exists
// only for a real finite point, and a new path starts at every `start`.

export function segmentData(bars, values, include = () => true) {
    const segments = []
    let segment = []
    for (let i = 0; i < bars.length; i++) {
        const value = values[i]
        if (Number.isFinite(value) && include(i)) {
            segment.push({ time: bars[i].time, value })
        } else if (segment.length) {
            if (segment.length > 1) segments.push(segment)
            segment = []
        }
    }
    if (segment.length > 1) segments.push(segment)
    return segments
}

export function segmentSlotData(segments) {
    const history = []
    for (let s = 0; s < segments.length - 1; s++) {
        segments[s].forEach((point, index) => history.push({ time: point.time, value: point.value, start: index === 0 }))
    }
    return { history, tail: segments.length ? segments[segments.length - 1] : null }
}

// Canonicalize history items back into native-equivalent per-run segments.
export function segmentHistorySegments(data) {
    const segments = []
    for (const item of data) {
        if (item.start || !segments.length) segments.push([])
        segments[segments.length - 1].push({ time: item.time, value: item.value })
    }
    return segments
}

export function sameSegmentData(a, b) {
    if (!a || !b || a.length !== b.length) return false
    for (let i = 0; i < a.length; i++) {
        if (a[i].time !== b[i].time || a[i].value !== b[i].value || Boolean(a[i].start) !== Boolean(b[i].start)) return false
    }
    return true
}

// Mirrors LWC 5.1.0 native line drawing (simple line type, solid style):
// lineCap butt, lineJoin round, width = lineWidth * verticalPixelRatio,
// coordinates scaled to bitmap space. Custom series only get x for the exact
// visible range, so one neighbour on each side is positioned from the visible
// reference bar (x is linear in the time-point index) to clip at the edges.
// All segments are batched into one stroke (segments have >= 2 points).
export function drawSegmentHistory(scope, frame, options, priceToCoordinate) {
    const { bars, visibleRange, barSpacing } = frame
    if (!bars.length || !visibleRange || visibleRange.from >= visibleRange.to) return
    const { context: ctx, horizontalPixelRatio: h, verticalPixelRatio: v } = scope
    const ref = bars[visibleRange.from]
    const lo = Math.max(0, visibleRange.from - 1)
    const hi = Math.min(bars.length, visibleRange.to + 1)
    ctx.lineCap = 'butt'
    ctx.lineJoin = 'round'
    ctx.lineWidth = options.lineWidth * v
    ctx.strokeStyle = options.color
    ctx.setLineDash([])
    ctx.beginPath()
    let open = false
    for (let j = lo; j < hi; j++) {
        const bar = bars[j]
        const item = bar.originalData
        const y = Number.isFinite(item?.value) ? priceToCoordinate(item.value) : null
        if (y === null || !Number.isFinite(y)) { open = false; continue }
        const x = j >= visibleRange.from && j < visibleRange.to ? bar.x : ref.x + (bar.time - ref.time) * barSpacing
        if (!open || item.start) ctx.moveTo(x * h, y * v)
        else ctx.lineTo(x * h, y * v)
        open = true
    }
    ctx.stroke()
}

export function createSegmentHistoryView(defaultOptions = {}) {
    let frame = null
    let seriesOptions = null
    const renderer = {
        draw(target, priceToCoordinate) {
            if (!frame || !seriesOptions) return
            target.useBitmapCoordinateSpace(scope => drawSegmentHistory(scope, frame, seriesOptions, priceToCoordinate))
        },
    }
    return {
        renderer: () => renderer,
        update(data, options) { frame = data; seriesOptions = options },
        priceValueBuilder: item => [item.value],
        isWhitespace: item => !Number.isFinite(item?.value),
        defaultOptions: () => ({ ...defaultOptions, color: '#eb5757', lineWidth: 1 }),
        destroy() { frame = null; seriesOptions = null },
    }
}

// ------------------------------------------------------------ Candles
export const PRICE_UP_COLOR = "#27ae60"
export const PRICE_DOWN_COLOR = "#eb5757"
export const PRICE_FLAT_COLOR = "#8b94a7"
export const CURRENT_PRICE_COLOR = "#f2994a"

// A non-positive or non-finite price field falls back (never drawn as 0).
const positivePrice = (value, fallback) => { const price = Number(value); return Number.isFinite(price) && price > 0 ? price : fallback }

export function barClose(bar, aggregate) {
    const close = Number(bar.price_close)
    return aggregate ? positivePrice(bar.apc, close) : close
}

// One candle for the source or aggregate price fields. Colour is direction
// versus the previous candle's close (LWC colours by close-vs-open, which
// reads as random on tiny intraday candles); the first bar uses open/close.
export function candlePoint(bar, previousBar, aggregate) {
    const close = barClose(bar, aggregate)
    const open = positivePrice(aggregate ? bar.apo : bar.po, close)
    const extreme = (raw, pick) => {
        const candidates = [positivePrice(raw, close), open, close].filter(price => Number.isFinite(price) && price > 0)
        return candidates.length ? pick(...candidates) : NaN
    }
    const high = extreme(aggregate ? bar.aph : bar.ph, Math.max)
    const low = extreme(aggregate ? bar.apl : bar.pl, Math.min)
    const previousClose = previousBar ? barClose(previousBar, aggregate) : NaN
    const color = Number.isFinite(previousClose)
        ? (close > previousClose ? PRICE_UP_COLOR : close < previousClose ? PRICE_DOWN_COLOR : PRICE_FLAT_COLOR)
        : Number.isFinite(open) && Number.isFinite(close) && close !== open ? (close > open ? PRICE_UP_COLOR : PRICE_DOWN_COLOR) : PRICE_FLAT_COLOR
    return {
        time: bar.time,
        open: Number.isFinite(open) ? open : close,
        high: Number.isFinite(high) ? high : close,
        low: Number.isFinite(low) ? low : close,
        close, color, borderColor: color, wickColor: color,
    }
}

// ------------------------------------------------------------ Time labels
// Display-only bar labels. Canonical Bar.time is the inclusive bucket END
// second ((id+1)*tf-1); charts label a bar by its interval START instead.
// Stored/series times are never changed: only LWC tick/crosshair text.
// Convention: UTC, matching LWC's default for UTCTimestamp values.
export const TF_SECONDS = Object.freeze({ "1m": 60, "5m": 300, "15m": 900, "30m": 1800, "1h": 3600 })

export function barOpenTime(time, tf) {
    const step = TF_SECONDS[tf]
    if (!step || typeof time !== "number" || !Number.isFinite(time)) return time
    return Math.floor(time / step) * step
}

const pad = n => String(n).padStart(2, "0")
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]

function hm(sec, seconds) {
    const d = new Date(sec * 1000)
    const base = `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`
    return seconds ? `${base}:${pad(d.getUTCSeconds())}` : base
}

// getTf() is read on every call, so timeframe switches never use a stale tf.
// Date-level tick types (Year/Month/DayOfMonth) return null -> LWC default.
export function createTimeLabelFormatters(getTf) {
    return {
        tickMarkFormatter(time, tickMarkType) {
            if (typeof time !== "number") return null
            if (tickMarkType === 3) return hm(barOpenTime(time, getTf()), false)
            if (tickMarkType === 4) return hm(barOpenTime(time, getTf()), true)
            return null
        },
        timeFormatter(time) {
            if (typeof time !== "number") return String(time)
            const open = barOpenTime(time, getTf())
            const d = new Date(open * 1000)
            return `${pad(d.getUTCDate())} ${MONTHS[d.getUTCMonth()]} '${pad(d.getUTCFullYear() % 100)}   ${hm(open, false)}`
        },
    }
}

// ------------------------------------------------------------ Source-quality readout
// Presentation-only interpretation of the backend's source-observation (quality) contract.
const STALE_MS = 60_000;
const OBSERVED_INPUTS = ['po','ph','pl','price_close','cvd_usdt','delta_usdt',
    'apo','aph','apl','apc','ac','ad','bvb','abvb','svb','asvb',
    'buy_volume_usdt','agb','sell_volume_usdt','ags','trades'];
export function mergeHistoryQuality(existing, incoming) {
    if (!existing || !incoming || Object.hasOwn(incoming, 'quality') ||
        !Number.isSafeInteger(existing.time) || existing.time <= 0 || existing.time !== incoming.time) return incoming;
    const q = existing.quality;
    if (!q || typeof q !== 'object' || Array.isArray(q) ||
        !Number.isFinite(q.observed_at_ms) || q.observed_at_ms <= 0 || q.observed_at_ms > 8.64e15 ||
        q.coverage !== 'unverified' || !Array.isArray(q.sources) || !q.sources.length) return incoming;
    if (!OBSERVED_INPUTS.every(key => {
        const a = existing[key], b = incoming[key];
        return typeof a === 'number' && typeof b === 'number' &&
            Number.isFinite(a) && Number.isFinite(b) &&
            Math.abs(a) <= Number.MAX_SAFE_INTEGER && Math.abs(b) <= Number.MAX_SAFE_INTEGER &&
            (key !== 'trades' || (Number.isSafeInteger(a) && Number.isSafeInteger(b) && a >= 0 && b >= 0)) && a === b;
    })) return incoming;
    return {...incoming, quality: {...q, sources: q.sources.map(source => ({...source})), retained: true}};
}
const retainedNote = q => q.retained === true ? 'Original real-time observation retained across identical history inputs, not a new historical completeness audit; coverage remains unverified. ' : '';
const stamp = ms => Number.isFinite(ms) && ms > 0 && ms <= 8.64e15 ? new Date(ms).toISOString() : 'unknown';
export function qualityDisplay(bar, aggregate, now = Date.now(), latest = true, scope = null) {
    // Only a matching source row may describe a selected venue. Aggregate
    // participation counts never attest selected-feed completeness.
    const q = bar?.quality;
    if (!q || !Number.isFinite(q.observed_at_ms) || q.observed_at_ms <= 0 || q.observed_at_ms > 8.64e15 || !Array.isArray(q.sources) || !q.sources.length || q.coverage !== 'unverified') {
        return { label: 'Quality unknown', tooltip: 'No retained source evidence for this bar; coverage unknown.' };
    }
    const entries = q.sources.filter(s => s && typeof s === 'object');
    if (!aggregate) {
        const source = entries.find(s => s.exchange === scope?.exchange && s.market === scope?.market);
        if (!source) return { label: 'Quality unknown', tooltip: 'No matching source observation for this selection; coverage unknown.' };
        const stale = (latest || q.retained === true) && (!Number.isFinite(now) || now - q.observed_at_ms > STALE_MS ||
            !Number.isFinite(q.status_observed_at_ms) || now - q.status_observed_at_ms > STALE_MS);
        const missing = source.contribution === 'missing' || source.contribution === 'carried';
        const recovering = ['starting', 'recovering', 'needs_backfill', 'unavailable'].includes(source.recovery);
        const label = missing ? 'Source missing' : recovering ? 'Source recovering' : source.recovery !== 'live' ? 'Source unknown' : 'Source ·';
        return { label: stale ? (label === 'Source ·' ? 'Source stale' : `${label} · stale`) : label,
            tooltip: `${retainedNote(q)}${stale ? (q.retained === true ? 'Retained observation stale. ' : 'Latest snapshot stale. ') : ''}${source.exchange} ${source.market}: ${source.contribution || 'unknown'}, recovery ${source.recovery || 'unknown'}; bar ${source.bar_time ?? 'unknown'}, last trade ${stamp(source.last_trade_ms)}. Inputs observed ${stamp(q.observed_at_ms)}; status observed ${stamp(q.status_observed_at_ms)}. Aggregate delivery observation only, not proof of selected-feed completeness; ${q.bar_state === 'elapsed' ? 'elapsed is not final' : 'bar forming'}.` };
    }
    const sources = entries.map(s => `${s.exchange || '?'} ${s.market || '?'}: ${s.contribution || 'unknown'}, recovery ${s.recovery || 'unknown'} (bar ${s.bar_time ?? 'unknown'}, last trade ${stamp(s.last_trade_ms)})`);
    const stale = (latest || q.retained === true) && (!Number.isFinite(now) || now - q.observed_at_ms > STALE_MS ||
        !Number.isFinite(q.status_observed_at_ms) || now - q.status_observed_at_ms > STALE_MS);
    const missing = entries.some(s => s.contribution === 'missing' || s.contribution === 'carried');
    const recovery = entries.some(s => ['starting', 'recovering', 'needs_backfill', 'unavailable'].includes(s.recovery));
    const unknownRecovery = entries.length !== q.sources.length || entries.some(s => !['starting','live','recovering','needs_backfill','unavailable'].includes(s.recovery));
    const partial = missing || recovery || (Number.isFinite(q.expected_sources) && Number.isFinite(q.present_sources) && q.present_sources < q.expected_sources);
    const label = stale ? (missing ? 'Quality missing · stale' : recovery ? 'Quality recovering · stale' : 'Quality stale')
        : missing ? 'Quality missing' : recovery ? 'Quality recovering' : partial ? 'Quality partial' : unknownRecovery ? 'Quality unknown' : 'Quality ·';
    return { label, tooltip: `${retainedNote(q)}${stale ? (q.retained === true ? 'Retained observation stale. ' : 'Latest snapshot stale. ') : ''}${q.bar_state === 'elapsed' ? 'Elapsed, not final. ' : 'Forming. '}Coverage ${q.coverage || 'unverified'}; ${q.present_sources ?? '?'} / ${q.expected_sources ?? '?'} same-bucket participants, not completeness. Inputs observed ${stamp(q.observed_at_ms)}; recovery status observed ${stamp(q.status_observed_at_ms)}. ${sources.join('; ')}` };
}
