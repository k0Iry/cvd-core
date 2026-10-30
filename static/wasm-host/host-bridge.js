// Trusted browser runtime for local cvd-wasm v1 indicator plugins: the
// Worker bridge (WasmPluginHost), the binding to the Rust coordinator, the
// per-plugin async driver, and the IndexedDB package store.
//
// Decisions are Rust's: untrusted-module policy and response fencing live in
// cvd_browser_host.wasm (`host_fence`); scheduling, readiness, coverage,
// retry/stall, admission and the accepted values (merge, precedence,
// invalidation, drop) live in cvd_browser_coordinator.wasm (sched.rs). This
// file moves bytes, owns Workers/timers/IndexedDB and forwards Rust events.
// Untrusted module parsing never runs on this thread. API: CONTRACT.md §6/§7.

// ------------------------------------------------------------ Worker bridge
export function codeError(code, message) { return Object.assign(new Error(message || code), { code }) }

const enc = new TextEncoder()
const dec = new TextDecoder()

// Host-enforced caps (mirror cvd_indicator_sdk::wasm constants; CONTRACT §3).
export const CAPS = Object.freeze({
    MAX_MODULE_BYTES: 16 << 20,
    MAX_PARAMS_BYTES: 64 << 10,
    MAX_INPUT_BYTES: 32 << 20,
    MAX_DEADLINE_MS: 60000,
    MAX_INSTANCE_ID: 2 ** 52 - 1
})
const LOAD_SCOPE = "__load"
const VALIDATE_SCOPE = "__validate"

function checkLimits(base, over) {
    const out = { ...base }
    for (const [k, v] of Object.entries(over)) {
        if (!["compile_ms", "instantiate_ms", "compute_ms"].includes(k)) throw codeError("bad_limits", `unknown limit ${k}`)
        if (typeof v !== "number" || !Number.isFinite(v) || v <= 0 || v > CAPS.MAX_DEADLINE_MS) throw codeError("bad_limits", `${k} must be a finite number in (0, ${CAPS.MAX_DEADLINE_MS}]`)
        out[k] = Math.floor(v)
    }
    return out
}

function moduleBytes(bytes) {
    const b = bytes instanceof Uint8Array ? bytes : (bytes instanceof ArrayBuffer ? new Uint8Array(bytes) : null)
    if (!b) throw codeError("bad_module", "module must be an ArrayBuffer or Uint8Array")
    if (b.length === 0) throw codeError("bad_magic", "empty module")
    if (b.length > CAPS.MAX_MODULE_BYTES) throw codeError("module_too_large", "module exceeds 16 MiB")
    return b.slice().buffer // fresh transferable copy; caller buffer untouched
}

// Cap BEFORE encoding/structured clone. UTF-16 length <= UTF-8 byte length.
function boundedText(s, max, what) {
    if (typeof s !== "string") throw codeError("invalid_input", `${what} must be a string`)
    if (s.length > max) throw codeError("input_too_large", `${what} exceeds limit`)
    if (s.length * 3 > max && enc.encode(s).length > max) throw codeError("input_too_large", `${what} exceeds limit`)
}

export class WasmPluginHost {
    static async create(hostWasmBytes, opts = {}) {
        const { instance } = await WebAssembly.instantiate(hostWasmBytes, {})
        return new WasmPluginHost(hostWasmBytes, instance.exports, opts)
    }

    constructor(hostWasmBytes, exports, opts) {
        this.hostWasm = hostWasmBytes
        this.h = exports
        this.workerUrl = opts.workerUrl || new URL("./plugin-worker.js", import.meta.url)
        this.limits = checkLimits(this._call("host_limits"), opts.limits || {})
        this.instances = new Map() // instance -> rec { worker, generation, busy }
        this.waiters = new Map() // request id -> { resolve, reject, timer, rec, instance, op }
        this.dropped = 0
        this._validators = 0
    }

    _result() {
        const p = this.h.host_result_ptr(); const n = this.h.host_result_len()
        return JSON.parse(dec.decode(new Uint8Array(this.h.memory.buffer).slice(p, p + n)))
    }

    _call(fn, ...a) { const st = this.h[fn](...a); const r = this._result(); if (st !== 0) throw codeError(r.code, r.message); return r }

