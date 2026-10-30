// Node-side numerical oracle for browser harnesses (successor of the retired
// native runner-client.mjs). It executes the SHIPPED trusted plugin Worker
// (static/wasm-host/plugin-worker.js) unmodified inside a node:vm context with
// the shipped Rust host (cvd_browser_host.wasm) and the same plugin .wasm the
// browser imports. No native runner, cdylib or private input is involved.
//
// API mirrors the old RunnerClient: spawn(pluginPath) -> { descriptor,
// descriptorJson, history(scope, outputFrom, bars) -> {ok, output}|{ok:false,error} }.
import fs from 'node:fs'
import vm from 'node:vm'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

// Same static root the browser serves (CVD_STATIC_ROOT, e.g. the assembled
// tree with freshly built host wasm); never an implicit generated copy.
const STATIC = path.resolve(process.env.CVD_STATIC_ROOT || fileURLToPath(new URL('../static', import.meta.url)))
const WORKER_SRC = fs.readFileSync(path.join(STATIC, 'wasm-host/plugin-worker.js'), 'utf8')
const HOST = fs.readFileSync(path.join(STATIC, 'wasm-host/cvd_browser_host.wasm'))
const OP_HISTORY = 1
const ab = buf => buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength)

export class WasmOracleClient {
    static async spawn(pluginPath, params = '{}') {
        const c = new WasmOracleClient()
        const r = await c.post({ type: 'load', hostWasm: ab(HOST), pluginWasm: ab(fs.readFileSync(pluginPath)), params })
        if (r.type !== 'loaded') throw new Error(`oracle load rejected: ${r.code} ${r.message}`)
        c.descriptor = r.descriptor
        c.descriptorJson = JSON.stringify(r.descriptor)
        return c
    }

    constructor() {
        this.waiter = null
        this.closed = false
        this.queue = Promise.resolve()
        this.ids = { id: 0, instance: 1, generation: 1 }
        const self = {
            postMessage: msg => { const w = this.waiter; this.waiter = null; if (w) w(msg) },
            close: () => { this.closed = true },
            onmessage: null,
        }
        this.self = self
        vm.runInNewContext(WORKER_SRC, { self, WebAssembly, TextEncoder, TextDecoder, Uint8Array, ArrayBuffer, JSON, Error, Object, String }, { filename: 'plugin-worker.js' })
    }

    // One message in flight (like one Worker request at a time).
    post(msg) {
        const run = () => new Promise((resolve, reject) => {
            if (this.closed) return reject(new Error('oracle instance closed'))
            this.waiter = resolve
            this.ids.id++
            this.self.onmessage({ data: { ...msg, ...this.ids, scopeKey: 'oracle', revision: 1 } })
        })
        const p = this.queue.then(run, run)
        this.queue = p.catch(() => {})
        return p
    }

    async history(scope, outputFrom, bars) {
        const body = JSON.stringify({ scope, revision: 1, output_from: outputFrom, bars })
        const r = await this.post({ type: 'compute', op: OP_HISTORY, body })
        if (r.type === 'result') return { ok: true, output: JSON.parse(r.output) }
        if (r.fatal) throw new Error(`oracle fatal ${r.code}: ${r.message}`)
        return { ok: false, error: { code: r.code, message: r.message } }
    }

    close() { this.closed = true }
}

// Canonical InputBar subset of a host bar (complete typed row; no extras).
export const INPUT_FIELDS = ['time', 'po', 'ph', 'pl', 'price_close', 'apo', 'aph', 'apl', 'apc', 'cvd_usdt', 'ac', 'delta_usdt', 'ad',
    'bvb', 'abvb', 'svb', 'asvb', 'buy_volume_usdt', 'agb', 'sell_volume_usdt', 'ags', 'trades']
export function inputBar(bar) {
    const out = {}
    for (const key of INPUT_FIELDS) {
        if (!Number.isFinite(bar[key])) throw new Error(`incomplete canonical bar: ${key}`)
        out[key] = bar[key]
    }
    return out
}
