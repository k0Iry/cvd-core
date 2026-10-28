// Frontend half of the former Flow feature tests. Field selection (single vs
// aggregate agb/ags/ad), reconciliation tolerance and "never fall back to
// another scope" now live in the public Rust flow plugin
// (cvd-indicators/indicators/flow, tests). The browser only maps declared
// SDK output values onto chart slots and readouts.
const { test } = require('node:test')
const assert = require('node:assert/strict')
const path = require('node:path')
const { pathToFileURL } = require('node:url')

const load = file => import(pathToFileURL(path.join(__dirname, '../static/js', file)).href)

test('dual_range {upper, lower} maps to the official dual-range slots around base 0', async () => {
    const { dualRangePoint } = await load('plugin-render.mjs')
    assert.deepEqual(dualRangePoint(60, { upper: 125, lower: -80 }), { time: 60, values: [125, 0, -80, 0] })
})

test('invalid dual_range values become whitespace, while true zero flow remains valid', async () => {
    const { dualRangePoint } = await load('plugin-render.mjs')
    for (const value of [undefined, null, 5, [1, 2], { upper: null, lower: 0 }, { upper: Number.NaN, lower: 0 }, { upper: 1, lower: Infinity }, { upper: 1 }]) {
        assert.deepEqual(dualRangePoint(1, value), { time: 1 })
    }
    const zero = dualRangePoint(5, { upper: 0, lower: -0 })
    assert.deepEqual(zero, { time: 5, values: [0, 0, 0, 0] })
    assert.equal(Object.is(zero.values[2], -0), false, 'no negative zero')
})

test('line/histogram keep zero, gap non-finite/absent values and colour by sign', async () => {
    const { linePoint, histogramPoint } = await load('plugin-render.mjs')
    const style = { upColor: 'U', downColor: 'D' }
    assert.deepEqual(linePoint(1, 0), { time: 1, value: 0 })
    for (const value of [undefined, null, Number.NaN, Infinity, '3', {}, { value: 3 }]) assert.deepEqual(linePoint(1, value), { time: 1 })
    assert.deepEqual(histogramPoint(2, -4, style), { time: 2, value: -4, color: 'D' })
    assert.deepEqual(histogramPoint(2, 0, style), { time: 2, value: 0, color: 'U' })
    assert.deepEqual(histogramPoint(2, null, style), { time: 2 })
})

test('readouts derive from kind/label only; missing values read unavailable, zero reads 0', async () => {
    const { normalizeOutput, readoutText } = await load('plugin-registry.mjs')
    const plugin = { id: 'p' }
    const band = normalizeOutput(plugin, { key: 'o', label: 'Buy/Sell', pane: 'separate', type: 'dual_range' }, 0)
    assert.equal(readoutText(band, { upper: 125, lower: -80 }), 'Buy/Sell 125 / -80')
    assert.equal(readoutText(band, null), 'Buy/Sell unavailable / unavailable')
    const line = normalizeOutput(plugin, { key: 'l', label: 'CVD', pane: 'separate', type: 'line' }, 1)
    assert.equal(readoutText(line, 0), 'CVD 0')
    assert.equal(readoutText(line, 1.5e6), 'CVD 1.50M')
    assert.equal(readoutText(line, undefined), 'CVD unavailable')
    assert.equal(readoutText(normalizeOutput(plugin, { key: 's', pane: 'price', type: 'segmented_lines', layers: 2 }, 2), [1, 2]), '')
})

