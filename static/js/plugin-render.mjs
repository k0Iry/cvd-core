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

export function bandPoints(time, value) {
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
    const { LightweightCharts: LWC, outputValue, segmentData, segmentSlotData, sameSegmentData, createSegmentHistoryView } = deps
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