    // Trusted, small, host-built JSON commands only.
    _fence(cmd) {
        const b = enc.encode(JSON.stringify(cmd))
        const p = this.h.host_alloc(b.length)
        if (!p) throw codeError("bad_command", "fence command too large")
        new Uint8Array(this.h.memory.buffer).set(b, p)
        try { return this._call("host_fence", p, b.length) } finally { this.h.host_free(p, b.length) }
    }

    _settle(id, fn) {
        const w = this.waiters.get(id)
        if (!w) return
        this.waiters.delete(id); clearTimeout(w.timer)
        if (w.rec) w.rec.busy = false
        fn(w)
    }

    _send(rec, instance, msg, scopeKey, revision, op, ms, transfer) {
        if (this.instances.get(instance) !== rec) return Promise.reject(codeError("stale", "instance is not running"))
        if (rec.busy) return Promise.reject(codeError("busy", "a request is already in flight for this instance"))
        let issued
        try { issued = this._fence({ cmd: "issue", instance, scope_key: scopeKey, revision, op }) } catch (e) { return Promise.reject(e) }
        const { id, generation } = issued
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                // Bound to this exact request AND worker incarnation.
                if (!this.waiters.has(id) || this.instances.get(instance) !== rec) return
                this._kill(instance, rec, "deadline")
            }, ms)
            rec.busy = true
            this.waiters.set(id, { resolve, reject, timer, rec, instance, op })
            try {
                rec.worker.postMessage({ ...msg, id, instance, generation, scopeKey, revision, op }, transfer || [])
            } catch (e) {
                this._kill(instance, rec, "post_failed") // rejects this waiter with `post_failed`
            }
        })
    }

    _onMessage(rec, instance, m) {
        try {
            if (!m || typeof m !== "object" || !Number.isSafeInteger(m.id)) { this.dropped++; return }
            const w = this.waiters.get(m.id)
            // Originating worker incarnation is checked BEFORE the Rust fence consumes anything.
            if (!w || w.rec !== rec || this.instances.get(instance) !== rec || m.instance !== instance) { this.dropped++; return }
            if (typeof m.type !== "string" || typeof m.scopeKey !== "string" || !Number.isSafeInteger(m.revision) || !Number.isSafeInteger(m.generation)) { this.dropped++; return }
            const v = this._fence({ cmd: "accept", response: { id: m.id, instance, generation: m.generation, kind: m.type, scope_key: m.scopeKey, revision: m.revision } })
            if (v.verdict !== "accepted") { this.dropped++; return }
            if (m.type === "error") {
                const fatal = m.fatal !== false || w.op === 0 // a failed load/validate leaves no instance behind
                this._settle(m.id, (x) => x.reject(codeError(String(m.code), String(m.message))))
                if (fatal) this._kill(instance, rec, null)
            } else this._settle(m.id, (x) => x.resolve(m))
        } catch (_) { this.dropped++ }
    }

    // Terminate exactly `rec` (no-op if already replaced) and settle its waiters.
    _kill(instance, rec, code) {
        if (this.instances.get(instance) !== rec) return
        try { rec.worker.terminate() } catch (_) {}
        this.instances.delete(instance)
        try { this._fence({ cmd: "retire", instance }) } catch (_) {}
        for (const [id, w] of [...this.waiters]) {
            if (w.rec === rec) this._settle(id, (x) => x.reject(codeError(code || "terminated")))
        }
    }

    _spawn(instance) {
        const { generation } = this._fence({ cmd: "start", instance })
        let worker
        try { worker = new Worker(this.workerUrl) } catch (e) { this._fence({ cmd: "retire", instance }); throw codeError("worker_failed", String(e && e.message)) }
        const rec = { worker, generation, busy: false }
        this.instances.set(instance, rec)
        worker.onmessage = (ev) => this._onMessage(rec, instance, ev.data)
        worker.onmessageerror = () => this._kill(instance, rec, "trap")
        worker.onerror = (ev) => { if (ev && ev.preventDefault) ev.preventDefault(); this._kill(instance, rec, "trap") }
        return rec
    }

    /** Bounded policy validation of an untrusted module in a one-shot terminable Worker. Resolves to the ModuleReport. */
    async validateModule(bytes) {
        const buf = moduleBytes(bytes)
        const instance = CAPS.MAX_INSTANCE_ID + 1 + (this._validators++) // private id space
        const rec = this._spawn(instance)
        try {
            const m = await this._send(rec, instance, { type: "validate", hostWasm: this.hostWasm, pluginWasm: buf }, VALIDATE_SCOPE, 0, 0, this.limits.compile_ms, [buf])
            return m.report
        } finally { this._kill(instance, rec, "done") }
    }

    /** Start a Worker which validates (Rust, before compile), instantiates, describes and creates. Replaces any previous incarnation. */
    async load(instance, pluginBytes, params = "{}") {
        if (!Number.isSafeInteger(instance) || instance < 0 || instance > CAPS.MAX_INSTANCE_ID) throw codeError("invalid_input", "bad instance id")
        const buf = moduleBytes(pluginBytes)
        boundedText(params, CAPS.MAX_PARAMS_BYTES, "params")
        const old = this.instances.get(instance)
        if (old) this._kill(instance, old, "replaced")
        const rec = this._spawn(instance)
        let m
        try {
            m = await this._send(rec, instance, { type: "load", hostWasm: this.hostWasm, pluginWasm: buf, params }, LOAD_SCOPE, 0, 0,
                this.limits.compile_ms + this.limits.instantiate_ms, [buf])
        } catch (e) {
            // ANY failure after spawn (admission/quota, postMessage, worker error,
            // deadline) retires exactly this incarnation; no-op if already replaced.
            this._kill(instance, rec, (e && e.code) || "load_failed")
            throw e
        }
        return { generation: rec.generation, descriptor: m.descriptor }
    }

    /** op: 1 history, 2 live_init, 3 live_update; body: request JSON string. One request in flight per instance (else `busy`). Resolves to validated Output JSON text. */
    async compute(instance, scopeKey, revision, op, body) {
        const rec = this.instances.get(instance)
        if (!rec) throw codeError("stale", "instance is not running")
        if (typeof scopeKey !== "string" || scopeKey.startsWith("__") || !Number.isSafeInteger(revision) || revision < 0) throw codeError("invalid_input", "bad scope/revision")
        if (![1, 2, 3].includes(op)) throw codeError("invalid_input", "unknown operation")
        boundedText(body, CAPS.MAX_INPUT_BYTES, "body")
        const m = await this._send(rec, instance, { type: "compute", body }, scopeKey, revision, op, this.limits.compute_ms)
        return m.output
    }

    /**
      * Scope/revision changed. Every outstanding compute of `instance` for another
      * scope/revision is rejected NOW with `superseded`. Because a dropped request
      * is by construction the one executing (or queued) in that Worker, the
      * concrete Worker incarnation is TERMINATED at once (physical cancellation:
      * no untrusted code keeps running, no later request is needed as a watchdog)
      * and the instance is retired: its live state is gone. The caller MUST
      * `load()` again (and re-send LiveInit) before computing on `instance`.
      * Idle supersede (nothing dropped) only advances the fence; the instance is
      * kept. Returns the number of cancelled requests.
      */
    supersede(instance, scopeKey, revision) {
        const { dropped } = this._fence({ cmd: "supersede", instance, scope_key: scopeKey, revision })
        const recs = new Set()
        for (const id of dropped) { const w = this.waiters.get(id); if (w) recs.add(w.rec) }
        for (const id of dropped) this._settle(id, (w) => w.reject(codeError("superseded")))
        for (const rec of recs) this._kill(instance, rec, "superseded")
        return dropped.length
    }

    /** Disable: terminate the Worker, reject pending work; late results are rejected by the fence. */
    disable(instance) { const rec = this.instances.get(instance); if (rec) this._kill(instance, rec, "disabled") }

    stats() {
        let timers = 0; for (const w of this.waiters.values()) if (w.timer) timers++
        return Object.assign(this._fence({ cmd: "stats" }), { dropped: this.dropped, running: this.instances.size, waiters: this.waiters.size, timers })
    }
}

