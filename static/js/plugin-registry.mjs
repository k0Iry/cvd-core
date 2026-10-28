// Generic, descriptor-driven indicator registry. Descriptors follow the frozen
// SDK `Descriptor` (cvd-core crates/indicator-sdk/src/model.rs, docs/
// plugin-api-v1.md §4) verbatim; the browser re-validates them defensively.
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

export const OUTPUT_KINDS = new Set(["line", "histogram", "dual_range", "segmented_lines"])
const ID_RE = /^[a-z0-9_.-]{1,64}$/
const KEY_RE = /^[A-Za-z0-9_.-]{1,64}$/
const COLOR_RE = /^(#[0-9a-fA-F]{3,8}|rgba?\([0-9., ]+\))$/
const MAX_OUTPUTS = 64
const MAX_LAYERS = 32
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
    if (!raw || typeof raw !== "object") throw new Error("output is not an object")
    if (!KEY_RE.test(raw.key ?? "")) throw new Error(`invalid output key ${JSON.stringify(raw.key)}`)
    if (!OUTPUT_KINDS.has(raw.type)) throw new Error(`unsupported output type ${JSON.stringify(raw.type)}`)
    if (raw.pane !== "price" && raw.pane !== "separate") throw new Error(`invalid pane ${JSON.stringify(raw.pane)}`)
    const group = raw.pane_group == null ? null : text(raw.pane_group, null, 64)
    if (raw.pane_group != null && (!group || !KEY_RE.test(group))) throw new Error("invalid pane_group")
    const colors = raw.colors
    const readoutOnly = raw.readout_only === true
    if (readoutOnly && raw.type !== "line") throw new Error("readout_only requires a line output")
    if (Array.isArray(raw.widths) && raw.widths.length > MAX_LAYERS) throw new Error("too many widths")
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
        if (!Number.isInteger(raw.layers) || raw.layers < 1 || raw.layers > MAX_LAYERS) throw new Error(`segmented_lines layers must be 1..=${MAX_LAYERS}`)
        output.tailMarker = raw.tail_marker === true
        output.layers = Array.from({ length: raw.layers }, (_, index) => ({
            index, color: colorAt(colors, index, DEFAULT_COLORS[index % DEFAULT_COLORS.length]), lineWidth: widthAt(raw.widths, index, 1),
        }))
    }
    return output
}

export function normalizeDescriptor(raw) {
    if (!raw || typeof raw !== "object") throw new Error("descriptor is not an object")
    if (!ID_RE.test(raw.id ?? "")) throw new Error(`invalid indicator id ${JSON.stringify(raw.id)}`)
    const version = text(raw.version, null, 40)
    if (!version) throw new Error(`${raw.id}: missing version`)
    if (!Array.isArray(raw.outputs) || !raw.outputs.length || raw.outputs.length > MAX_OUTPUTS) throw new Error(`${raw.id}: requires 1..=${MAX_OUTPUTS} outputs`)
    const plugin = {
        id: raw.id,
        version,
        label: text(raw.name, raw.id, 60),
        description: text(raw.description, "", 240),
        params: Array.isArray(raw.params) ? raw.params.slice(0, 32) : [],
        defaultEnabled: true,
        outputs: [],
    }
    plugin.outputs = raw.outputs.map((output, index) => normalizeOutput(plugin, output, index))
    if (new Set(plugin.outputs.map(o => o.id)).size !== plugin.outputs.length) throw new Error(`${raw.id}: duplicate output key`)
    return plugin
}

// Host-reported availability of a configured indicator (discovery status or
// per-payload status). Anything but "ok" means: clear and label its outputs.

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

export function formatCompact(value) {
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
