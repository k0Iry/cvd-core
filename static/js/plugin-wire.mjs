// Host <-> browser wire adapter for indicator plugins.
//
// The ONLY frontend module that knows the host's indicator envelopes. The
// authoritative shapes are cvd-core's server types (crates/cvd-server/src/
// indicators.rs `IndicatorInfo`, `IndicatorHistory`, `ModeResult`;
// live_indicators.rs WebSocket frames) documented in
// docs/host-indicator-integration.md, carrying SDK v1 `Descriptor`/`Output`/
// `PluginError` documents verbatim. Output stays COLUMNAR; the browser keeps
// it in plugin-store.mjs keyed by canonical bar time and never copies it into
// base bars.
//
//   GET /indicators -> { api_version: 1, indicators: [ { id, status: starting|ready|failed|disabled|unavailable,
//                                                       version?, descriptor?, error? } ] }
//   /history        -> { ...base page, indicator_api_version: 1,
//                        indicators: { <id>: { version, modes: { source|aggregate: { status: ok|unavailable, output?, error? } } } } }
//   /ws typed frames (base bar frames have no `type`):
//     { type: "indicator", api_version, id, version, mode, generation, session, seq, output }
//     { type: "indicator_status", api_version, id, mode, generation, session?, status: ok|unavailable, error? }
//     { type: "indicator_reset", api_version, generation }

export const WIRE_API_VERSION = 1
export const DISCOVERY_PATH = "/indicators"
export const MODES = ["source", "aggregate"]
const MAX_INDICATORS = 64
const WIRE_ID_RE = /^[a-z0-9_.-]{1,64}$/
const CODE_RE = /^[a-z_]{1,40}$/

// `api_version` may be absent from servers predating its addition; any other
// value than 1 is rejected (unknown versions never parse as v1).
function checkVersion(value, what) {
    if (value !== undefined && value !== WIRE_API_VERSION) throw new Error(`${what}: unsupported api_version ${value}`)
}

// Public error -> fixed code only. Messages are never shown verbatim (they
// could carry arbitrary plugin text); the code is a stable public enum.
export function publicError(error) {
    const code = typeof error?.code === "string" && CODE_RE.test(error.code) ? error.code : "unavailable"
    return { code }
}

export function parseDiscovery(payload) {
    if (!payload || typeof payload !== "object") throw new Error("indicator discovery: not an object")
    checkVersion(payload.api_version, "indicator discovery")
    if (!Array.isArray(payload.indicators)) throw new Error("indicator discovery: indicators is not an array")
    return payload.indicators.slice(0, MAX_INDICATORS).flatMap(item => {
        if (!item || typeof item !== "object" || !WIRE_ID_RE.test(item.id ?? "")) return []
        const status = ["starting", "ready", "failed", "disabled", "unavailable"].includes(item.status) ? item.status : "unavailable"
        return [{ id: item.id, status, descriptor: item.descriptor ?? null, error: item.error ? publicError(item.error) : null }]
    })
}

function validOutput(output) {
    return output && typeof output === "object" && Array.isArray(output.times) && output.series && typeof output.series === "object"
}

// -> [{ id, version, mode, ok, output?, error? }] for a /history payload.
export function historyResults(payload) {
    const block = payload?.indicators
    if (!block || typeof block !== "object" || Array.isArray(block)) return []
    checkVersion(payload.indicator_api_version, "history indicators")
    const results = []
    for (const [id, entry] of Object.entries(block).slice(0, MAX_INDICATORS)) {
        if (!WIRE_ID_RE.test(id) || !entry?.modes || typeof entry.modes !== "object") continue
        for (const mode of MODES) {
            const result = entry.modes[mode]
            if (!result) continue
            if (result.status === "ok" && validOutput(result.output)) results.push({ id, version: entry.version ?? null, mode, ok: true, output: result.output })
            else results.push({ id, version: entry.version ?? null, mode, ok: false, error: publicError(result.error ?? { code: result.status === "ok" ? "invalid_output" : "unavailable" }) })
        }
    }
    return results
}

// Typed WebSocket indicator frame -> normalized event, or null for base bar
// frames. Malformed typed frames throw (caller resyncs).
export function parseIndicatorFrame(message) {
    if (!message || typeof message !== "object" || typeof message.type !== "string") return null
    if (!message.type.startsWith("indicator")) return null
    checkVersion(message.api_version, "indicator frame")
    const generation = Number.isSafeInteger(message.generation) ? message.generation : null
    if (generation === null) throw new Error("indicator frame: missing generation")
    if (message.type === "indicator_reset") return { type: "reset", generation }
    if (!WIRE_ID_RE.test(message.id ?? "") || !MODES.includes(message.mode)) throw new Error("indicator frame: bad id/mode")
    // `session` identifies the runner live session; servers that do not send
    // it yet are treated as one implicit session (0).
    const session = Number.isSafeInteger(message.session) ? message.session : 0
    const base = { id: message.id, mode: message.mode, generation, session }
    if (message.type === "indicator_status") {
        return message.status === "ok" ? { type: "status", ...base, ok: true } : { type: "status", ...base, ok: false, error: publicError(message.error) }
    }
    if (message.type === "indicator") {
        if (!Number.isSafeInteger(message.seq) || !validOutput(message.output)) throw new Error("indicator frame: bad seq/output")
        // as_of_rev: newest base-frame rev included. Required non-negative
        // safe integer; anything else fails closed (caller resyncs).
        if (!Number.isSafeInteger(message.as_of_rev) || message.as_of_rev < 0) throw new Error("indicator frame: bad as_of_rev")
        return { type: "output", ...base, version: message.version ?? null, seq: message.seq, asOfRev: message.as_of_rev, output: message.output }
    }
    throw new Error(`indicator frame: unknown type ${message.type}`)
}

const finiteOrNull = v => typeof v === "number" && Number.isFinite(v) ? v : null

// Value of one SDK SeriesValues column at index i, by kind.
export function seriesValueAt(series, index) {
    switch (series?.type) {
        case "line":
        case "histogram":
            return finiteOrNull(series.values?.[index])
        case "dual_range": {
            const upper = finiteOrNull(series.upper?.[index]), lower = finiteOrNull(series.lower?.[index])
            return upper === null || lower === null ? null : { upper, lower }
        }
        case "segmented_lines":
            return Array.isArray(series.layers) ? series.layers.map(layer => finiteOrNull(layer?.[index])) : null
        default:
            return undefined
    }
}
