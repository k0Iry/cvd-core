// Actual-app DATA acceptance cases (PARENT-B-REV4-DATA-SLICE). Included by
// tests/local-wasm-browser.mjs when CVD_LOCAL_SUITE=data. Real public WASM in
// real Workers; only the owned host.compute RESULT promise is delayed (the
// indicator output itself is never mocked). Every case name below is required.
export const DATA_CASES = [
  'window_plan_bounded_and_actual_prefix',
  'tail_same_time_update_during_delayed_compute',
  'stale_old_live_released_while_latest_held',
  'stale_old_history_released_while_latest_held',
  'delayed_history_after_newer_live_and_prepend',
  'partial_range_merge_preserves_outside_values',
  'older_page_own_basis_return_right',
  'hidden_update_no_chart_writes_then_resume',
  'metadata_only_change_no_recompute_no_repaint',
  'older_bar_inplace_correction_recomputes',
  'warmup_multi_page_shared_requests_quiet_source',
  'warmup_exhausted_source_truthful_coverage',
  'disable_during_warmup_releases',
  'producer_serialized_fixture_through_app',
]

export async function runDataCases(t) {
  const { evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds, pushLive, STEP, FILES, producerFixture, makeBar, setDataset } = t
  const ran = new Set()
  const done = name => ran.add(name)
  const installDelay = `(() => { if (window.__delay) return; const h = __cvd.host(); const orig = h.compute.bind(h);
    window.__delay = { hold: null, calls: 0, held: 0 };
    h.compute = async (...a) => { window.__delay.calls++; const r = await orig(...a); const D = window.__delay;
      if (D.queue) { await new Promise(res => D.q.push({ r: res, op: a[3] })); return r }
      const d = D.hold; if (d) { D.held++; await d.p } return r } })()`
  const hold = `(() => { let r; const p = new Promise(x => r = x); window.__delay.hold = { p, r }; window.__delay.held = 0 })()`
  const release = `(() => { const d = window.__delay.hold; window.__delay.hold = null; d && d.r() })()`
  const chartVal = (key, time) => evaluate(`(() => { const r = __cvd.renderers().get(${JSON.stringify(key)}); const p = r && r.seriesHandle().data().find(p => p.time === ${time}); return p && p.value !== undefined ? p.value : null })()`)
  const authoritative = async () => new Map(await evaluate('__cvd.page.bars.map(b => [b.time, b.cvd_usdt])'))
  const allExact = async () => { const a = await authoritative(); const v = await valuesOf('cvd/cvd'); let bad = 0; for (const [tm, x] of a) if (!Object.is(v[tm], x)) bad++; return { bad, n: a.size } }

  await evaluate(installDelay)
  let n = await importFile(FILES.cvd); check('data_cvd_imported', n.kind === 'ok', JSON.stringify(n))
  await toggle('cvd')
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`)

  // 1. Bounded canonical window: explicit end = cache end, actual prefix.
  {
    const p = await evaluate(`__cvd.compute().runtime('cvd').lastPlan`)
    const len = await evaluate('__cvd.page.bars.length')
    const ctx = await evaluate(`(d => d.context.bars ?? 0)(__cvd.records().get('cvd').descriptor)`)
    const keep = p.trimmed_from + p.output_from
    check('window_plan_bounded_and_actual_prefix', p.input_to === len && p.output_from === Math.min(ctx, keep) && p.output_from <= ctx, JSON.stringify({ p, len, ctx }))
    done('window_plan_bounded_and_actual_prefix')
  }

  // 2. Same-time tail replace while an earlier compute result is delayed:
  //    dataRevision moves, historyRevision does not; stale value never repainted.
  {
    const last = ds().at(-1)
    const histRev0 = await evaluate('__cvd.historyRevision()')
    await evaluate(hold)
    pushLive({ ...last, cvd_usdt: last.cvd_usdt + 111, ac: last.ac + 222 })
    await waitFor(`window.__delay.held > 0`)
    const oldV = last.cvd_usdt
    pushLive({ ...last, cvd_usdt: last.cvd_usdt + 333, ac: last.ac + 444 })
    await waitFor(`__cvd.page.bars.at(-1).cvd_usdt === ${last.cvd_usdt + 333}`)
    const during = await chartVal('cvd/cvd', last.time)
    const histRev1 = await evaluate('__cvd.historyRevision()')
    await evaluate(release)
    await waitFor(settled)
    await sleep(100); await waitFor(settled)
    const after = await chartVal('cvd/cvd', last.time)
    check('tail_same_time_update_during_delayed_compute',
      histRev1 === histRev0 && during !== oldV && during !== last.cvd_usdt + 111 && Object.is(after, last.cvd_usdt + 333),
      JSON.stringify({ histRev0, histRev1, during, after, want: last.cvd_usdt + 333 }))
    done('tail_same_time_update_during_delayed_compute')
  }


  // 2b/2c. Release the OLD (pre-replace) result while the LATEST compute is
  // still held; record every LWC write + readout during that interval.
  //   historyRevision is unchanged by the same-time replacement.
  for (const variant of ['live', 'history']) {
    const name = `stale_old_${variant}_released_while_latest_held`
    await waitFor(settled); await sleep(100); await waitFor(settled)
    await evaluate(`(() => { const d = window.__delay; d.q = []; d.queue = true })()`)
    await evaluate(`(() => { const s = __cvd.renderers().get('cvd/cvd').seriesHandle(); window.__w = [];
      if (!s.__rec) { s.__rec = 1; for (const m of ['setData', 'update']) { const o = s[m].bind(s); s[m] = (...a) => { (window.__w ||= []).push([m, m === 'update' ? [a[0]] : a[0].slice(-3)]); return o(...a) } } } })()`)
    const last = ds().at(-1)
    const hr0 = await evaluate('__cvd.historyRevision()')
    const oldBad = last.cvd_usdt + (variant === 'live' ? 40000 : 0)   // formats differently from want in the readout
    if (variant === 'live') pushLive({ ...last, cvd_usdt: oldBad, ac: last.ac + 1 })
    else await evaluate(`__cvd.noteDataChanged()`)   // old History on current tail
    await waitFor(`window.__delay.q.length >= 1`, 20000)
    const hr1 = await evaluate('__cvd.historyRevision()')
    const want = last.cvd_usdt + 3333
    pushLive({ ...last, cvd_usdt: want, ac: last.ac + 2 })
    await waitFor(`__cvd.page.bars.at(-1).cvd_usdt === ${want}`)
    const hrSame = (await evaluate('__cvd.historyRevision()')) === hr1
    const staleV = variant === 'live' ? oldBad : last.cvd_usdt
    await evaluate(`(() => { window.__w = []; window.__ro = []; window.__delay.q.shift().r() })()`)
    await waitFor(`window.__delay.q.length >= 1`, 20000)   // latest compute started and is held
    await sleep(150)
    const interval = await evaluate(`({ w: window.__w, chart: (() => { const p = __cvd.renderers().get('cvd/cvd').seriesHandle().data().find(p => p.time === ${last.time}); return p && p.value !== undefined ? p.value : null })(),
      val: __cvd.values().get('cvd/cvd|source')?.get(${last.time}) ?? null, readout: [...document.querySelectorAll('[data-pane-summary]')].map(e => e.textContent).join(' | '), fmtStale: __cvd.fmtReadout('cvd/cvd', ${staleV}), fmtWant: __cvd.fmtReadout('cvd/cvd', ${want}) })`)
    const staleWrites = interval.w.filter(([m, pts]) => pts.some(p => p && p.time === last.time && Object.is(p.value, staleV))).length
    const staleReadout = !!interval.fmtStale && interval.fmtStale !== interval.fmtWant && interval.readout.includes(interval.fmtStale)
    const stalePaint = staleWrites > 0 || Object.is(interval.chart, staleV) || Object.is(interval.val, staleV) || staleReadout
    await evaluate(`(() => { const d = window.__delay; d.queue = false; for (const x of d.q.splice(0)) x.r() })()`)
    await waitFor(settled); await sleep(150); await waitFor(settled)
    const after = await chartVal('cvd/cvd', last.time)
    const readoutAfter = await evaluate(`[...document.querySelectorAll('[data-pane-summary]')].map(e => e.textContent).join(' | ')`)
    const r = { hr0, hr1, hrSame, writes: interval.w.length, staleWrites, chart: interval.chart, val: interval.val, staleV, after, want, readout: interval.readout, fmtStale: interval.fmtStale, fmtWant: interval.fmtWant, staleReadout, readoutAfter }
    console.log('WITNESS', name, JSON.stringify(r))
    check(name, (variant === 'live' ? hr1 === hr0 : hr1 === hr0 + 1) && hrSame && !stalePaint && (variant !== 'live' || interval.fmtStale !== interval.fmtWant) && Object.is(after, want) && interval.readout.length > 0 && !!interval.fmtWant && readoutAfter.includes(interval.fmtWant), JSON.stringify(r))
    done(name)
  }

  // 3. Delayed History result after a newer live tick and an older prepend
  //    (history basis change): coherent final values, bitwise authoritative.
  {
    await evaluate(hold)
    const p0 = await evaluate('__cvd.page.bars.length')
    await evaluate(`(async () => { __cvd.page.loadingOlder = false; window.__lmb = __cvd.loadMoreBefore() })()`)
    await waitFor(`__cvd.page.bars.length > ${p0} || __cvd.page.reachedStart`, 20000)
    const tm = ds().at(-1).time + STEP
    pushLive({ ...makeBar(ds().length), time: tm })
    await waitFor(`__cvd.page.bars.at(-1).time === ${tm}`)
    const staleShown = await chartVal('cvd/cvd', tm)
    await evaluate(release)
    await waitFor(settled, 30000); await sleep(150); await waitFor(settled)
    const { bad, n: total } = await allExact()
    check('delayed_history_after_newer_live_and_prepend', bad === 0 && total > p0 && (staleShown === null || Object.is(staleShown, ds().at(-1).cvd_usdt)), JSON.stringify({ bad, total, p0, staleShown }))
    done('delayed_history_after_newer_live_and_prepend')
  }

  // 4. User older page = its OWN producer request basis (own origin, actual
  //    prefix, right end before the newer page); newer pages are never
  //    re-anchored; outside values (NONEMPTY) preserved bit-for-bit in values
  //    AND chart; returning right triggers no recompute.
  {
    await waitFor(settled); await sleep(100); await waitFor(settled)
    const chartMap = () => evaluate(`Object.fromEntries(__cvd.renderers().get('cvd/cvd').seriesHandle().data().filter(p => p.value !== undefined).map(p => [p.time, p.value]))`)
    const before = await valuesOf('cvd/cvd'), chart0 = await chartMap()
    const tail0 = await evaluate(`__cvd.compute().runtime('cvd').lastPlan`)
    const oldest0 = await evaluate('__cvd.page.bars[0].time'), p0 = await evaluate('__cvd.page.bars.length')
    const nPlans0 = await evaluate(`(__cvd.compute().runtime('cvd').plans ?? []).length`)
    const ctx = await evaluate(`(d => d.context.bars ?? 0)(__cvd.records().get('cvd').descriptor)`)
    await sleep(1200)   // fetch cooldown
    await evaluate(`(async () => { __cvd.page.loadingOlder = false; await __cvd.loadMoreBefore() })()`)
    await waitFor(`__cvd.page.bars.length > ${p0}`, 20000)
    await waitFor(settled); await sleep(150); await waitFor(settled)
    const plans = (await evaluate(`__cvd.compute().runtime('cvd').plans`)).slice(nPlans0 > 15 ? 0 : nPlans0)
    const older = plans.filter(q => q.kind === 'older').at(-1)
    const tail = await evaluate(`__cvd.compute().runtime('cvd').lastPlan`)
    const bars = await evaluate('__cvd.page.bars.map(b => b.time)')
    const olderFrom = bars[0] < oldest0 ? bars.find(t => t >= (older?.range_from_time ?? Infinity)) : null
    const fromIdx = bars.indexOf(older?.range_from_time), rightIdx = bars.indexOf(oldest0)
    const after = await valuesOf('cvd/cvd'), chart1 = await chartMap()
    const outside = Object.keys(before).map(Number).filter(t => t >= oldest0)
    const outsideChart = Object.keys(chart0).map(Number).filter(t => t >= oldest0)
    const keptV = outside.filter(t => Object.is(after[t], before[t])).length
    const keptC = outsideChart.filter(t => Object.is(chart1[t], chart0[t])).length
    const olderVals = Object.keys(after).map(Number).filter(t => t < oldest0).length
    const basisOk = !!older && older.range_to_time < oldest0 && older.input_to === rightIdx && fromIdx >= 0 &&
      older.trimmed_from === Math.max(0, fromIdx - ctx) && older.output_from === fromIdx - older.trimmed_from &&
      tail.range_from_time === tail0.range_from_time
    // Return right: pan back to the live edge — no compute, no value change.
    const c0 = await evaluate('window.__delay.calls')
    await evaluate(`__cvd.chart.timeScale().scrollToRealTime()`); await sleep(400); await waitFor(settled)
    const c1 = await evaluate('window.__delay.calls')
    const afterRight = await valuesOf('cvd/cvd')
    const rightSame = outside.every(t => Object.is(afterRight[t], before[t]))
    const r = { ctx, older, tail0: tail0.range_from_time, tail: tail.range_from_time, fromIdx, rightIdx, outside: outside.length, keptV, outsideChart: outsideChart.length, keptC, olderVals, rightCalls: c1 - c0, rightSame, olderFrom }
    console.log('WITNESS older_page_own_basis_return_right', JSON.stringify(r))
    check('partial_range_merge_preserves_outside_values', outside.length > 100 && keptV === outside.length && outsideChart.length > 0 && keptC === outsideChart.length && olderVals > 0, JSON.stringify(r))
    check('older_page_own_basis_return_right', basisOk && c1 - c0 === 0 && rightSame, JSON.stringify(r))
    done('partial_range_merge_preserves_outside_values'); done('older_page_own_basis_return_right')
  }

  // 5. Hidden page: live update computes but no chart writes; resume paints.
  {
    await evaluate(`(() => { Object.defineProperty(document, 'hidden', { configurable: true, get: () => true }); document.dispatchEvent(new Event('visibilitychange')) })()`)
    const r = `__cvd.renderers().get('cvd/cvd').seriesHandle()`
    await evaluate(`(() => { const s = ${r}; window.__writes = 0; for (const m of ['setData', 'update']) { const o = s[m].bind(s); s[m] = (...a) => { window.__writes++; return o(...a) } } })()`)
    const tm = ds().at(-1).time + STEP
    const bar = { ...makeBar(ds().length), time: tm }
    pushLive(bar)
    await waitFor(`__cvd.page.bars.at(-1).time === ${tm}`); await waitFor(settled); await sleep(200)
    const hiddenWrites = await evaluate('window.__writes')
    const computedHidden = Object.is((await valuesOf('cvd/cvd'))[tm], bar.cvd_usdt)
    await evaluate(`(() => { Object.defineProperty(document, 'hidden', { configurable: true, get: () => false }); document.dispatchEvent(new Event('visibilitychange')) })()`)
    await waitFor(`(() => { const p = ${r}.data().find(p => p.time === ${tm}); return p && p.value === ${bar.cvd_usdt} })()`, 10000).catch(() => {})
    const shown = await chartVal('cvd/cvd', tm)
    check('hidden_update_no_chart_writes_then_resume', hiddenWrites === 0 && computedHidden && Object.is(shown, bar.cvd_usdt), JSON.stringify({ hiddenWrites, computedHidden, shown }))
    done('hidden_update_no_chart_writes_then_resume')
  }

  // 5b. Metadata-only authority notification (relabel path): no revision bump,
  // no compute, values and live overlay untouched.
  {
    const hr0 = await evaluate('__cvd.historyRevision()'), dr0 = await evaluate('__cvd.revision()'), c0 = await evaluate('window.__delay.calls')
    const v0 = JSON.stringify(await valuesOf('cvd/cvd'))
    await evaluate(`(() => { const s = __cvd.renderers().get('cvd/cvd').seriesHandle(); window.__mw = 0;
      if (!s.__mrec) { s.__mrec = 1; for (const m of ['setData', 'update']) { const o = s[m].bind(s); s[m] = (...a) => { window.__mw++; return o(...a) } } } })()`)
    const ro0 = await evaluate(`[...document.querySelectorAll('[data-pane-summary]')].map(e => e.textContent).join('|')`)
    await evaluate(`__cvd.noteDataChanged('meta')`)
    await sleep(200); await waitFor(settled)
    const same = JSON.stringify(await valuesOf('cvd/cvd')) === v0
    const ro1 = await evaluate(`[...document.querySelectorAll('[data-pane-summary]')].map(e => e.textContent).join('|')`)
    const r = { hr: await evaluate('__cvd.historyRevision()') - hr0, dr: await evaluate('__cvd.revision()') - dr0, calls: await evaluate('window.__delay.calls') - c0, same, writes: await evaluate('window.__mw'), readoutSame: ro0 === ro1, readoutLen: ro0.length }
    console.log('WITNESS metadata_only_change_no_recompute_no_repaint', JSON.stringify(r))
    check('metadata_only_change_no_recompute_no_repaint', r.hr === 0 && r.dr === 0 && r.calls === 0 && same && r.writes === 0 && r.readoutSame, JSON.stringify(r))
    done('metadata_only_change_no_recompute_no_repaint')
  }

  // 5c. In-place correction of an OLDER canonical bar (same array, same length)
  // is a real data change, never mistaken for metadata-only.
  {
    const old = ds().at(-5)
    const hr0 = await evaluate('__cvd.historyRevision()')
    pushLive({ ...old, cvd_usdt: old.cvd_usdt + 7777, ac: old.ac + 1 })
    await waitFor(`__cvd.page.bars.find(b => b.time === ${old.time}).cvd_usdt === ${old.cvd_usdt + 7777}`)
    await sleep(100); await waitFor(settled); await sleep(100); await waitFor(settled)
    const v = (await valuesOf('cvd/cvd'))[old.time]
    const { bad } = await allExact()
    const hr = await evaluate('__cvd.historyRevision()') - hr0
    check('older_bar_inplace_correction_recomputes', Object.is(v, old.cvd_usdt + 7777) && bad === 0 && hr >= 1, JSON.stringify({ v, bad, hr }))
    done('older_bar_inplace_correction_recomputes')
  }

  // 6–8. Warm-up through the ONE shared history path on a quiet source.
  {
    await toggle('cvd'); await waitFor(settled)
    n = await importFile(FILES.sample); check('data_sample_imported', n.kind === 'ok' || n.kind === 'info', JSON.stringify(n))
    // Fresh page so warm-up starts from one page only; quiet source (no live ticks).
    control.pageCap = 200   // context (500) spans several pages -> multi-page warm-up + cooldown continuation
    await t.reopen()
    await evaluate(installDelay)
    const h0 = control.history.length, ws0 = control.wsUpgrades
    const oldest0 = await evaluate('__cvd.page.bars[0].time')
    await toggle('sample.channel')
    await waitFor(`(__cvd.values().get('sample.channel/sma|source')?.size ?? 0) > 0 && ${settled}`, 30000)
    await waitFor(`__cvd.compute().runtime('sample.channel').coverage === 'ready' && ${settled}`, 45000).catch(() => {})
    const reqs = control.history.slice(h0).filter(q => /before=/.test(q))
    const p = await evaluate(`__cvd.compute().runtime('sample.channel').lastPlan`)
    const ctx = await evaluate(`__cvd.records().get('sample.channel').descriptor.context.bars`)
    check('warmup_multi_page_shared_requests_quiet_source',
      reqs.length >= 3 && await evaluate(`__cvd.page.bars[0].time`) < oldest0 && control.wsUpgrades - ws0 <= 1 && p.output_from === ctx && p.coverage === 'ready',
      JSON.stringify({ reqs, p, ctx, ws: control.wsUpgrades - ws0 }))
    done('warmup_multi_page_shared_requests_quiet_source')
    control.pageCap = 0

    // Exhausted (quiet): 300 bars < context 500, the server keeps saying
    // has_more_before so exhaustion is only learned from the EMPTY final page
    // (loadMoreBefore empty branch); no live ticks follow. Valid partial SMA
    // outputs must remain (not 'unavailable'); missing context is 'partial'.
    setDataset(Array.from({ length: 300 }, (_, i) => makeBar(i)))
    control.pageCap = 150; control.hideStart = true
    const hx0 = control.history.length
    await t.reopen()
    await evaluate(installDelay)
    await waitFor(`__cvd.records().get('sample.channel')?.enabled === true`)
    await waitFor(`__cvd.page.reachedStart && !__cvd.page.loadingOlder && ${settled}`, 30000).catch(() => {})
    await sleep(300); await waitFor(settled)
    control.pageCap = 0; control.hideStart = false
    const emptyFinal = control.history.slice(hx0).filter(q => /before=/.test(q)).length
    const pe = await evaluate(`__cvd.compute().runtime('sample.channel').lastPlan`)
    const st = await evaluate(`(() => { const r = __cvd.compute().runtime('sample.channel'); return { state: r.state, coverage: r.coverage } })()`)
    const vals = await valuesOf('sample.channel/sma')
    const len = await evaluate('__cvd.page.bars.length')
    const L = await evaluate(`(r => r.params?.length ?? 20)(__cvd.records().get('sample.channel'))`)
    const closes = await evaluate('__cvd.page.bars.map(b => [b.time, b.price_close])')
    let expectedFinite = 0, matched = 0, wrongNull = 0, fakeValue = 0, zeros = 0
    const reqFrom = pe?.range_from_time ?? Infinity   // requested page; warm-up pages are context only
    closes.forEach(([tm, _], i) => {
      const v = vals[tm]
      if (tm >= reqFrom && i + 1 >= L) { expectedFinite++; const want = closes.slice(i + 1 - L, i + 1).reduce((a, [, c]) => a + c, 0) / L
        if (v == null) wrongNull++; else if (Math.abs(v - want) <= 1e-9 * Math.abs(want)) matched++ ; if (v === 0) zeros++ }
      else if (v != null && Number.isFinite(v)) fakeValue++
    })
    const r = { emptyFinal, pe, st, len, L, expectedFinite, matched, wrongNull, fakeValue, zeros }
    console.log('WITNESS warmup_exhausted_source_truthful_coverage', JSON.stringify(r))
    check('warmup_exhausted_source_truthful_coverage',
      len === 300 && emptyFinal >= 2 && pe && pe.input_to === len && st.coverage === 'partial' && st.state === 'partial' &&
      expectedFinite === 150 && pe.output_from === 150 && matched === expectedFinite && wrongNull === 0 && fakeValue === 0,
      JSON.stringify(r))
    done('warmup_exhausted_source_truthful_coverage')

    // Disable during a held warm-up compute: worker released, no stale write.
    setDataset(Array.from({ length: 4000 }, (_, i) => makeBar(i)))
    await t.reopen()
    await evaluate(installDelay)
    await evaluate(hold)
    await waitFor(`window.__delay.held > 0`, 30000)
    await toggle('sample.channel')
    await evaluate(release)
    await waitFor(`!__cvd.compute().running().includes('sample.channel') && ${settled}`)
    await sleep(200)
    const stats = await evaluate('__cvd.host().stats()')
    const left = await evaluate(`__cvd.values().get('sample.channel/sma|source')?.size ?? 0`)
    check('disable_during_warmup_releases', stats.running === 0 && left === 0, JSON.stringify({ stats, left }))
    done('disable_during_warmup_releases')
  }

  // 9. Rust-producer-serialized PUBLIC synthetic history through the app with
  //    real public CVD AND Flow: computed values, visible LWC series and
  //    readout on history + live, for source and (where present) aggregate.
  if (producerFixture) {
    setDataset(producerFixture.slice(0, -1))
    await t.reopen()
    if (!(await evaluate(`__cvd.records().has('flow')`))) { n = await importFile(FILES.flow); check('data_flow_imported', n.kind === 'ok', JSON.stringify(n)) }
    for (const id of ['cvd', 'flow']) if (!(await evaluate(`__cvd.records().get('${id}').enabled`))) await toggle(id)
    await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && (__cvd.values().get('flow/delta|source')?.size ?? 0) > 0 && ${settled}`, 30000)
    pushLive(producerFixture.at(-1))   // producer-serialized live append
    const lt = producerFixture.at(-1).time
    await waitFor(`__cvd.page.bars.at(-1).time === ${lt} && ${settled}`)
    await sleep(150); await waitFor(settled)
    const byT = new Map(producerFixture.map(b => [b.time, b]))
    const cvd = await valuesOf('cvd/cvd'), delta = await valuesOf('flow/delta'), total = await valuesOf('flow/total'), bs = await valuesOf('flow/buy_sell')
    let badCvd = 0, badDelta = 0, badTotal = 0, badBs = 0, zeros = 0, nBs = 0
    for (const [tm, b] of byT) {
      if (!Object.is(cvd[tm], b.cvd_usdt)) badCvd++
      if (!Object.is(delta[tm], b.delta_usdt)) badDelta++
      if (b.delta_usdt === 0) zeros += Object.is(delta[tm], 0) ? 1 : 0
      if (!Object.is(total[tm], b.buy_volume_usdt + b.sell_volume_usdt)) badTotal++   // Flow total = buy + sell (exact)
      const v = bs[tm]; const nums = v == null ? [] : typeof v === 'object' ? Object.values(v).filter(Number.isFinite) : [v]
      if (nums.includes(b.buy_volume_usdt) && nums.includes(-b.sell_volume_usdt)) nBs++; else badBs++
    }
    const lwc = await evaluate(`(() => { const o = {}; for (const [k, r] of __cvd.renderers()) { if (!(k.startsWith('cvd/') || k.startsWith('flow/'))) continue; const hs = r.seriesHandles ? r.seriesHandles() : [r.seriesHandle()]; o[k] = hs.map(h => { const d = h ? h.data() : []; const p = d.find(p => p.time === ${lt}); return { n: d.length, last: p ? (p.value ?? null) : null } }) } return o })()`)
    const readout = await evaluate(`[...document.querySelectorAll('[data-pane-summary]')].map(e => e.textContent).join(' | ')`)
    const last = producerFixture.at(-1)
    const roFmt = await evaluate(`({ cvd: __cvd.fmtReadout('cvd/cvd', ${last.cvd_usdt}), total: __cvd.fmtReadout('flow/total', ${last.buy_volume_usdt + last.sell_volume_usdt}) })`)
    // Buy/Sell shares a display group with Delta: select it through the real
    // UI control and read the visible band (upper edge = buy).
    const bsSel = await evaluate(`(() => { const s = [...document.querySelectorAll('select')].find(s => [...s.options].some(o => /buy_sell/.test(o.value))); if (!s) return null; const prev = s.value; s.value = [...s.options].find(o => /buy_sell/.test(o.value)).value; s.dispatchEvent(new Event('change')); return prev })()`)
    await sleep(200)
    const bsLwc = await evaluate(`(() => { const r = [...__cvd.renderers()].find(([k]) => /buy_sell/.test(k))?.[1]; const d = r ? r.seriesHandle().data() : []; const p = d.find(p => p.time === ${lt}); return { n: d.filter(p => p.value !== undefined || Array.isArray(p.values)).length, last: p ? (p.value ?? p.values ?? null) : null } })()`)
    lwc['flow/buy_sell'] = [bsLwc]
    if (bsSel) await evaluate(`(() => { const s = [...document.querySelectorAll('select')].find(s => [...s.options].some(o => /buy_sell/.test(o.value))); s.value = ${JSON.stringify('__PREV__')}; })()`.replace('"__PREV__"', JSON.stringify(bsSel)) + `; (s => s && s.dispatchEvent(new Event('change')))([...document.querySelectorAll('select')].find(s => [...s.options].some(o => /buy_sell/.test(o.value))))`)
    const flat = Object.entries(lwc).flatMap(([k, hs]) => hs.map(h => [k, h]))
    const lastVals = flat.flatMap(([, h]) => Array.isArray(h.last) ? h.last : [h.last])
    const visible = flat.length >= 3 && flat.every(([, h]) => h.n === producerFixture.length) && Object.is(lwc['cvd/cvd']?.[0]?.last, last.cvd_usdt) &&
      lastVals.includes(last.delta_usdt) && lastVals.includes(last.buy_volume_usdt) && lastVals.includes(-last.sell_volume_usdt)
    // Aggregate mapping where supported: cvd in aggregate mode equals `ac`.
    let agg = 'unsupported'
    if (await evaluate(`typeof __cvd.setMode === 'function'`)) {
      await evaluate(`__cvd.setMode('aggregate')`); await waitFor(`(__cvd.values().get('cvd/cvd|aggregate')?.size ?? 0) > 0 && ${settled}`, 20000)
      const av = await valuesOf('cvd/cvd', 'aggregate'); let bad = 0; for (const [tm, b] of byT) if (!Object.is(av[tm], b.ac)) bad++
      agg = { bad }; await evaluate(`__cvd.setMode('source')`); await waitFor(settled)
    }
    const r = { rows: byT.size, badCvd, badDelta, badTotal, badBs, nBs, zeros, lwc, readout, roFmt, agg }
    console.log('WITNESS producer_serialized_fixture_through_app', JSON.stringify(r))
    check('producer_serialized_fixture_through_app', badCvd === 0 && badDelta === 0 && badTotal === 0 && badBs === 0 && nBs === byT.size && zeros === 26 && visible && roFmt.cvd && readout.includes(roFmt.cvd) && roFmt.total && readout.includes(roFmt.total) && (agg === 'unsupported' || agg.bad === 0), JSON.stringify(r))
    done('producer_serialized_fixture_through_app')
  }

  const missing = DATA_CASES.filter(c => !ran.has(c))
  check('data_case_inventory_complete', missing.length === 0, 'missing: ' + missing.join(','))
}
