// Trusted application-owned plugin Worker (cvd-wasm v1). One Worker per
// plugin instance (or per one-shot validation). It loads the Rust host
// (cvd_browser_host.wasm) for all validation and executes the untrusted plugin
// with the browser's native WebAssembly engine and an EMPTY import object.
// No plugin-supplied JS is ever evaluated. See CONTRACT.md.
'use strict'

const enc = new TextEncoder()
const dec = new TextDecoder()
let host = null // trusted Rust host exports
let plugin = null // untrusted plugin exports
let identity = null

function hostBytes () { return new Uint8Array(host.memory.buffer) }
function hostPut (bytes) {
  if (bytes.length === 0) return [0, 0]
  const p = host.host_alloc(bytes.length)
  if (!p) throw fail('input_too_large', 'host allocation failed')
  hostBytes().set(bytes, p)
  return [p, bytes.length]
}
function hostResult () {
  const p = host.host_result_ptr(); const n = host.host_result_len()
  return JSON.parse(dec.decode(hostBytes().slice(p, p + n)))
}
// Call a host export with a byte argument; throws on host-reported error.
function hostCall (fn, prefix, bytes) {
  const [p, n] = hostPut(bytes || new Uint8Array(0))
  let st
  try { st = host[fn](...prefix, p, n) } finally { if (n) host.host_free(p, n) }
  const r = hostResult()
  if (st !== 0) throw Object.assign(new Error(r.message), { code: r.code })
  return r
}
function hostStatus (st) { const r = hostResult(); if (st !== 0) throw Object.assign(new Error(r.message), { code: r.code }); return r }
function fail (code, message) { return Object.assign(new Error(message), { code }) }

// Copy the plugin's result slot after bounds/limit checks in Rust. MUST run
// immediately after the producing export, before any other plugin export
// except cvd_result_ptr/cvd_result_len (CONTRACT §3 result lifetime).
function pluginResult (kind) {
  const ptr = plugin.cvd_result_ptr() >>> 0; const len = plugin.cvd_result_len() >>> 0
  hostStatus(host.host_check_region(plugin.memory.buffer.byteLength, ptr, len, kind))
  return new Uint8Array(plugin.memory.buffer).slice(ptr, ptr + len)
}
// Fresh copy of input bytes into plugin memory; invoke; COPY the result; only
// then release the input (cvd_dealloc may legally clobber the result slot).
function callWithInput (bytes, fn, kind) {
  if (bytes.length === 0) { const st = fn(0, 0); return [st, pluginResult(kind)] }
  const p = plugin.cvd_alloc(bytes.length) >>> 0
  if (!p || p + bytes.length > plugin.memory.buffer.byteLength) throw fail('invalid_output_region', 'plugin allocation out of bounds')
  new Uint8Array(plugin.memory.buffer).set(bytes, p)
  const st = fn(p, bytes.length)
  const out = pluginResult(kind)
  plugin.cvd_dealloc(p, bytes.length) // a trap here is fatal like any other trap
  return [st, out]
}

async function startHost (m) {
  if (!(m.hostWasm instanceof ArrayBuffer || ArrayBuffer.isView(m.hostWasm)) || !(m.pluginWasm instanceof ArrayBuffer)) throw fail('bad_command', 'bad load message')
  const h = await WebAssembly.instantiate(m.hostWasm, {})
  host = h.instance.exports
  host.host_session_reset()
  const bytes = new Uint8Array(m.pluginWasm)
  return [bytes, hostCall('host_validate_module', [], bytes)] // bounded policy check BEFORE compile
}

async function load (m) {
  const [bytes] = await startHost(m)
  const mod = await WebAssembly.compile(bytes) // structural validity authority
  const inst = await WebAssembly.instantiate(mod, {})
  plugin = inst.exports
  hostStatus(host.host_accept_abi(plugin.cvd_wasm_abi_version() >>> 0))
  const dst = plugin.cvd_describe()
  const desc = hostCall('host_accept_descriptor', [dst], pluginResult(0))
  if (typeof m.params !== 'string') throw fail('invalid_input', 'params must be a string')
  const [cst, cout] = callWithInput(enc.encode(m.params), (p, n) => plugin.cvd_create(p, n), 1)
  hostCall('host_accept_create', [cst], cout)
  return desc
}

function compute (m) {
  if (typeof m.body !== 'string') throw fail('invalid_input', 'body must be a string')
  const body = enc.encode(m.body)
  hostCall('host_begin', [m.op], body)
  const [st, out] = callWithInput(body, (p, n) => plugin.cvd_invoke(m.op, p, n), 1)
  const [p, n] = hostPut(out)
  let s
  try { s = host.host_accept_output(st, p, n) } finally { if (n) host.host_free(p, n) }
  const rp = host.host_result_ptr(); const rn = host.host_result_len()
  const text = dec.decode(hostBytes().slice(rp, rp + rn))
  if (s !== 0) { const e = JSON.parse(text); throw fail(e.code, e.message) }
  return text // validated Output JSON
}

// Nonfatal: host-side admission rejections and plugin-reported errors; the
// committed session state is unchanged. Everything else poisons + closes.
const NONFATAL = ['plugin_error', 'invalid_input', 'sequence_gap', 'stale_revision', 'input_too_large']

self.onmessage = async (ev) => {
  const m = ev.data || {}
  const ids = { id: m.id, instance: m.instance, generation: m.generation, scopeKey: m.scopeKey, revision: m.revision }
  try {
    if (m.type === 'load' || m.type === 'validate') {
      if (identity) throw fail('lifecycle', 'already loaded')
      identity = { instance: m.instance, generation: m.generation }
      if (m.type === 'validate') {
        const [, report] = await startHost(m)
        self.postMessage({ type: 'validated', ...ids, report })
        self.close()
        return
      }
      const descriptor = await load(m)
      self.postMessage({ type: 'loaded', ...ids, descriptor })
    } else if (!identity || m.instance !== identity.instance || m.generation !== identity.generation) {
      throw fail('stale', 'message for another instance')
    } else if (m.type === 'compute') {
      const output = compute(m)
      self.postMessage({ type: 'result', ...ids, output })
    } else throw fail('bad_command', 'unknown message')
  } catch (e) {
    let code = e && e.code
    if (!code) code = (e instanceof WebAssembly.CompileError ? 'malformed_module' : 'trap')
    const fatal = m.type !== 'compute' || !NONFATAL.includes(code)
    if (fatal && host) { try { hostCall('host_poison', [], enc.encode(code === 'memory_limit' ? code : 'trap')) } catch (_) {} }
    self.postMessage({ type: 'error', ...ids, code, message: String((e && e.message) || e), fatal })
    if (fatal) self.close()
  }
}