// ------------------------------------------------------------ Rust coordinator binding
export async function createCoordinator(bytes) {
    const { instance } = await WebAssembly.instantiate(bytes, {})
    const x = instance.exports
    const enc = new TextEncoder(), dec = new TextDecoder()
    // Boundary accounting (calls, JS->Rust and Rust->JS bytes) for measurement.
    const stats = { calls: 0, bytesIn: 0, bytesOut: 0 }
    // `raw` (a Worker's JSON output frame) is appended after one newline and
    // parsed once in Rust: never re-encoded as a string inside the command.
    function call(cmd, raw = null) {
        const input = enc.encode(raw == null ? JSON.stringify(cmd) : `${JSON.stringify(cmd)}\n${raw}`)
        const p = x.coord_alloc(input.length)
        new Uint8Array(x.memory.buffer, p, input.length).set(input)
        const status = x.coord_call(p, input.length)
        x.coord_free(p, input.length)
        const len = x.coord_result_len()
        stats.calls++; stats.bytesIn += input.length; stats.bytesOut += len
        const out = JSON.parse(dec.decode(new Uint8Array(x.memory.buffer, x.coord_result_ptr(), len)))
        if (status !== 0) throw Object.assign(new Error(out.message || out.code), { code: out.code })
        return out
    }
    return {
        history: (scope, revision, bars, context, wantFromIndex, wantToIndex, exhausted, invalidTimes = []) =>
            call({ cmd: "history", scope, revision, bars, context, want_from_index: wantFromIndex, want_to_index: wantToIndex, exhausted: !!exhausted, invalid_times: invalidTimes }),
        live: (plugin, scopeKey, scope, context, tail, invalidTimes = []) => call({ cmd: "live", plugin, scope_key: scopeKey, scope, context, tail, invalid_times: invalidTimes }),
        commit: (plugin, ok) => call({ cmd: "commit", plugin, ok: !!ok }),
        reset: plugin => call({ cmd: "reset", plugin }),
        map: bars => call({ cmd: "map", bars }),
        // Rust-owned scheduling/result authority (crates/browser-coordinator/src/sched.rs).
        plugins: list => call({ cmd: "plugins", list }),
        clearFault: plugin => call({ cmd: "clear_fault", plugin }),
        demand: d => call({ cmd: "demand", ...d }),
        superseded: plugin => call({ cmd: "superseded", plugin }),
        next: plugin => call({ cmd: "next", plugin }),
        done: (plugin, token, error = null, descriptor = undefined) => call({ cmd: "done", plugin, token, ...(error ? { error } : {}), ...(descriptor !== undefined ? { descriptor } : {}) }),
        // Successful compute: the output frame crosses once, merged in Rust.
        doneOutput: (plugin, token, raw) => call({ cmd: "done", plugin, token }, String(raw)),
        invalidate: times => call({ cmd: "invalidate", times }),
        dropValues: plugin => call({ cmd: "drop_values", plugin }),
        presentationFault: plugin => call({ cmd: "presentation_fault", plugin }),
        stats: () => ({ ...stats }),
        pending: plugin => call({ cmd: "pending", plugin }),
        idle: () => call({ cmd: "idle" }),
        inspect: plugin => call({ cmd: "inspect", plugin }),
        running: () => call({ cmd: "running" }),
        // Descriptor/params approval (SDK Descriptor::validate + resolve_params)
        // at restore/import/replace (lenient) and form edit (strict) boundaries.
        approve: (descriptor, params, strict = false) => call({ cmd: "approve", descriptor, params: params ?? null, strict: !!strict }),
    }
}