test('descriptor validation follows the SDK shape and rejects unknown types, bad ids/panes and unbounded layers', async () => {
    const { normalizeDescriptor, buildRegistry, activeGroupOutput } = await load('plugin-registry.mjs')
    const d = (outputs, extra = {}) => ({ id: 'p', version: '1', name: 'P', abi: 1, context: { bars: 0 }, outputs, ...extra })
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'js' }])), /unsupported output type/)
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'top', type: 'line' }])), /invalid pane/)
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'line' }], { id: '../x' })), /invalid indicator id/)
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'line' }], { version: '' })), /missing version/)
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'segmented_lines', layers: 33 }])), /1\.\.=32/)
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'line' }, { key: 'o', pane: 'price', type: 'line' }])), /duplicate output key/)
    const plugin = normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'line', colors: ['url(javascript:1)'] }]))
    assert.equal(plugin.outputs[0].style.color.startsWith('#'), true, 'unsafe colour replaced by palette')
    const reg = buildRegistry([
        { id: 'p', status: 'ready', descriptor: d([{ key: 'x', pane: 'separate', pane_group: 'g', type: 'histogram' }, { key: 'y', pane: 'separate', pane_group: 'g', type: 'dual_range' },
            { key: 'z', pane: 'separate', pane_group: 'g', type: 'line' }, { key: 't', pane: 'separate', pane_group: 'g', type: 'line', readout_only: true }]) },
        { id: 'p', status: 'ready', descriptor: d([{ key: 'q', pane: 'price', type: 'line' }], { version: '2' }) },
        { id: 'starting.one', status: 'starting', descriptor: null },
        { id: 'mismatch', status: 'ready', descriptor: d([{ key: 'q', pane: 'price', type: 'line' }]) },
    ])
    assert.deepEqual(reg.rejected.map(r => r.message), ['duplicate indicator id p', 'descriptor id p does not match mismatch'])
    assert.deepEqual(reg.pending, [{ id: 'starting.one', status: 'starting', error: null }])
    assert.equal(reg.panes[1].outputs.find(o => o.id === 't').readoutOnly, true)
    assert.throws(() => normalizeDescriptor(d([{ key: 'o', pane: 'price', type: 'histogram', readout_only: true }])), /readout_only requires a line/)
    const widths = normalizeDescriptor(d([{ key: 's', pane: 'price', type: 'segmented_lines', layers: 3, widths: [2, 4, 6] }, { key: 'l', pane: 'price', type: 'line', widths: [5] }, { key: 'e', pane: 'price', type: 'line' }]))
    assert.deepEqual(widths.outputs[0].layers.map(l => l.lineWidth), [2, 4, 6])
    assert.equal(widths.outputs[1].style.lineWidth, 5)
    assert.equal(widths.outputs[2].style.lineWidth, undefined, 'empty widths keep the native renderer default')
    assert.deepEqual(normalizeDescriptor(d([{ key: 's', pane: 'price', type: 'segmented_lines', layers: 2, widths: [3] }])).outputs[0].layers.map(l => l.lineWidth), [3, 3])
    const pane = reg.panes[1]
    assert.equal(pane.id, 'p:g')
    const group = pane.groups.get('p:g')
    assert.deepEqual(group.outputs.map(o => o.key), ['p/x', 'p/y'], 'bar-type outputs in one pane are alternatives; lines are not')
    assert.equal(activeGroupOutput(group, undefined).key, 'p/x', 'first declared is the default mode')
    assert.equal(activeGroupOutput(group, 'p/y').key, 'p/y')
    assert.equal(activeGroupOutput(group, 'p/missing').key, 'p/x')
})

