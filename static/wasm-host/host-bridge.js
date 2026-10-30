// Thin trusted main-thread bridge for cvd-wasm v1 plugins (no UI code).
// Response acceptance is decided by the Rust fence (cvd_browser_host.wasm,
// `host_fence`); this file only moves bytes, owns Workers and timers.
// Untrusted module parsing NEVER runs on this thread: it happens inside the
// terminable plugin Worker under the load deadline. API: CONTRACT.md §6/§7.

const enc = new TextEncoder()
const dec = new TextDecoder()
const err = (code, message) => Object.assign(new Error(message || code), { code })

// Host-enforced caps (mirror cvd_indicator_sdk::wasm constants; CONTRACT §3).
export const CAPS = Object.freeze({
  MAX_MODULE_BYTES: 16 << 20,
  MAX_PARAMS_BYTES: 64 << 10,
  MAX_INPUT_BYTES: 32 << 20,
  MAX_DEADLINE_MS: 60000,
  MAX_INSTANCE_ID: 2 ** 52 - 1
})
const LOAD_SCOPE = '__load'
const VALIDATE_SCOPE = '__validate'

function checkLimits (base, over) {
  const out = { ...base }
  for (const [k, v] of Object.entries(over)) {
    if (!['compile_ms', 'instantiate_ms', 'compute_ms'].includes(k)) throw err('bad_limits', `unknown limit ${k}`)
    if (typeof v !== 'number' || !Number.isFinite(v) || v <= 0 || v > CAPS.MAX_DEADLINE_MS) throw err('bad_limits', `${k} must be a finite number in (0, ${CAPS.MAX_DEADLINE_MS}]`)
    out[k] = Math.floor(v)
  }
  return out
}

function moduleBytes (bytes) {
  const b = bytes instanceof Uint8Array ? bytes : (bytes instanceof ArrayBuffer ? new Uint8Array(bytes) : null)
  if (!b) throw err('bad_module', 'module must be an ArrayBuffer or Uint8Array')
  if (b.length === 0) throw err('bad_magic', 'empty module')
  if (b.length > CAPS.MAX_MODULE_BYTES) throw err('module_too_large', 'module exceeds 16 MiB')
  return b.slice().buffer // fresh transferable copy; caller buffer untouched
}

// Cap BEFORE encoding/structured clone. UTF-16 length <= UTF-8 byte length.
function boundedText (s, max, what) {
  if (typeof s !== 'string') throw err('invalid_input', `${what} must be a string`)
  if (s.length > max) throw err('input_too_large', `${what} exceeds limit`)
  if (s.length * 3 > max && enc.encode(s).length > max) throw err('input_too_large', `${what} exceeds limit`)
}

export class WasmPluginHost {
  static async create (hostWasmBytes, opts = {}) {
    const { instance } = await WebAssembly.instantiate(hostWasmBytes, {})
    return new WasmPluginHost(hostWasmBytes, instance.exports, opts)
  }

  constructor (hostWasmBytes, exports, opts) {
    this.hostWasm = hostWasmBytes
    this.h = exports
    this.workerUrl = opts.workerUrl || new URL('./plugin-worker.js', import.meta.url)
    this.limits = checkLimits(this._call('host_limits'), opts.limits || {})
    this.instances = new Map() // instance -> rec { worker, generation, busy }
    this.waiters = new Map() // request id -> { resolve, reject, timer, rec, instance, op }
    this.dropped = 0
    this._validators = 0
  }

  _result () {
    const p = this.h.host_result_ptr(); const n = this.h.host_result_len()
    return JSON.parse(dec.decode(new Uint8Array(this.h.memory.buffer).slice(p, p + n)))
  }

  _call (fn, ...a) { const st = this.h[fn](...a); const r = this._result(); if (st !== 0) throw err(r.code, r.message); return r }

  // Trusted, small, host-built JSON commands only.
  _fence (cmd) {
    const b = enc.encode(JSON.stringify(cmd))
    const p = this.h.host_alloc(b.length)
    if (!p) throw err('bad_command', 'fence command too large')
    new Uint8Array(this.h.memory.buffer).set(b, p)
    try { return this._call('host_fence', p, b.length) } finally { this.h.host_free(p, b.length) }
  }

  _settle (id, fn) {
    const w = this.waiters.get(id)
    if (!w) return
    this.waiters.delete(id); clearTimeout(w.timer)
    if (w.rec) w.rec.busy = false
    fn(w)
  }

