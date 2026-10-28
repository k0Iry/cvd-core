// Minimal Node client for the real public `cvd-indicator-runner` (frame
// protocol v1, docs/plugin-api-v1.md §7), used by the browser harness to serve
// REAL cdylib output in the host's /indicators, /history and /ws envelopes.
// Header: "CVDP" | u16 version=1 | u16 kind | u64 request_id | u64 session |
// u64 generation | u32 params_len | u32 body_len (little endian).
import { spawn } from 'node:child_process'

const KIND = { Hello: 1, Ready: 2, History: 10, Result: 20, Error: 21, Shutdown: 30 }
const HEADER = 40

export class RunnerClient {
    static async spawn(runnerPath, libraryPath) {
        const child = spawn(runnerPath, ['--library', libraryPath], { env: {}, cwd: '/', stdio: ['pipe', 'pipe', 'inherit'] })
        const client = new RunnerClient(child)
        const [kind, body] = await client.request(KIND.Hello, Buffer.alloc(0), Buffer.from(JSON.stringify({ protocol: 1, abi: 1 })))
        if (kind !== KIND.Ready) throw new Error(`runner handshake rejected: ${body}`)
        client.descriptorJson = body.toString('utf8')
        client.descriptor = JSON.parse(client.descriptorJson)
        return client
    }

    constructor(child) {
        this.child = child
        this.nextId = 1n
        this.buffer = Buffer.alloc(0)
        this.waiters = []
        this.queue = Promise.resolve()
        child.stdout.on('data', chunk => { this.buffer = Buffer.concat([this.buffer, chunk]); this.drain() })
        child.on('exit', () => { for (const w of this.waiters.splice(0)) w.reject(new Error('runner exited')) })
    }

    drain() {
        while (this.buffer.length >= HEADER && this.waiters.length) {
            const h = this.buffer
            if (h.toString('latin1', 0, 4) !== 'CVDP') throw new Error('bad magic')
            const kind = h.readUInt16LE(6)
            const id = h.readBigUInt64LE(8)
            const len = h.readUInt32LE(32) + h.readUInt32LE(36)
            if (this.buffer.length < HEADER + len) return
            const body = this.buffer.subarray(HEADER + h.readUInt32LE(32), HEADER + len)
            this.buffer = this.buffer.subarray(HEADER + len)
            const waiter = this.waiters.shift()
            if (id !== waiter.id) waiter.reject(new Error('request id mismatch'))
            else waiter.resolve([kind, Buffer.from(body)])
        }
    }

    // Requests are serialized: one in flight per runner, like the host.
    request(kind, params, body) {
        const run = () => new Promise((resolve, reject) => {
            const id = this.nextId++
            const header = Buffer.alloc(HEADER)
            header.write('CVDP', 0, 'latin1')
            header.writeUInt16LE(1, 4)
            header.writeUInt16LE(kind, 6)
            header.writeBigUInt64LE(id, 8)
            header.writeBigUInt64LE(0n, 16)
            header.writeBigUInt64LE(1n, 24)
            header.writeUInt32LE(params.length, 32)
            header.writeUInt32LE(body.length, 36)
            this.waiters.push({ id, resolve, reject })
            this.child.stdin.write(Buffer.concat([header, params, body]))
        })
        const result = this.queue.then(run, run)
        this.queue = result.catch(() => {})
        return result
    }

    // -> { ok: true, output } | { ok: false, error }
    async history(scope, outputFrom, bars) {
        const body = Buffer.from(JSON.stringify({ scope, revision: 1, output_from: outputFrom, bars }))
        const [kind, out] = await this.request(KIND.History, Buffer.from('{}'), body)
        const doc = JSON.parse(out.toString('utf8'))
        return kind === KIND.Result ? { ok: true, output: doc } : { ok: false, error: doc }
    }

    close() {
        try { this.child.stdin.end() } catch {}
        this.child.kill('SIGTERM')
    }
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