test('wire: discovery/history/live frames follow the host envelopes; errors reduce to public codes', async () => {
    const { parseDiscovery, historyResults, parseIndicatorFrame, publicError } = await load('plugin-wire.mjs')
    const disc = parseDiscovery({ indicators: [{ id: 'a', status: 'ready', descriptor: { id: 'a' } }, { id: 'b', status: 'starting', descriptor: null },
        { id: 'c', status: 'failed', error: { code: 'incompatible', message: '/opt/x/lib.so: undefined symbol' } }, { id: 'Bad', status: 'ready' }, { id: 'd', status: 'weird' }] })
    assert.deepEqual(disc.map(e => [e.id, e.status, e.error?.code ?? null]), [['a', 'ready', null], ['b', 'starting', null], ['c', 'failed', 'incompatible'], ['d', 'unavailable', null]])
    assert.equal(JSON.stringify(disc).includes('/opt/x'), false, 'no raw paths/messages surface')
    assert.throws(() => parseDiscovery({ api_version: 2, indicators: [] }), /api_version/)
    assert.deepEqual(publicError({ code: 'Not A Code', message: 'x' }), { code: 'unavailable' })
    const res = historyResults({ indicator_api_version: 1, indicators: {
        p: { version: '1', modes: { source: { status: 'ok', output: { times: [60], series: {} } }, aggregate: { status: 'unavailable', error: { code: 'timeout', message: 'm' } } } },
        q: { version: '1', modes: { source: { status: 'ok', output: { times: 'bad' } } } } } })
    assert.deepEqual(res.map(r => [r.id, r.mode, r.ok, r.error?.code ?? null]), [['p', 'source', true, null], ['p', 'aggregate', false, 'timeout'], ['q', 'source', false, 'invalid_output']])
    assert.throws(() => historyResults({ indicator_api_version: 3, indicators: { p: { modes: {} } } }), /api_version/)
    assert.equal(parseIndicatorFrame({ series: 'spot-v1', bar: { time: 1 } }), null, 'base bar frames are not indicator frames')
    assert.deepEqual(parseIndicatorFrame({ type: 'indicator_reset', generation: 7 }), { type: 'reset', generation: 7 })
    const f = parseIndicatorFrame({ type: 'indicator', api_version: 1, id: 'p', version: '1', mode: 'source', generation: 7, session: 3, seq: 2, as_of_rev: 7, output: { times: [], series: {} } })
    assert.deepEqual([f.type, f.session, f.seq, f.asOfRev], ['output', 3, 2, 7], 'as_of_rev carried')
    assert.equal(parseIndicatorFrame({ type: 'indicator', id: 'p', mode: 'source', generation: 7, seq: 0, as_of_rev: 0, output: { times: [], series: {} } }).session, 0, 'missing session = implicit session 0')
    assert.deepEqual(parseIndicatorFrame({ type: 'indicator_status', id: 'p', mode: 'aggregate', generation: 7, status: 'unavailable', error: { code: 'crashed', message: 'stderr...' } }),
        { type: 'status', id: 'p', mode: 'aggregate', generation: 7, session: 0, ok: false, error: { code: 'crashed' } })
    for (const bad of [{ type: 'indicator', id: 'p', mode: 'source', seq: 0, output: { times: [], series: {} } }, { type: 'indicator', id: 'p', mode: 'x', generation: 1, seq: 0, output: {} },
        { type: 'indicator_bogus', id: 'p', mode: 'source', generation: 1 }, { type: 'indicator', api_version: 9, id: 'p', mode: 'source', generation: 1, seq: 0, output: { times: [], series: {} } }]) {
        assert.throws(() => parseIndicatorFrame(bad))
    }
})

test('wire: as_of_rev missing/negative/malformed fails closed (separate from success fixtures)', async () => {
    const { parseIndicatorFrame } = await load('plugin-wire.mjs')
    const ok = { type: 'indicator', api_version: 1, id: 'p', mode: 'source', generation: 7, session: 1, seq: 0, output: { times: [], series: {} } }
    for (const [label, rev] of [['missing', undefined], ['negative', -1], ['float', 1.5], ['string', '7'], ['null', null], ['unsafe', 2 ** 60]]) {
        const frame = { ...ok }
        if (rev !== undefined) frame.as_of_rev = rev
        assert.throws(() => parseIndicatorFrame(frame), /as_of_rev/, label)
    }
})

test('store: as_of_rev vs base rev — stale forming-bar value is pending, never current; races', async () => {
    const { createIndicatorStore } = await load('plugin-store.mjs')
    const st = createIndicatorStore()
    const out = (v, time = 120) => ({ times: [time], series: { v: { type: 'line', values: [v] } } })
    st.applyHistory('p', 'source', out(1))
    assert.equal(st.get('p/v', 'source', 120), 1, 'history snapshot current while no live base rev seen')
    st.noteBaseRev(120, 3)
    assert.equal(st.get('p/v', 'source', 120), undefined, 'live base rev 3 newer than snapshot -> pending')
    const fr = (seq, rev, v) => ({ id: 'p', mode: 'source', generation: 1, session: 1, seq, asOfRev: rev, output: out(v) })
    assert.equal(st.applyFrame(fr(0, 2, 2)).accepted, true)
    assert.equal(st.get('p/v', 'source', 120), undefined, 'as_of_rev 2 < base rev 3 -> pending')
    assert.equal(st.applyFrame(fr(1, 3, 3)).accepted, true)
    assert.equal(st.get('p/v', 'source', 120), 3, 'matching revision -> current')
    assert.equal(st.applyFrame(fr(2, 4, 4)).accepted, true)
    assert.equal(st.get('p/v', 'source', 120), undefined, 'output ahead of OBSERVED base (as_of_rev 4 > observed 3) is pending')
    st.noteBaseRev(180, 4)
    assert.equal(st.get('p/v', 'source', 120), 4, 'rev 4 observed via another bar of the scope -> current')
    // Race: new session restarts seq but carries an older revision for the same bar.
    assert.equal(st.applyFrame({ ...fr(0, 2, 99), session: 2 }).accepted, true)
    st.noteBaseRev(120, 4)
    assert.equal(st.get('p/v', 'source', 120), 4, 'older-revision frame never overwrites a newer value')
    st.reset()
    assert.equal(st.get('p/v', 'source', 120), undefined, 'reset clears values and revisions')
})

