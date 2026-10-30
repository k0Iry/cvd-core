// Thin JS binding to the trusted Rust coordinator (crates/browser-coordinator,
// static/wasm-host/cvd_browser_coordinator.wasm). All bar mapping, warm-up
// coverage and live-session (LiveInit/LiveUpdate, seq, session revision,
// reinit) planning happen in Rust; this file only moves JSON across.

export async function createCoordinator(bytes) {
    const { instance } = await WebAssembly.instantiate(bytes, {})
    const x = instance.exports
    const enc = new TextEncoder(), dec = new TextDecoder()
    function call(cmd) {
        const input = enc.encode(JSON.stringify(cmd))
        const p = x.coord_alloc(input.length)
        new Uint8Array(x.memory.buffer, p, input.length).set(input)
        const status = x.coord_call(p, input.length)
        x.coord_free(p, input.length)
        const out = JSON.parse(dec.decode(new Uint8Array(x.memory.buffer, x.coord_result_ptr(), x.coord_result_len()).slice()))
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
    }
}

export const LIVE_REPLACE_SLACK = 64