// ------------------------------------------------------------ Worker driver
// Thin async Worker I/O driver for local indicator plugins.
//
// Every scheduling and result-authority decision lives in the trusted Rust
// coordinator (crates/browser-coordinator/src/sched.rs): readiness, history
// vs live, bounded older pages, warm-up coverage, invalid-window fail-closed,
// bounded live reinit -> stalled, reload vs fault, retire/epoch fencing of
// success AND failure, admission (scope/basis fence + stale_from) and the
// accepted values themselves (merge/precedence/invalidation; each event
// carries a batched `view` diff for the app's read-only projection).
// This file only asks `next`, performs that one Worker call, reports `done`
// (with the raw output frame, once), and forwards the coordinator's events
// to the app callbacks. Each plugin has its own loop, bounded per slice with
// a scheduled continuation. A throwing app callback is a presentation
// failure (NOT a WASM trap): the coordinator faults that plugin truthfully.

const SLICE = 8

export function createLocalCompute({ host, coord, onResult, onStatus, onInvalid, onView, log = () => {}, schedule = f => setTimeout(f, 0) }) {
    const io = new Map()          // id -> { bytes, params } (Worker inputs only)
    const running = new Set()
    let sent = null               // last demand basis sent to Rust (tail diff)

    const wantOf = w => ({ scopeKey: w.scope_key, revision: w.revision, historyRevision: w.history_revision, mode: w.mode })
    // Deliver every event; a callback exception never interrupts the rest
    // (the coordinator already committed them). Returns the first error.
    function emit(events) {
        let failed = null
        for (const e of events ?? []) {
            try {
                if (e.ev === "status") onStatus?.(e.id, { state: e.state, code: e.code, ...(e.coverage !== undefined ? { coverage: e.coverage } : {}), ...(e.needBefore !== undefined ? { needBefore: e.needBefore } : {}) })
                else if (e.ev === "disable") { try { host.disable(e.inst) } catch (_) {} }
                else if (e.ev === "view") onView?.(e.id, e.view)
                else if (e.ev === "invalid") onInvalid?.(e.id, e.want.mode, wantOf(e.want), { from: e.from ?? -Infinity, to: e.to ?? Infinity, reason: e.reason }, e.view)
                else if (e.ev === "result") onResult(e.id, e.want.mode, e.view, wantOf(e.want), {
                    kind: e.kind, op: e.op, touched: e.touched, older: e.older, coverage: e.coverage, needBefore: e.need_before,
                    rangeFrom: e.range_from, rangeTo: e.range_to, staleFrom: e.stale_from ?? Infinity,
                })
                else if (e.ev === "log") log(e.msg, e.id, e.code)
            } catch (error) { failed ??= { id: e.id, error } }
        }
        return failed
    }
    // Bounded presentation failure: fault the affected plugin (values dropped,
    // Worker released, no autorestart); a second callback failure is only logged.
    function presentationFailed(f) {
        log("local plugin presentation error", f.id, f.error)
        if (f.id == null) return
        const again = emit(coord.presentationFault(f.id).events)
        if (again) log("local plugin presentation error", again.id, again.error)
    }
    const code = error => String(error?.code ?? "trap")

    async function run(id) {
        if (running.has(id)) return
        running.add(id)
        try {
            for (let i = 0; i < SLICE && io.has(id); i++) {
                const a = coord.next(id)
                const fa = emit(a.events)
                if (fa) { presentationFailed(fa); break }   // fault also clears any installed inflight token
                if (a.act === "idle") break
                if (a.act === "again") continue
                let r
                if (a.act === "load") {
                    const p = io.get(id)
                    try { const { descriptor } = await host.load(a.inst, p.bytes, JSON.stringify(p.params ?? {})); r = coord.done(id, a.token, null, descriptor) }
                    catch (error) { r = coord.done(id, a.token, code(error)) }
                } else {
                    let raw = null, failure = null
                    try { raw = await host.compute(a.inst, a.scope_key, a.revision, a.op, a.body) } catch (error) { failure = code(error) }
                    r = failure ? coord.done(id, a.token, failure) : coord.doneOutput(id, a.token, raw)
                }
                const fr = emit(r.events)
                if (fr) { presentationFailed(fr); break }
                if (!r.more) break
            }
        } catch (error) { log("local plugin driver error", id, error) }
        finally { running.delete(id) }
        if (io.has(id) && coord.pending(id)) schedule(() => run(id))
    }
    function kickAll() { for (const id of io.keys()) run(id) }

    function setPlugins(list) {
        for (const id of [...io.keys()]) if (!list.some(p => p.id === id)) io.delete(id)
        for (const p of list) io.set(p.id, { bytes: p.bytes, params: p.params })
        const f = emit(coord.plugins(list.map(p => ({ id: p.id, load_key: p.loadKey }))).events)
        if (f) presentationFailed(f)
        kickAll()
    }

    // Send only the bars that changed since the last demand of the same
    // scope + history basis (identity scan, no serialization); Rust patches
    // its mirror, or asks for the full cache when the tail cannot apply.
    function setDemand(next) {
        const base = { scope: next.scope, scope_key: next.scopeKey, revision: next.revision, history_revision: next.historyRevision,
            exhausted: !!next.exhausted, want_from_index: next.wantFromIndex ?? 0, want_to_index: next.wantToIndex ?? next.bars.length,
            older_ranges: (next.olderRanges ?? []).map(r => ({ from_index: r.fromIndex, to_index: r.toIndex, from: r.from, to: r.to })),
            invalid_times: next.invalidTimes ?? [] }
        let r = null
        if (sent && sent.scopeKey === next.scopeKey && sent.historyRevision === next.historyRevision) {
            const old = sent.bars, bars = next.bars, n = Math.min(old.length, bars.length)
            let i = 0
            while (i < n && old[i] === bars[i]) i++
            r = coord.demand({ ...base, tail: bars.slice(i), len: bars.length })
        }
        if (!r || r.need_full) r = coord.demand({ ...base, bars: next.bars })
        // The app mutates page.bars in place (replace/push of new bar
        // objects): keep a reference snapshot (pointer copy, no serialization).
        sent = { scopeKey: next.scopeKey, historyRevision: next.historyRevision, bars: next.bars.slice() }
        if (r.view?.clear) onView?.(null, r.view)
        for (const s of r.supersede ?? []) {
            try { if (host.supersede(s.inst, next.scopeKey, next.revision) > 0) coord.superseded(s.id) } catch (_) { coord.superseded(s.id) }
        }
        kickAll()
    }

    return {
        setPlugins, setDemand,
        clearFault: id => coord.clearFault(id),
        // Synchronous invalidation before a canonical bar changes: returns the
        // view diff the caller must apply before any stale write/readout.
        invalidate: times => coord.invalidate(times),
        dropValues: id => coord.dropValues(id).view,
        stats: () => coord.stats(),
        runtime: id => { const r = coord.inspect(id); if (!r) return undefined; r.olderCov = new Map(Object.entries(r.olderCov ?? {})); return r },
        fault: id => coord.inspect(id)?.fault ?? null,
        running: () => coord.running(),
        idle: () => running.size === 0 && coord.idle(),
        ops: id => ({ ...(coord.inspect(id)?.ops ?? { 2: 0, 3: 0 }) }),
    }
}