test('store: HTTP history overlap never erases/demotes a verified live result (same, changed, late response)', async () => {
    const { createIndicatorStore } = await load('plugin-store.mjs')
    const out = v => ({ times: [60], series: { cvd: { type: 'line', values: [v] } } })
    const live = (v, rev = 5, seq = 1) => ({ id: 'cvd', mode: 'source', generation: 1, session: 1, seq, asOfRev: rev, output: out(v) })
    // (1) parent reproduction: identical tuple
    let st = createIndicatorStore()
    st.noteBaseRev(60, 5); st.applyFrame(live(1))
    assert.equal(st.get('cvd/cvd', 'source', 60), 1)
    st.applyHistory('cvd', 'source', out(1))
    assert.equal(st.get('cvd/cvd', 'source', 60), 1, 'identical history keeps live revision (no permanent gap)')
    // changed tuple: stale overlapping snapshot rejected, live stays current
    st.applyHistory('cvd', 'source', out(7))
    assert.equal(st.get('cvd/cvd', 'source', 60), 1, 'changed overlapping snapshot cannot override verified live')
    // response-after-newer-live: request issued before rev 6, answered after
    st = createIndicatorStore()
    st.noteBaseRev(60, 5); st.applyFrame(live(1)); st.noteBaseRev(60, 6); st.applyFrame(live(2, 6, 2))
    st.applyHistory('cvd', 'source', out(1))
    assert.equal(st.get('cvd/cvd', 'source', 60), 2, 'late older response never replaces newer live value')
    // new epoch (reconnect): history snapshot is accepted again
    st.resetFences(); st.applyHistory('cvd', 'source', out(3))
    assert.equal(st.get('cvd/cvd', 'source', 60), 3, 'after reconnect the snapshot is the current truth')
})

test('store: error status is a terminal fence; old sessions cannot revive; newer session recovers', async () => {
    const { createIndicatorStore } = await load('plugin-store.mjs')
    const st = createIndicatorStore()
    const out = v => ({ times: [60], series: { cvd: { type: 'line', values: [v] } } })
    const fr = (session, seq, v) => ({ id: 'cvd', mode: 'source', generation: 1, session, seq, asOfRev: 5, output: out(v) })
    st.noteBaseRev(60, 5)
    assert.equal(st.applyFrame(fr(3, 1, 1)).accepted, true)
    assert.equal(st.applyStatus({ id: 'cvd', mode: 'source', generation: 1, session: 3, ok: false }), true)
    assert.equal(st.get('cvd/cvd', 'source', 60), undefined, 'error clears output')
    assert.deepEqual(st.applyFrame(fr(2, 9, 999)), { accepted: false, reason: 'stale_session' }, 'parent reproduction rejected')
    assert.equal(st.applyFrame(fr(3, 2, 5)).reason, 'failed_session', 'failed session is terminal')
    assert.equal(st.applyStatus({ id: 'cvd', mode: 'source', generation: 1, session: 3, ok: true }), false, 'failed session cannot report ready')
    assert.equal(st.applyFrame({ ...fr(3, 1, 1), generation: 0 }).accepted, false, 'older generation rejected')
    assert.equal(st.get('cvd/cvd', 'source', 60), undefined)
    assert.equal(st.applyStatus({ id: 'cvd', mode: 'source', generation: 1, session: 4, ok: true }), true, 'newer session ready')
    assert.equal(st.applyFrame(fr(4, 0, 4)).accepted, true)
    assert.equal(st.get('cvd/cvd', 'source', 60), 4, 'recovered')
})

