// Local (browser-only) indicator plugin packages: pure helpers + IndexedDB
// persistence. Every visitor imports their own .wasm; nothing is uploaded and
// no plugin id/author is special. The module bytes are untrusted and are only
// ever handed to the trusted Worker bridge (static/wasm-host/host-bridge.js),
// which validates them in Rust BEFORE compile (CONTRACT.md §2/§6).
//
// Record (one per descriptor id):
//   { id, version, name, sha256, size, fileName, bytes: ArrayBuffer,
//     descriptor, params: {key: value}, enabled: bool, importedAt }

export const DB_NAME = "cvd-local-plugins"
export const DB_STORE = "plugins"
export const MAX_MODULE_BYTES = 16 << 20
export const MAX_LOCAL_PLUGINS = 32

// Canonical server bar (/history, /ws `bar`) -> SDK InputBar. Pure field
// projection mirroring cvd-server indicators.rs `input_bar` (wire names are
// the serde renames of models.rs `Bar`). Authoritative cumulative CVD comes
// from the server (`cvd_usdt`, `ac`); nothing is recomputed or defaulted:
// a missing/non-finite field fails closed instead of fabricating zeros.
export const INPUT_FIELDS = ["po", "ph", "pl", "price_close", "apo", "aph", "apl", "apc", "cvd_usdt", "ac", "delta_usdt", "ad",
    "bvb", "abvb", "svb", "asvb", "buy_volume_usdt", "agb", "sell_volume_usdt", "ags"]

export function inputBar(bar) {
    if (!bar || !Number.isSafeInteger(bar.time) || bar.time <= 0) throw codeError("invalid_input", "bar without canonical time")
    const out = { time: bar.time }
    for (const field of INPUT_FIELDS) {
        const value = bar[field]
        if (typeof value !== "number" || !Number.isFinite(value)) throw codeError("invalid_input", `bar ${bar.time}: ${field} missing`)
        out[field] = value
    }
    const trades = bar.trades
    if (!Number.isSafeInteger(trades) || trades < 0) throw codeError("invalid_input", `bar ${bar.time}: trades missing`)
    out.trades = trades
    return out
}

export function codeError(code, message) { return Object.assign(new Error(message || code), { code }) }

export function contextBars(descriptor, tf) {
    const ctx = descriptor?.context ?? {}
    const per = ctx.per_tf && typeof ctx.per_tf === "object" ? ctx.per_tf[tf] : undefined
    const n = Number.isSafeInteger(per) ? per : ctx.bars
    return Number.isSafeInteger(n) && n >= 0 ? n : 0
}

// History request over the shared canonical cache. Exact warm-up: the first
// `context` bars are warm-up only; outputs exist only for bars that have the
// full declared context before them. `complete` tells the caller whether more
// history must be fetched (prepend) to cover the oldest cached bar.
export function historyBody(bars, scope, revision, descriptor) {
    const ctx = contextBars(descriptor, scope.tf)
    const input = bars.map(inputBar)
    const outputFrom = Math.min(ctx, input.length)
    return { body: JSON.stringify({ scope, revision, output_from: outputFrom, bars: input }), outputFrom, context: ctx }
}

export function defaultParams(descriptor) {
    const params = {}
    for (const spec of Array.isArray(descriptor?.params) ? descriptor.params : []) params[spec.key] = spec.default
    return params
}

// Keep only declared keys with values of the declared kind/range; anything
// else falls back to the default (persisted params can never smuggle keys).
export function sanitizeParams(descriptor, raw) {
    const out = defaultParams(descriptor)
    const src = raw && typeof raw === "object" && !Array.isArray(raw) ? raw : {}
    for (const spec of Array.isArray(descriptor?.params) ? descriptor.params : []) {
        if (!Object.hasOwn(src, spec.key)) continue
        const v = src[spec.key]
        if (spec.type === "int" && Number.isSafeInteger(v) && v >= spec.min && v <= spec.max) out[spec.key] = v
        else if (spec.type === "float" && typeof v === "number" && Number.isFinite(v) && v >= spec.min && v <= spec.max) out[spec.key] = v
        else if (spec.type === "bool" && typeof v === "boolean") out[spec.key] = v
        else if (spec.type === "choice" && Array.isArray(spec.options) && spec.options.includes(v)) out[spec.key] = v
    }
    return out
}

