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