test('store: revision epochs — reconnect restarts counters without cross-epoch comparison; no LRU promotion', async () => {
    const { createIndicatorStore } = await load('plugin-store.mjs')
    const st = createIndicatorStore()
    const out = v => ({ times: [60], series: { cvd: { type: 'line', values: [v] } } })
    const fr = (rev, v, seq = 0) => ({ id: 'cvd', mode: 'source', generation: 9, session: 9, seq, asOfRev: rev, output: out(v) })
    st.noteBaseRev(60, 900); st.applyFrame(fr(900, 1))
    assert.equal(st.get('cvd/cvd', 'source', 60), 1)
    // whole-server restart: generation/session/seq/rev all restart low
    st.resetFences()
    assert.equal(st.applyFrame({ ...fr(2, 2), generation: 1, session: 0 }).accepted, true, 'restarted counters accepted')
    assert.equal(st.get('cvd/cvd', 'source', 60), undefined, 'new epoch: rev 2 not yet observed -> pending (old 900 not reused)')
    st.noteBaseRev(60, 2)
    assert.equal(st.get('cvd/cvd', 'source', 60), 2, 'old-epoch rev 900 neither blocks nor outranks new rev 2')
    st.noteBaseRev(60, 3)
    assert.equal(st.get('cvd/cvd', 'source', 60), undefined, 'new-epoch base rev 3 > as_of_rev 2 -> pending')
    // revision metadata is not LRU-evicted: thousands of other bars do not forget t=60
    for (let t = 1000; t < 1000 + 5000 * 60; t += 60) st.noteBaseRev(t, 1)
    assert.equal(st.get('cvd/cvd', 'source', 60), undefined, 'forgotten revision never promotes old value')
    st.pruneBefore(120)
    assert.equal(st.stats().baseRevs, 5000, 'pruned only with the canonical cache')
})

test('store: columnar values by time; generation/session/seq fencing; restart at seq 0; pending bound', async () => {
    const { createIndicatorStore, PENDING_LIMIT } = await load('plugin-store.mjs')
    const st = createIndicatorStore()
    const out = (times, values) => ({ times, series: { v: { type: 'line', values } } })
    st.applyHistory('p', 'source', out([60, 120], [0, 5]))
    assert.equal(st.get('p/v', 'source', 60), 0, 'zero kept')
    assert.equal(st.get('p/v', 'aggregate', 60), undefined, 'scopes never cross')
    assert.equal(st.get('p/v', 'source', 180), undefined)
    const frame = (generation, session, seq, value, time = 120) => ({ id: 'p', mode: 'source', generation, session, seq, output: out([time], [value]) })
    assert.equal(st.applyFrame(frame(5, 1, 0, 6)).accepted, true)
    assert.equal(st.applyFrame(frame(5, 1, 1, 7)).accepted, true)
    assert.deepEqual(st.applyFrame(frame(5, 1, 1, 99)), { accepted: false, reason: 'stale_seq' }, 'duplicate seq')
    assert.deepEqual(st.applyFrame(frame(5, 1, 0, 99)), { accepted: false, reason: 'stale_seq' }, 'delayed older frame')
    assert.equal(st.get('p/v', 'source', 120), 7)
    // Runner restart: new session re-opens at seq 0 without a generation change.
    assert.equal(st.applyFrame(frame(5, 2, 0, 8)).accepted, true)
    assert.deepEqual(st.applyFrame(frame(5, 1, 2, 99)), { accepted: false, reason: 'stale_session' }, 'old session cannot overwrite')
    assert.equal(st.applyFrame(frame(5, 2, 2, 9)).gap, true, 'seq gap is reported for resync')
    assert.deepEqual(st.applyFrame(frame(4, 9, 9, 99)), { accepted: false, reason: 'stale_generation' })
    assert.equal(st.get('p/v', 'source', 120), 9)
    // Status unavailable drops that stream; older-session status is ignored.
    assert.equal(st.applyStatus({ id: 'p', mode: 'source', generation: 5, session: 1, ok: false }), false)
    assert.equal(st.applyStatus({ id: 'p', mode: 'source', generation: 5, session: 2, ok: false }), true)
    assert.equal(st.get('p/v', 'source', 60), undefined, 'unavailable clears, never stale')
    // Pending results ahead of the base axis are bounded.
    const times = Array.from({ length: PENDING_LIMIT + 50 }, (_, i) => 1000 + i)
    st.applyHistory('p', 'source', out(times, times.map(() => 1)), 999)
    assert.equal(st.stats().points, PENDING_LIMIT)
    st.reset(10)
    assert.equal(st.stats().points, 0)
    assert.equal(st.hubGeneration, 10)
})

