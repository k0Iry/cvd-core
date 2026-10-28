// Client-side indicator result store. Columnar SDK outputs are written here
// keyed by (output key, mode, canonical bar time); base bars are never
// modified. Rendering only ever iterates host bars, so results for times the
// host does not (yet) have are invisible and can never extend the time axis;
// they merge automatically once the base bar arrives (bounded pending set).
//
// Live frame fencing per (indicator, mode) stream: hub generation, runner
// session, then seq. Older generations/sessions and non-increasing seq are
// dropped; a newer session starts over at any seq (runner restart re-opens at
// seq 0 without necessarily changing the hub generation). An error status is
// a TERMINAL fence for its session: the watermark is retained, so neither the
// failed session nor any older one can revive the stream; only a newer
// session (or generation) recovers it.
//
// Revisions: base `rev` / frame `as_of_rev` are counters of one socket/server
// lifetime ("epoch"). Every reconnect/reset opens a new epoch and revisions of
// different epochs are never compared numerically.

import { seriesValueAt } from "./plugin-wire.mjs?v=plugin-ui-2"

export const PENDING_LIMIT = 256

export function createIndicatorStore() {
    const values = new Map()      // `${id}/${key}|${mode}` -> Map(time -> value)
    const streams = new Map()     // `${id}|${mode}` -> { generation, session, seq, failed }
    let hubGeneration = null
    let epoch = 0
    // Newest live base rev per canonical bar time, scoped to `epoch`. Pruned
    // only together with the canonical cache (pruneBefore), never by LRU, so
    // forgetting a rev can never promote an old value to current.
    const baseRevs = new Map()    // time -> { epoch, rev }
    // `${outputKey}|${mode}` -> Map(time -> { epoch, rev }). rev null = HTTP
    // snapshot (no revision): current unless the bar has a live base rev in
    // this epoch, in which case it is pending until a matching live frame.
    const revs = new Map()
    // Highest base rev actually OBSERVED in this epoch for this scope (any
    // bar). as_of_rev is a scope-wide snapshot counter: a result computed at
    // a revision the client has not yet observed describes base data it does
    // not display, so it is pending regardless of which bar it concerns.
    // Never compared across epochs.
    let observed = { epoch: 0, rev: -1 }
    const observedRev = () => observed.epoch === epoch ? observed.rev : -1
    const admitted = r => !!r && r.epoch === epoch && r.rev != null && r.rev <= observedRev()

    const column = (outputKey, mode) => {
        const k = `${outputKey}|${mode}`
        let col = values.get(k)
        if (!col) values.set(k, col = new Map())
        return col
    }

    function write(id, mode, output, newestBaseTime, asOfRev, fromHistory, changed = null) {
        const touched = []
        for (const [key, series] of Object.entries(output.series)) {
            const col = column(`${id}/${key}`, mode)
            const rk = `${id}/${key}|${mode}`
            let rcol = revs.get(rk)
            if (!rcol) revs.set(rk, rcol = new Map())
            output.times.forEach((time, i) => {
                if (!Number.isSafeInteger(time)) return
                const value = seriesValueAt(series, i)
                if (value === undefined) return
                const prev = rcol.get(time)
                const live = !!prev && prev.rev != null && prev.epoch === epoch
                if (fromHistory) {
                    // A snapshot has no revision, so it can never prove it is
                    // newer than a verified live result of this epoch. Same
                    // value = unchanged input: keep the live revision. Other
                    // value = possibly stale overlapping response: rejected
                    // for this bar (live stays current; no gap is created).
                    // Exception: the host accepted a DIFFERENT canonical input
                    // tuple for this bar from this same response (`changed`);
                    // supersedeBase already invalidated the old live proof,
                    // which described another bar, so the snapshot wins.
                    if (live && !changed?.has(time)) return
                } else {
                    if (live && asOfRev != null && prev.rev > asOfRev) return
                    // Bar superseded by an accepted historical tuple: results at
                    // or below the superseded revision are stale forever; newer
                    // ones are kept pending until their base is displayed.
                    const base = baseRevs.get(time)
                    if (base?.historical && base.epoch === epoch && (asOfRev == null || asOfRev <= base.rev)) return
                }
                col.set(time, value)
                rcol.set(time, { epoch, rev: fromHistory ? null : asOfRev })
            })
            if (newestBaseTime != null) {
                const pending = [...col.keys()].filter(t => t > newestBaseTime)
                for (const t of pending.slice(0, Math.max(0, pending.length - PENDING_LIMIT))) { col.delete(t); rcol.delete(t) }
            }
        }
        for (const time of output.times) if (Number.isSafeInteger(time)) touched.push(time)
        return touched
    }

    function dropStream(id, mode) {
        for (const k of [...values.keys()]) if (k.startsWith(`${id}/`) && k.endsWith(`|${mode}`)) { values.delete(k); revs.delete(k) }
    }

    return {
        // True if a live base frame with `rev` for `time` predates an accepted
        // historical replacement of that bar; the host must not display it.
        baseSuperseded(time, rev) {
            const base = baseRevs.get(time)
            // Delayed live base frame older than the one already observed for
            // this bar in this epoch: stale, never displayed over newer data.
            if (base && !base.historical && base.epoch === epoch && Number.isSafeInteger(rev) && rev < base.rev) return true
            return !!base?.historical && base.epoch === epoch && (Number.isSafeInteger(rev) ? rev <= base.rev : base.rev >= 0)
        },
        // The host is replacing the canonical bar at `time` with a different
        // historical tuple. Prior live proof (base rev and every indicator's
        // as_of_rev) described another bar: invalidate it. Results are
        // pending until a coherent snapshot (or a live frame proven newer than
        // the superseded revision) arrives; delayed live frames at or below
        // that revision can never restore authority.
        supersedeBase(time) {
            if (!Number.isSafeInteger(time)) return
            let floor = -1
            const base = baseRevs.get(time)
            if (base && base.epoch === epoch) floor = base.rev
            for (const [k, rcol] of revs) {
                const r = rcol.get(time)
                if (!r) continue
                // Only ADMITTED (observed) live revisions were proof that was
                // shown; an unadmitted future result must not raise the floor
                // and so reject its own later matching base.
                if (admitted(r)) floor = Math.max(floor, r.rev)
                rcol.delete(time)
                values.get(k)?.delete(time)
            }
            baseRevs.set(time, { epoch, rev: floor, historical: true })
        },
        noteBaseRev(time, rev) {
            if (!Number.isSafeInteger(rev) || rev < 0 || !Number.isSafeInteger(time)) return false
            const cur = baseRevs.get(time)
            let scope = false
            if (rev > observedRev()) {
                // Observation bound advanced: future results anywhere in the
                // scope may become current -> caller repaints the whole scope.
                const lo = observedRev()
                scope = [...revs.values()].some(c => [...c.values()].some(r => r.epoch === epoch && r.rev != null && r.rev > lo && r.rev <= rev))
                observed = { epoch, rev }
            }
            if (cur && cur.epoch === epoch && cur.rev >= rev) return scope ? "scope" : false
            baseRevs.set(time, { epoch, rev })
            // Authority changed: renderability of stored results may differ.
            return scope ? "scope" : true
        },
        // The canonical cache no longer holds bars older than `time`; their
        // revision metadata leaves with them (never rendered again).
        pruneBefore(time) {
            if (!Number.isSafeInteger(time)) return
            for (const t of [...baseRevs.keys()]) if (t < time) baseRevs.delete(t)
        },
        // Current value, or undefined if absent or pending on a newer base rev.
        get(outputKey, mode, time) {
            const col = values.get(`${outputKey}|${mode}`)
            if (!col || !col.has(time)) return undefined
            const base = baseRevs.get(time)
            if (base && base.epoch === epoch && base.historical) {
                // Displayed bar is a historical tuple: only its snapshot is
                // current. Newer live results stay pending until their live
                // base frame (rev > superseded) is accepted and displayed.
                const r = revs.get(`${outputKey}|${mode}`)?.get(time)
                if (!r || r.epoch !== epoch || r.rev != null) return undefined
            } else if (base && base.epoch === epoch) {
                const r = revs.get(`${outputKey}|${mode}`)?.get(time)
                if (!r || r.rev == null || r.epoch !== epoch || r.rev < base.rev) return undefined
            }
            // Live result computed at a revision not yet observed: pending.
            const own = revs.get(`${outputKey}|${mode}`)?.get(time)
            if (own && own.rev != null && own.epoch === epoch && own.rev > observedRev()) return undefined
            return col.get(time)
        },
        // `changedTimes`: times whose canonical input tuple in the same
        // response differs from the displayed bar and is being accepted
        // (after supersedeBase). Unchanged times keep verified live proof.
        applyHistory(id, mode, output, newestBaseTime = null, changedTimes = null) {
            return write(id, mode, output, newestBaseTime, null, true, changedTimes)
        },
        // Live frame -> { accepted, reason?, touched?, gap? }
        applyFrame(frame, newestBaseTime = null) {
            if (hubGeneration !== null && frame.generation < hubGeneration) return { accepted: false, reason: "stale_generation" }
            if (hubGeneration === null || frame.generation > hubGeneration) hubGeneration = frame.generation
            const key = `${frame.id}|${frame.mode}`
            const cur = streams.get(key)
            let gap = false
            if (cur && frame.generation < cur.generation) return { accepted: false, reason: "stale_generation" }
            if (cur && cur.generation === frame.generation) {
                if (frame.session < cur.session) return { accepted: false, reason: "stale_session" }
                if (frame.session === cur.session) {
                    if (cur.failed) return { accepted: false, reason: "failed_session" }
                    if (frame.seq <= cur.seq) return { accepted: false, reason: "stale_seq" }
                    gap = frame.seq !== cur.seq + 1
                }
            }
            streams.set(key, { generation: frame.generation, session: frame.session, seq: frame.seq, failed: false })
            const rev = Number.isSafeInteger(frame.asOfRev) ? frame.asOfRev : null
            return { accepted: true, gap, touched: write(frame.id, frame.mode, frame.output, newestBaseTime, rev, false) }
        },
        // Status frames advance the fence. ok=false keeps a terminal fence.
        applyStatus(event) {
            if (hubGeneration !== null && event.generation < hubGeneration) return false
            hubGeneration = Math.max(hubGeneration ?? event.generation, event.generation)
            const key = `${event.id}|${event.mode}`
            const cur = streams.get(key)
            if (cur && event.generation < cur.generation) return false
            if (cur && cur.generation === event.generation && event.session < cur.session) return false
            const same = !!cur && cur.generation === event.generation && cur.session === event.session
            if (!event.ok) {
                dropStream(event.id, event.mode)
                streams.set(key, { generation: event.generation, session: event.session, seq: same ? cur.seq : -1, failed: true })
            } else if (!same) streams.set(key, { generation: event.generation, session: event.session, seq: -1, failed: false })
            else if (cur.failed) return false   // a failed session cannot report ready again
            return true
        },
        reset(generation = null) {
            values.clear()
            revs.clear()
            streams.clear()
            baseRevs.clear()
            hubGeneration = generation
            epoch++
        },
        // New connection: generation/session/seq/rev counters restart. Keep
        // data, open a new revision epoch.
        resetFences() {
            streams.clear()
            hubGeneration = null
            epoch++
        },
        dropStream,
        get hubGeneration() { return hubGeneration },
        get epoch() { return epoch },
        stats() { return { columns: values.size, streams: streams.size, baseRevs: baseRevs.size, points: [...values.values()].reduce((n, c) => n + c.size, 0) } },
    }
}