  _send (rec, instance, msg, scopeKey, revision, op, ms, transfer) {
    if (this.instances.get(instance) !== rec) return Promise.reject(err('stale', 'instance is not running'))
    if (rec.busy) return Promise.reject(err('busy', 'a request is already in flight for this instance'))
    let issued
    try { issued = this._fence({ cmd: 'issue', instance, scope_key: scopeKey, revision, op }) } catch (e) { return Promise.reject(e) }
    const { id, generation } = issued
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        // Bound to this exact request AND worker incarnation.
        if (!this.waiters.has(id) || this.instances.get(instance) !== rec) return
        this._kill(instance, rec, 'deadline')
      }, ms)
      rec.busy = true
      this.waiters.set(id, { resolve, reject, timer, rec, instance, op })
      try {
        rec.worker.postMessage({ ...msg, id, instance, generation, scopeKey, revision, op }, transfer || [])
      } catch (e) {
        this._kill(instance, rec, 'post_failed') // rejects this waiter with `post_failed`
      }
    })
  }

  _onMessage (rec, instance, m) {
    try {
      if (!m || typeof m !== 'object' || !Number.isSafeInteger(m.id)) { this.dropped++; return }
      const w = this.waiters.get(m.id)
      // Originating worker incarnation is checked BEFORE the Rust fence consumes anything.
      if (!w || w.rec !== rec || this.instances.get(instance) !== rec || m.instance !== instance) { this.dropped++; return }
      if (typeof m.type !== 'string' || typeof m.scopeKey !== 'string' || !Number.isSafeInteger(m.revision) || !Number.isSafeInteger(m.generation)) { this.dropped++; return }
      const v = this._fence({ cmd: 'accept', response: { id: m.id, instance, generation: m.generation, kind: m.type, scope_key: m.scopeKey, revision: m.revision } })
      if (v.verdict !== 'accepted') { this.dropped++; return }
      if (m.type === 'error') {
        const fatal = m.fatal !== false || w.op === 0 // a failed load/validate leaves no instance behind
        this._settle(m.id, (x) => x.reject(err(String(m.code), String(m.message))))
        if (fatal) this._kill(instance, rec, null)
      } else this._settle(m.id, (x) => x.resolve(m))
    } catch (_) { this.dropped++ }
  }

  // Terminate exactly `rec` (no-op if already replaced) and settle its waiters.
  _kill (instance, rec, code) {
    if (this.instances.get(instance) !== rec) return
    try { rec.worker.terminate() } catch (_) {}
    this.instances.delete(instance)
    try { this._fence({ cmd: 'retire', instance }) } catch (_) {}
    for (const [id, w] of [...this.waiters]) {
      if (w.rec === rec) this._settle(id, (x) => x.reject(err(code || 'terminated')))
    }
  }

  _spawn (instance) {
    const { generation } = this._fence({ cmd: 'start', instance })
    let worker
    try { worker = new Worker(this.workerUrl) } catch (e) { this._fence({ cmd: 'retire', instance }); throw err('worker_failed', String(e && e.message)) }
    const rec = { worker, generation, busy: false }
    this.instances.set(instance, rec)
    worker.onmessage = (ev) => this._onMessage(rec, instance, ev.data)
    worker.onmessageerror = () => this._kill(instance, rec, 'trap')
    worker.onerror = (ev) => { if (ev && ev.preventDefault) ev.preventDefault(); this._kill(instance, rec, 'trap') }
    return rec
  }

  /** Bounded policy validation of an untrusted module in a one-shot terminable Worker. Resolves to the ModuleReport. */
  async validateModule (bytes) {
    const buf = moduleBytes(bytes)
    const instance = CAPS.MAX_INSTANCE_ID + 1 + (this._validators++) // private id space
    const rec = this._spawn(instance)
    try {
      const m = await this._send(rec, instance, { type: 'validate', hostWasm: this.hostWasm, pluginWasm: buf }, VALIDATE_SCOPE, 0, 0, this.limits.compile_ms, [buf])
      return m.report
    } finally { this._kill(instance, rec, 'done') }
  }

  /** Start a Worker which validates (Rust, before compile), instantiates, describes and creates. Replaces any previous incarnation. */
  async load (instance, pluginBytes, params = '{}') {
    if (!Number.isSafeInteger(instance) || instance < 0 || instance > CAPS.MAX_INSTANCE_ID) throw err('invalid_input', 'bad instance id')
    const buf = moduleBytes(pluginBytes)
    boundedText(params, CAPS.MAX_PARAMS_BYTES, 'params')
    const old = this.instances.get(instance)
    if (old) this._kill(instance, old, 'replaced')
    const rec = this._spawn(instance)
    let m
    try {
      m = await this._send(rec, instance, { type: 'load', hostWasm: this.hostWasm, pluginWasm: buf, params }, LOAD_SCOPE, 0, 0,
        this.limits.compile_ms + this.limits.instantiate_ms, [buf])
    } catch (e) {
      // ANY failure after spawn (admission/quota, postMessage, worker error,
      // deadline) retires exactly this incarnation; no-op if already replaced.
      this._kill(instance, rec, (e && e.code) || 'load_failed')
      throw e
    }
    return { generation: rec.generation, descriptor: m.descriptor }
  }

  /** op: 1 history, 2 live_init, 3 live_update; body: request JSON string. One request in flight per instance (else `busy`). Resolves to validated Output JSON text. */
  async compute (instance, scopeKey, revision, op, body) {
    const rec = this.instances.get(instance)
    if (!rec) throw err('stale', 'instance is not running')
    if (typeof scopeKey !== 'string' || scopeKey.startsWith('__') || !Number.isSafeInteger(revision) || revision < 0) throw err('invalid_input', 'bad scope/revision')
    if (![1, 2, 3].includes(op)) throw err('invalid_input', 'unknown operation')
    boundedText(body, CAPS.MAX_INPUT_BYTES, 'body')
    const m = await this._send(rec, instance, { type: 'compute', body }, scopeKey, revision, op, this.limits.compute_ms)
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
  supersede (instance, scopeKey, revision) {
    const { dropped } = this._fence({ cmd: 'supersede', instance, scope_key: scopeKey, revision })
    const recs = new Set()
    for (const id of dropped) { const w = this.waiters.get(id); if (w) recs.add(w.rec) }
    for (const id of dropped) this._settle(id, (w) => w.reject(err('superseded')))
    for (const rec of recs) this._kill(instance, rec, 'superseded')
    return dropped.length
  }

  /** Disable: terminate the Worker, reject pending work; late results are rejected by the fence. */
  disable (instance) { const rec = this.instances.get(instance); if (rec) this._kill(instance, rec, 'disabled') }

  stats () {
    let timers = 0; for (const w of this.waiters.values()) if (w.timer) timers++
    return Object.assign(this._fence({ cmd: 'stats' }), { dropped: this.dropped, running: this.instances.size, waiters: this.waiters.size, timers })
  }
}