// Coerce a form value into the declared param kind or throw invalid_params.
export function parseParamInput(spec, text) {
    if (spec.type === "bool") return text === true || text === "true"
    if (spec.type === "choice") { if (!spec.options.includes(text)) throw codeError("invalid_params", `${spec.key}: unknown option`); return text }
    const v = Number(text)
    if (!Number.isFinite(v) || (spec.type === "int" && !Number.isSafeInteger(v)) || v < spec.min || v > spec.max) throw codeError("invalid_params", `${spec.key} must be ${spec.min}..${spec.max}`)
    return v
}

// Pre-read file checks (name/size). Bytes are checked again by the bridge.
export function checkImportFile(file) {
    if (!file || typeof file.name !== "string") throw codeError("bad_module", "no file selected")
    if (!/\.wasm$/i.test(file.name)) throw codeError("bad_file_type", "only .wasm files can be imported")
    if (!(file.size > 0)) throw codeError("bad_magic", "empty file")
    if (file.size > MAX_MODULE_BYTES) throw codeError("module_too_large", "module exceeds 16 MiB")
}

// Decide what an import does relative to existing records (pure).
//   -> { action: "add" | "replace" | "same", previous? }
export function classifyImport(records, candidate) {
    const previous = records.find(r => r.id === candidate.id)
    if (!previous) {
        if (records.length >= MAX_LOCAL_PLUGINS) throw codeError("resource_limit", `at most ${MAX_LOCAL_PLUGINS} local plugins`)
        return { action: "add" }
    }
    if (previous.sha256 === candidate.sha256) return { action: "same", previous }
    return { action: "replace", previous }
}

// Content identity for duplicate/replace decisions (not a security control).
// crypto.subtle exists only in secure contexts (https/localhost); a plain
// http LAN deployment falls back to a 64-bit FNV-1a digest, and callers
// confirm "same" by full byte comparison.
export async function sha256Hex(bytes) {
    if (globalThis.crypto?.subtle) {
        const digest = await crypto.subtle.digest("SHA-256", bytes)
        return [...new Uint8Array(digest)].map(b => b.toString(16).padStart(2, "0")).join("")
    }
    return fnv64Hex(bytes)
}

// Plain-HTTP fallback: 64-bit FNV-1a on two 32-bit halves (no BigInt per
// byte), yielding to the event loop every `chunk` bytes so a 16 MiB module
// never blocks the UI thread for one long task.
export async function fnv64Hex(bytes, { chunk = 1 << 20, yieldFn = () => new Promise(r => setTimeout(r, 0)) } = {}) {
    const view = new Uint8Array(bytes)
    let hi = 0xcbf29ce4, lo = 0x84222325
    for (let start = 0; start < view.length; start += chunk) {
        const end = Math.min(view.length, start + chunk)
        for (let i = start; i < end; i++) {
            lo = (lo ^ view[i]) >>> 0
            // (hi,lo) * 0x100000001b3 mod 2^64 = (hi,lo)*0x1b3 + (lo<<40)
            const p = lo * 0x1b3 // < 2^41: exact in a double
            hi = (Math.imul(hi, 0x1b3) + Math.floor(p / 0x100000000) + (lo << 8)) >>> 0
            lo = p % 0x100000000
        }
        if (end < view.length) await yieldFn()
    }
    return `fnv64:${hi.toString(16).padStart(8, "0")}${lo.toString(16).padStart(8, "0")}:${view.length}`
}

