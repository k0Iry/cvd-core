// Browser-local indicator scheduling glue. Substantive planning (bar mapping,
// warm-up coverage, LiveInit/LiveUpdate deltas, seq, session revision,
// reinit) is in the Rust coordinator (`coord`); this file only sequences
// Worker calls and enforces lifetimes:
//   * one shared canonical cache demand: { scope, scopeKey, revision,
//     historyRevision, bars, exhausted, wantFromIndex }; no per-plugin feeds;
//   * op1 History (stateless, fresh instance in the SDK glue) only when the
//     history basis changes (load/prepend/scope) — never contaminates live;
//   * op2 LiveInit / op3 LiveUpdate on the persistent instance for ordinary
//     ticks of `live` plugins; a rejected update drops the session and the
//     next step re-sends LiveInit (explicit reinit, never a History rewrite);
//   * every runtime incarnation (`epoch`) fences success AND failure; retire
//     always calls host.disable, so a Worker still loading is terminated now;
//   * each plugin progresses independently (own loop), bounded per slice with
//     a scheduled continuation so quiet feeds never strand latest work.
import { codeError, contextBars } from "./local-plugins.mjs?v=local-wasm-3"
import { LIVE_REPLACE_SLACK } from "./coordinator.mjs?v=local-wasm-3"

const RELOAD_CODES = new Set(["superseded", "stale", "replaced", "disabled", "terminated"])
const NONFATAL_LIVE = new Set(["sequence_gap", "stale_revision", "invalid_input"])
const MAX_LIVE_RETRIES = 2   // attempts per (scope, data revision) before `stalled`
const SLICE = 8