test('store: observed-base admission bound — every interleaving keeps visible CVD == displayed base', async () => {
    const { createIndicatorStore } = await load('plugin-store.mjs')
    const out = v => ({ times: [60], series: { cvd: { type: 'line', values: [v] } } })
    const fr = (seq, rev, v, session = 1) => ({ id: 'cvd', mode: 'source', generation: 1, session, seq, asOfRev: rev, output: out(v) })
    const get = st => st.get('cvd/cvd', 'source', 60)
    // Parent repro: live -> future result -> base (no historical supersede).
    let st = createIndicatorStore()
    st.noteBaseRev(60, 1); st.applyFrame(fr(1, 1, 100))
    assert.equal(get(st), 100)
    st.applyFrame(fr(2, 2, 200))
    assert.equal(get(st), undefined, 'asOf 2 > observed 1: pending, never 200 against base 100')
    assert.equal(st.noteBaseRev(60, 2), 'scope', 'matching base observation asks for a scope repaint')
    assert.equal(get(st), 200)
    // Same-value update: future result equal to current value still pending until observed.
    st = createIndicatorStore()
    st.noteBaseRev(60, 1); st.applyFrame(fr(1, 1, 100)); st.applyFrame(fr(2, 2, 100))
    assert.equal(get(st), undefined)
    assert.equal(st.noteBaseRev(60, 2), 'scope'); assert.equal(get(st), 100)
    // Revision observed via ANOTHER bar of the scope (this bar unchanged).
    st = createIndicatorStore()
    st.noteBaseRev(60, 1); st.applyFrame(fr(1, 3, 100))
    assert.equal(get(st), undefined)
    assert.equal(st.noteBaseRev(120, 3), 'scope'); assert.equal(get(st), 100)
    // Output before any base at all (bootstrap): pending.
    st = createIndicatorStore()
    st.applyFrame(fr(1, 0, 7)); assert.equal(get(st), undefined)
    st.noteBaseRev(60, 0); assert.equal(get(st), 7)
    // HTTP overlap + unadmitted future result: supersede must not use the
    // unadmitted rev as floor, so the legitimate later base is accepted.
    st = createIndicatorStore()
    st.noteBaseRev(60, 1); st.applyFrame(fr(1, 1, 100)); st.applyFrame(fr(2, 2, 200))
    st.supersedeBase(60)
    st.applyHistory('cvd', 'source', out(150), null, new Set([60]))
    assert.equal(get(st), 150, 'historical tuple coherent with its snapshot')
    assert.equal(st.baseSuperseded(60, 2), false, 'later matching base rev 2 is NOT rejected')
    st.noteBaseRev(60, 2); st.applyFrame(fr(3, 2, 200))
    assert.equal(get(st), 200)
    assert.equal(st.baseSuperseded(60, 1), true, 'delayed rev 1 base after rev 2 is stale')
    // Reconnect: counters restart; old-epoch observation never admits new results.
    st = createIndicatorStore()
    st.noteBaseRev(60, 50); st.applyFrame(fr(1, 50, 1))
    st.resetFences()
    st.applyFrame({ ...fr(0, 3, 2), generation: 1, session: 0 })
    assert.equal(get(st), undefined, 'new epoch rev 3 not observed yet')
    st.noteBaseRev(60, 3); assert.equal(get(st), 2)
})