// Restored IndexedDB record shape/identity check (fail closed: a corrupted or
// foreign record is skipped and reported, never half-loaded).
export function validRecord(r) {
    if (!r || typeof r !== "object") return false
    if (typeof r.id !== "string" || !r.id || r.id.length > 128) return false
    if (!(r.bytes instanceof ArrayBuffer) || r.bytes.byteLength === 0 || r.bytes.byteLength > MAX_MODULE_BYTES) return false
    if (typeof r.size === "number" && r.size !== r.bytes.byteLength) return false
    if (!r.descriptor || typeof r.descriptor !== "object" || r.descriptor.id !== r.id) return false
    if (typeof r.sha256 !== "string" || !r.sha256) return false
    if (typeof r.enabled !== "boolean") return false
    if (!Number.isFinite(r.importedAt)) return false
    return true
}

export function sameBytes(a, b) {
    if (!(a instanceof ArrayBuffer) || !(b instanceof ArrayBuffer) || a.byteLength !== b.byteLength) return false
    const x = new Uint8Array(a), y = new Uint8Array(b)
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return false
    return true
}

// Human-readable, fixed error texts (plugin-supplied messages are never shown
// verbatim; only the bridge/host public code).
const IMPORT_MESSAGES = {
    bad_file_type: "only .wasm files can be imported", bad_module: "not a WebAssembly module", bad_magic: "not a WebAssembly module", malformed_module: "malformed WebAssembly",
    module_too_large: "module exceeds 16 MiB", forbidden_import: "module imports host functions (not allowed)",
    forbidden_start: "module has a start function (not allowed)", bad_memory: "memory declaration not allowed",
    bad_table: "table declaration not allowed", resource_limit: "module exceeds resource limits",
    unsupported_feature: "unsupported WebAssembly feature", missing_export: "not a cvd-wasm v1 plugin (missing export)",
    bad_signature: "not a cvd-wasm v1 plugin (bad export signature)", unexpected_export: "module exports unexpected symbols",
    abi_mismatch: "incompatible plugin ABI version", bad_descriptor: "invalid plugin descriptor", deadline: "plugin timed out",
    trap: "plugin crashed", plugin_error: "plugin rejected its parameters", invalid_descriptor: "descriptor not displayable",
    storage: "browser storage unavailable", replace_params: "new version rejects your current parameters; previous version kept", invalid_params: "invalid parameter value",
}
export function importMessage(error) {
    const code = typeof error?.code === "string" ? error.code : "bad_module"
    return { code, text: IMPORT_MESSAGES[code] ?? `import failed (${code})` }
}

// ---------------------------------------------------------------- IndexedDB
export function openPluginDb(indexedDB = globalThis.indexedDB) {
    return new Promise((resolve, reject) => {
        if (!indexedDB) return reject(codeError("storage", "IndexedDB unavailable"))
        const req = indexedDB.open(DB_NAME, 1)
        req.onupgradeneeded = () => { if (!req.result.objectStoreNames.contains(DB_STORE)) req.result.createObjectStore(DB_STORE, { keyPath: "id" }) }
        req.onsuccess = () => resolve(createPluginDb(req.result))
        req.onerror = () => reject(codeError("storage", String(req.error?.message ?? "open failed")))
    })
}

function createPluginDb(db) {
    const tx = (mode, fn) => new Promise((resolve, reject) => {
        const t = db.transaction(DB_STORE, mode)
        const store = t.objectStore(DB_STORE)
        let result
        let r
        try { r = fn(store) } catch (error) {
            // Synchronous store failure (e.g. DataCloneError/quota): abort and
            // report a storage error, never a misleading module error.
            try { t.abort() } catch (_) {}
            return reject(codeError("storage", String(error?.message ?? "write failed")))
        }
        if (r) r.onsuccess = () => { result = r.result }
        t.oncomplete = () => resolve(result)
        t.onerror = t.onabort = () => reject(codeError("storage", String(t.error?.message ?? "transaction failed")))
    })
    return {
        all: () => tx("readonly", s => s.getAll()).then(list => (list || []).sort((a, b) => a.importedAt - b.importedAt || a.id.localeCompare(b.id))),
        put: record => tx("readwrite", s => s.put(record)),
        delete: id => tx("readwrite", s => s.delete(id)),
    }
}