export function createLocalCompute({ host, coord, onResult, onStatus, onInvalid, log = () => {}, schedule = f => setTimeout(f, 0) }) {
    const runtimes = new Map()
    let nextInstance = 1
    let demand = null
    let stopped = false

    function status(rt, state, code = "", extra = {}) {
        rt.state = state; rt.code = code
        onStatus?.(rt.id, { state, code, ...extra })
    }

    // Physical stop: bump epoch (fences any in-flight success/failure),
    // terminate the Worker even while load is pending, drop live session.
    function retire(rt) {
        rt.epoch++
        try { host.disable(rt.instance) } catch (_) {}
        try { coord.reset(rt.id) } catch (_) {}
        rt.loaded = false
        rt.hist = null
        rt.live = null
        rt.olderDone = null
        rt.olderCov = null
        rt.tailCov = null
    }

    function setPlugins(list) {
        const wanted = new Set(list.map(p => p.id))
        for (const [id, rt] of runtimes) if (!wanted.has(id)) { retire(rt); runtimes.delete(id); onStatus?.(id, { state: "off", code: "" }) }
        for (const p of list) {
            let rt = runtimes.get(p.id)
            if (rt && (rt.loadKey !== p.loadKey || rt.retry)) { retire(rt); rt.fault = null; rt.retry = false }
            if (!rt) {
                rt = { id: p.id, instance: nextInstance++, epoch: 0, loaded: false, hist: null, live: null, fault: null, running: false, state: "starting", code: "" }
                runtimes.set(p.id, rt)
            }
            Object.assign(rt, { bytes: p.bytes, params: p.params, loadKey: p.loadKey })
        }
        kickAll()
    }

    function setDemand(next) {
        const scopeChanged = demand && demand.scopeKey !== next.scopeKey
        demand = next
        if (scopeChanged) for (const rt of runtimes.values()) {
            rt.hist = null; rt.live = null
            try { coord.reset(rt.id) } catch (_) {}
            // Physical cancellation of an in-flight compute for the old scope.
            if (rt.loaded) try { if (host.supersede(rt.instance, next.scopeKey, next.revision) > 0) { rt.epoch++; rt.loaded = false } } catch (_) { rt.epoch++; rt.loaded = false }
        }
        kickAll()
    }

    // Retry after a fault: only marks intent; the caller's following
    // setPlugins (with fresh params/membership) performs the transition.
    function clearFault(id) { const rt = runtimes.get(id); if (rt) rt.retry = true }

    const current = rt => !stopped && runtimes.get(rt.id) === rt
    const isLive = rt => !!rt.descriptor?.live

    function upToDate(rt, want) {
        if (!want?.bars.length) return true
        const histOk = rt.hist && rt.hist.scopeKey === want.scopeKey && rt.hist.historyRevision === (isLive(rt) ? want.historyRevision : want.revision) &&
            rt.hist.keepTime === (want.bars[Math.max(0, Math.min(want.wantFromIndex ?? 0, want.bars.length))]?.time ?? Infinity)
        const liveOk = !isLive(rt) || (rt.live && rt.live.scopeKey === want.scopeKey && rt.live.revision === want.revision)
        return histOk && liveOk && !pendingOlder(rt, want)
    }

    // Bounded older user pages (own origin/prefix/right end), done per
    // (scope, history basis); never re-anchors the newer pages.
    const olderKey = (want, r) => `${want.scopeKey}|${want.historyRevision}|${r.from}|${r.to}|${want.exhausted ? 1 : 0}`
    function pendingOlder(rt, want) {
        for (const r of want.olderRanges ?? []) if (!rt.olderDone?.has(olderKey(want, r))) return r
        return null
    }
    // Bound bookkeeping to the CURRENT scope/basis/pages: keys of superseded
    // revisions/scopes/pages are dropped (never retained indefinitely).
    function pruneOlder(rt, want) {
        if (!rt.olderDone && !rt.olderCov) return
        const keep = new Set((want?.olderRanges ?? []).map(r => olderKey(want, r)))
        for (const k of [...(rt.olderDone ?? [])]) if (!keep.has(k)) rt.olderDone.delete(k)
        for (const k of [...(rt.olderCov?.keys() ?? [])]) if (!keep.has(k)) rt.olderCov.delete(k)
    }
    // Aggregate coverage over the tail plan AND every current older page: an
    // older page lacking its own prefix keeps the plugin non-ready, so the
    // shared warm-up path runs even when the tail itself is already ready.
    function aggregateCoverage(rt) {
        let coverage = rt.tailCov?.coverage ?? "ready", needBefore = rt.tailCov?.needBefore ?? 0
        for (const c of rt.olderCov?.values() ?? []) if (c.coverage !== "ready") {
            if (coverage === "ready" || c.coverage === "unavailable") coverage = c.coverage
            needBefore = Math.max(needBefore, c.needBefore || 0)
        }
        if (coverage === "ready") needBefore = 0
        rt.coverage = coverage
        return { coverage, needBefore }
    }

    async function ensureLoaded(rt, epoch) {
        if (rt.loaded) return true
        status(rt, "starting")
        const { descriptor } = await host.load(rt.instance, rt.bytes, JSON.stringify(rt.params ?? {}))
        if (epoch !== rt.epoch || !current(rt)) return false
        rt.descriptor = typeof descriptor === "string" ? JSON.parse(descriptor) : descriptor
        rt.loaded = true
        rt.hist = null; rt.live = null
        coord.reset(rt.id)   // new generation => LiveInit before any LiveUpdate
        return true
    }

    // One unit of work. Returns false when nothing remains.
    async function step(rt, epoch) {
        const want = demand
        if (rt.fault || upToDate(rt, want)) return false
        if (!(await ensureLoaded(rt, epoch))) return false
        if (epoch !== rt.epoch || demand !== want && demand.scopeKey !== want.scopeKey) return true
        const ctx = contextBars(rt.descriptor, want.scope.tf)
        const histRev = isLive(rt) ? want.historyRevision : want.revision
        const fromIdx = Math.max(0, Math.min(want.wantFromIndex ?? 0, want.bars.length))
        const toIdx = Math.max(fromIdx, Math.min(want.wantToIndex ?? want.bars.length, want.bars.length))
        const wantFromTime = want.bars[fromIdx]?.time ?? Infinity
        const wantToTime = want.bars[toIdx - 1]?.time ?? -Infinity
        const h = rt.hist
        const covered = h && h.scopeKey === want.scopeKey && h.historyRevision === histRev && h.keepTime === wantFromTime &&
            (isLive(rt) || h.toTime >= wantToTime)
        if (!covered) {
            let plan
            try { plan = coord.history(want.scope, want.revision, want.bars, ctx, fromIdx, toIdx, want.exhausted, want.invalidTimes ?? []) }
            catch (error) {
                if (error?.code !== "invalid_window") throw error
                // Fail closed (successor of the server window guard): this
                // basis's History spans an invalid canonical bucket. Withdraw
                // the plugin's values for the window; the live path below may
                // still recover once its context moves past the bucket, and a
                // newer valid replacement changes the basis (recompute).
                rt.hist = { scopeKey: want.scopeKey, historyRevision: histRev, keepTime: wantFromTime, toTime: wantToTime, blocked: true }
                rt.tailCov = { coverage: "unavailable", needBefore: 0 }
                // Values were withdrawn: a live plugin must re-derive its
                // context-clean tail via an explicit LiveInit (never reuse).
                rt.live = null
                try { coord.reset(rt.id) } catch (_) {}
                pruneOlder(rt, want)
                aggregateCoverage(rt)
                onInvalid?.(rt.id, want.scope.mode, want, { from: -Infinity, to: Infinity, reason: error.message })
                status(rt, "unavailable", "invalid_input_window", { coverage: "unavailable" })
                return isLive(rt)
            }
            status(rt, "computing", "", { coverage: plan.coverage })
            const text = await host.compute(rt.instance, want.scopeKey, want.revision, 1, plan.body)
            if (epoch !== rt.epoch || !current(rt) || demand.scopeKey !== want.scopeKey) return true
            // Basis moved (prepend/correction) while computing: discard, recompute.
            if (demand.historyRevision !== want.historyRevision) return true
            rt.hist = { scopeKey: want.scopeKey, historyRevision: histRev, keepTime: wantFromTime, toTime: plan.range_to_time ?? -Infinity }
            rt.tailCov = { coverage: plan.coverage, needBefore: plan.need_before ?? 0 }
            pruneOlder(rt, want)
            const agg = aggregateCoverage(rt)
            rt.plans = [...(rt.plans ?? []).slice(-15), { kind: "tail", trimmed_from: plan.trimmed_from, input_to: plan.input_to, output_from: plan.output_from, range_from_time: plan.range_from_time, range_to_time: plan.range_to_time, coverage: plan.coverage }]
            rt.lastPlan = { trimmed_from: plan.trimmed_from, input_to: plan.input_to, output_from: plan.output_from, range_from_time: plan.range_from_time, range_to_time: plan.range_to_time, coverage: plan.coverage }
            onResult(rt.id, want.scope.mode, JSON.parse(text), want, { kind: "history", coverage: agg.coverage, needBefore: agg.needBefore, rangeFrom: plan.range_from_time, rangeTo: plan.range_to_time })

            if (!isLive(rt)) { status(rt, agg.coverage === "ready" ? "ready" : agg.coverage, "", { coverage: agg.coverage, needBefore: agg.needBefore }); return true }
            // live session must still be (re)initialised below on next step
            return true
        }
        pruneOlder(rt, want)
        const older = pendingOlder(rt, want)
        if (older) {
            let plan
            try { plan = coord.history(want.scope, want.revision, want.bars, ctx, older.fromIndex, older.toIndex, want.exhausted, want.invalidTimes ?? []) }
            catch (error) {
                if (error?.code !== "invalid_window") throw error
                ;(rt.olderDone ??= new Set()).add(olderKey(want, older))
                ;(rt.olderCov ??= new Map()).set(olderKey(want, older), { coverage: "unavailable", needBefore: 0 })
                aggregateCoverage(rt)
                onInvalid?.(rt.id, want.scope.mode, want, { from: older.from, to: older.to, reason: error.message })
                return true
            }
            const text = await host.compute(rt.instance, want.scopeKey, want.revision, 1, plan.body)
            if (epoch !== rt.epoch || !current(rt) || demand.scopeKey !== want.scopeKey || demand.historyRevision !== want.historyRevision) return true
            ;(rt.olderDone ??= new Set()).add(olderKey(want, older))
            ;(rt.olderCov ??= new Map()).set(olderKey(want, older), { coverage: plan.coverage, needBefore: plan.need_before ?? 0 })
            const agg = aggregateCoverage(rt)
            rt.plans = [...(rt.plans ?? []).slice(-15), { kind: "older", trimmed_from: plan.trimmed_from, input_to: plan.input_to, output_from: plan.output_from, range_from_time: plan.range_from_time, range_to_time: plan.range_to_time, coverage: plan.coverage }]
            onResult(rt.id, want.scope.mode, JSON.parse(text), want, { kind: "history", older: true, coverage: agg.coverage, needBefore: agg.needBefore, rangeFrom: plan.range_from_time, rangeTo: plan.range_to_time })
            if (!isLive(rt)) status(rt, agg.coverage === "ready" ? "ready" : agg.coverage, "", { coverage: agg.coverage, needBefore: agg.needBefore })
            return true
        }
        // Live tick: Rust plans LiveInit or a bounded LiveUpdate delta.
        const tail = want.bars.slice(-(ctx + 1 + LIVE_REPLACE_SLACK))
        const plan = coord.live(rt.id, want.scopeKey, want.scope, ctx, tail, want.invalidTimes ?? [])
        if (plan.op === 0 && plan.reason === "invalid_window") {
            // Newest output's context spans an invalid bucket: no session,
            // live values at/after it withdrawn until context moves past it.
            rt.live = { scopeKey: want.scopeKey, revision: want.revision }
            onInvalid?.(rt.id, want.scope.mode, want, { from: plan.touched?.[0] ?? -Infinity, to: Infinity, reason: "live window spans invalid bucket" })
            status(rt, "unavailable", "invalid_input_window", { coverage: "unavailable" })
            return false
        }
        if (plan.op === 0) {
            rt.live = { scopeKey: want.scopeKey, revision: want.revision }
            // Nothing to send: still publish the terminal/ready status now.
            if (rt.state === "computing" || rt.state === "starting" || (rt.code === "invalid_input_window" && !rt.hist?.blocked)) status(rt, rt.coverage === "ready" ? "ready" : rt.coverage ?? "ready", "", { coverage: rt.coverage })
            return true
        }
        let text
        try {
            text = await host.compute(rt.instance, want.scopeKey, want.revision, plan.op, plan.body)
        } catch (error) {
            if (epoch === rt.epoch && NONFATAL_LIVE.has(String(error?.code))) {
                coord.commit(rt.id, false); rt.reinits = (rt.reinits ?? 0) + 1
                // Bounded recovery: one explicit LiveInit retry per data basis.
                // A basis rejected again is parked as `stalled` (actionable,
                // not a fault) until genuinely new input arrives — no spin.
                const key = `${want.scopeKey}|${want.revision}`
                rt.liveReject = rt.liveReject?.key === key ? { key, n: rt.liveReject.n + 1 } : { key, n: 1 }
                if (rt.liveReject.n >= MAX_LIVE_RETRIES) {
                    rt.live = { scopeKey: want.scopeKey, revision: want.revision }
                    status(rt, "stalled", String(error.code), { coverage: rt.coverage })
                    return false
                }
                return true
            }
            throw error
        }
        if (epoch !== rt.epoch || !current(rt)) return true
        coord.commit(rt.id, true)
        rt.ops = rt.ops ?? { 2: 0, 3: 0 }; rt.ops[plan.op]++
        if (demand.scopeKey !== want.scopeKey || demand.historyRevision !== want.historyRevision) return true
        rt.live = { scopeKey: want.scopeKey, revision: want.revision }
        onResult(rt.id, want.scope.mode, JSON.parse(text), want, { kind: "live", op: plan.op, touched: plan.touched })
        if (rt.hist?.blocked) status(rt, "unavailable", "invalid_input_window", { coverage: "unavailable" })
        else status(rt, rt.coverage === "ready" ? "ready" : rt.coverage ?? "ready", "", { coverage: rt.coverage })
        return true
    }

    // Independent per-plugin loop; bounded slice + scheduled continuation.
    async function run(rt) {
        if (rt.running) return
        rt.running = true
        const epoch = rt.epoch
        let more = false
        try {
            for (let i = 0; i < SLICE; i++) {
                if (epoch !== rt.epoch || !current(rt)) break
                if (!(await step(rt, epoch))) { more = false; break }
                more = true
            }
        } catch (error) {
            const code = String(error?.code ?? "trap")
            if (epoch === rt.epoch && current(rt)) {
                if (RELOAD_CODES.has(code)) { rt.loaded = false; coord.reset(rt.id); more = true }
                else {
                    retire(rt)                      // fault: stop/release, no autorestart, no fallback
                    rt.fault = { code }
                    log("local plugin fault", rt.id, code)
                    status(rt, "fault", code)
                }
            }
        } finally { rt.running = false }
        // Epoch changed while running (replace/params/retry): start the new one.
        if (current(rt) && !rt.fault && (more || epoch !== rt.epoch || !upToDate(rt, demand))) schedule(() => run(rt))
    }

    function kickAll() { for (const rt of runtimes.values()) if (!rt.fault) run(rt) }

    return {
        setPlugins, setDemand, clearFault,
        runtime: id => runtimes.get(id),
        fault: id => runtimes.get(id)?.fault ?? null,
        running: () => [...runtimes.values()].filter(rt => rt.loaded).map(rt => rt.id),
        idle: () => [...runtimes.values()].every(rt => rt.fault || (!rt.running && upToDate(rt, demand))),
        ops: id => ({ ...(runtimes.get(id)?.ops ?? { 2: 0, 3: 0 }) }),
        stopAll() { stopped = true; for (const rt of runtimes.values()) retire(rt); runtimes.clear() },
        _codeError: codeError,
    }
}