// ------------------------------------------------------------ Local plugin packages
// Local (browser-only) indicator plugin packages: pure helpers + IndexedDB
// persistence. Every visitor imports their own .wasm; nothing is uploaded and
// no plugin id/author is special. The module bytes are untrusted and are only
// ever handed to the trusted Worker bridge (WasmPluginHost above),
// which validates them in Rust BEFORE compile (CONTRACT.md §2/§6).
//
// Record (one per descriptor id):
//   { id, version, name, sha256, size, fileName, bytes: ArrayBuffer,
//     descriptor, params: {key: value}, enabled: bool, importedAt }

const DB_NAME = "cvd-local-plugins"
const DB_STORE = "plugins"
const MAX_LOCAL_PLUGINS = 32

// DOM-to-value coercion only (checkbox -> boolean, select -> string, number
// field -> number; blank/non-numeric -> null). Kind/range/option/safe-integer
// approval is the coordinator's `approve` (SDK resolve_params), never here.
export function parseParamInput(spec, text) {
    if (spec.type === "bool") return text === true || text === "true"
    if (spec.type === "choice") return String(text)
    const v = String(text).trim() === "" ? NaN : Number(text)
    return Number.isFinite(v) ? v : null
}

// Pre-read file checks (name/size). Bytes are checked again by the bridge.
export function checkImportFile(file) {
    if (!file || typeof file.name !== "string") throw codeError("bad_module", "no file selected")
    if (!/\.wasm$/i.test(file.name)) throw codeError("bad_file_type", "only .wasm files can be imported")
    if (!(file.size > 0)) throw codeError("bad_magic", "empty file")
    if (file.size > CAPS.MAX_MODULE_BYTES) throw codeError("module_too_large", "module exceeds 16 MiB")
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
    if (!(r.bytes instanceof ArrayBuffer) || r.bytes.byteLength === 0 || r.bytes.byteLength > CAPS.MAX_MODULE_BYTES) return false
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

// IndexedDB
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
