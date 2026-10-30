// Actual-app VALIDITY admission/withdrawal cases (E run7: parent
// PARENT-E-RUN6-ADMISSION-WINDOWS A + B). Invoked from runValidityCases after
// the authority cases (the session has already been through a producer
// restart, so validAuth holds OLD-epoch entries). REAL: app.js admission +
// shared cache, Rust coordinator + host WASM, public cvd plugin and the PUBLIC
// prefix-sum fixture (context 50, cumulative from the window origin) in
// Workers, LWC series. MOCK (SYNTHETIC bars): loopback HTTP/WS server.
// Every value compared is distinct (non-vacuous equality).
export const ADMISSION_CASES = [
  'a_old_epoch_valid_page_rejected_whole',
  'a_older_same_epoch_valid_row_cannot_overwrite_newer',
  'a_foreign_invalidation_cannot_delete_current_row',
  'a_current_epoch_invalid_beats_old_epoch_validauth',
  'b_prefix_interior_invalidation_withholds_beyond_context',
]

export async function runAdmissionCases(t) {
  const { evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds, pushLive, pushRaw, nextRev, FILES, done } = t
  const baseCvd = time => evaluate(`__cvd.page.bars.find(b => b.time === ${time})?.cvd_usdt ?? null`)
  const seriesVal = (key, time) => evaluate(`(() => { const r = __cvd.renderers().get(${JSON.stringify(key)}); const p = r && r.seriesHandle().data().find(p => p.time === ${time}); return p ? (p.value ?? null) : null })()`)
  const poisoned = time => evaluate(`__cvd.poison().includes(${time})`)
  const cur = await evaluate('__cvd.epoch()')
  const old = cur - 2
  const setRow = (time, patch) => { const d = ds(); const i = d.findIndex(b => b.time === time); d[i] = { ...d[i], ...patch }; return d[i] }
  // Repair buckets the earlier cases left poisoned (current-epoch rev>0 rows),
  // so every case below starts from a fully valid, computed cache.
  const repairAll = async () => {
    // Only buckets of THIS dataset can be repaired (P1 used producer-page
    // times outside it; those poisons stay and are outside the cache).
    const ours = (await evaluate('__cvd.poison()')).filter(tp => ds().some(x => x.time === tp))
    for (const tp of ours) { control.invalid?.delete(tp); pushLive(ds().find(x => x.time === tp)) }
    await sleep(400); await waitFor(`${JSON.stringify(ours)}.every(tp => !__cvd.poison().includes(tp)) && ${settled}`, 30000)
  }
  await repairAll()
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 100 && ${settled}`, 30000)

  // A1: an OLD-epoch page carrying an ordinary VALID row with a distinct value.
  {
    await waitFor(settled, 30000)
    const Ta = ds().at(-12).time
    const want = await baseCvd(Ta)
    const orig = ds().find(b => b.time === Ta)
    setRow(Ta, { cvd_usdt: orig.cvd_usdt + 123456.25 })
    control.epoch = old
    const got = await evaluate(`__cvd.fetchSkip(50, null, ${Ta - 600}).then(d => 'page:' + d.bars.length, e => 'err:' + e.message)`)
    control.epoch = cur
    setRow(Ta, { cvd_usdt: orig.cvd_usdt })
    await sleep(300); await waitFor(settled, 30000)
    const r = { Ta, got, want, base: await baseCvd(Ta), series: await seriesVal('cvd/cvd', Ta), epoch: await evaluate('__cvd.epoch()') }
    console.log('WITNESS a_old_epoch_valid_page', JSON.stringify(r))
    check('a_old_epoch_valid_page_rejected_whole', got === 'err:Foreign producer epoch' && Object.is(r.base, want) && Object.is(r.series, want) && r.epoch === cur, JSON.stringify(r))
    done('a_old_epoch_valid_page_rejected_whole')
  }

  // A2: newer valid rev r2 (value v2) then a delayed OLDER rev r1 (value v1).
  {
    const last = ds().at(-1)
    const r1 = nextRev(), r2 = nextRev()
    const v1 = last.cvd_usdt + 1111.5, v2 = last.cvd_usdt + 2222.75
    const b2 = setRow(last.time, { cvd_usdt: v2 })
    pushRaw({ series: 'spot-v1', action: 'append', rev: r2, bar: b2, epoch: cur })
    await sleep(300); await waitFor(settled, 30000)
    const mid = { base: await baseCvd(last.time), series: await seriesVal('cvd/cvd', last.time) }
    pushRaw({ series: 'spot-v1', action: 'append', rev: r1, bar: { ...b2, cvd_usdt: v1 }, epoch: cur })
    await sleep(400); await waitFor(settled, 30000)
    const r = { T: last.time, r1, r2, v1, v2, mid, base: await baseCvd(last.time), series: await seriesVal('cvd/cvd', last.time), va: await evaluate(`__cvd.validAuth(${last.time})`) }
    console.log('WITNESS a_older_same_epoch', JSON.stringify(r))
    check('a_older_same_epoch_valid_row_cannot_overwrite_newer', Object.is(mid.base, v2) && Object.is(mid.series, v2) && Object.is(r.base, v2) && Object.is(r.series, v2) && r.va?.rev === r2, JSON.stringify(r))
    done('a_older_same_epoch_valid_row_cannot_overwrite_newer')
  }

  // A3: a foreign-epoch invalidation against a CURRENT valid row.
  {
    const Tc = ds().at(-20).time
    const want = await baseCvd(Tc)
    pushRaw({ rev: nextRev() + 1000, series: 'spot-v1', action: 'invalidate', invalid_time: Tc, epoch: old })
    await sleep(150)
    const immediate = await baseCvd(Tc)                  // before any catch-up could refill a deleted row
    await sleep(250); await waitFor(settled, 30000)
    const r = { Tc, want, immediate, base: await baseCvd(Tc), series: await seriesVal('cvd/cvd', Tc), poison: await poisoned(Tc) }
    console.log('WITNESS a_foreign_invalidation', JSON.stringify(r))
    check('a_foreign_invalidation_cannot_delete_current_row', want !== null && Object.is(r.immediate, want) && Object.is(r.base, want) && Object.is(r.series, want) && !r.poison, JSON.stringify(r))
    done('a_foreign_invalidation_cannot_delete_current_row')
  }

  // A4: current-epoch DB invalid fact (rev 0, invalid_times) vs OLD-epoch validAuth.
  {
    const Td = await evaluate(`(() => { const ts = __cvd.page.bars.map(b => b.time); for (let i = ts.length - 1; i >= 0; i--) { const v = __cvd.validAuth(ts[i]); if (v && v.epoch !== ${cur}) return ts[i] } return null })()`)
    const before = Td == null ? null : await baseCvd(Td)
    let r = { Td, before }
    if (Td != null) {
      control.invalid.add(Td)
      const got = await evaluate(`__cvd.fetchSkip(50, null, ${Td - 600}).then(d => d.bars.some(b => b.time === ${Td}), e => 'err:' + e.message)`)
      await sleep(300); await waitFor(settled, 30000)
      r = { ...r, got, poison: await poisoned(Td), base: await baseCvd(Td), series: await seriesVal('cvd/cvd', Td) }
      control.invalid.delete(Td)
      pushLive(ds().find(b => b.time === Td))           // current-epoch rev>0 repair
      await sleep(400); await waitFor(settled, 30000)
      r.repaired = { poison: await poisoned(Td), base: await baseCvd(Td), series: await seriesVal('cvd/cvd', Td) }
    }
    console.log('WITNESS a_current_invalid_vs_old_validauth', JSON.stringify(r))
    check('a_current_epoch_invalid_beats_old_epoch_validauth', Td != null && r.got === false && r.poison && r.base === null && r.series === null &&
      !r.repaired.poison && Object.is(r.repaired.base, before) && Object.is(r.repaired.series, before), JSON.stringify(r))
    done('a_current_epoch_invalid_beats_old_epoch_validauth')
  }

  // B: PUBLIC prefix fixture (context 50, sums from the window origin). An
  // INTERIOR invalidation must withhold outputs far beyond bucket+context
  // during the held interval (before the Rust re-plan), and no stale value may
  // be written/shown at or after the bucket afterwards.
  {
    await repairAll()
    const n = await importFile(FILES.prefix1)
    await toggle('fixture.prefix')
    await waitFor(`(__cvd.values().get('fixture.prefix/sum|source')?.size ?? 0) > 0 && __cvd.compute().runtime('fixture.prefix').coverage === 'ready' && ${settled}`, 60000)
    const KEY = 'fixture.prefix/sum'
    const pre = await valuesOf(KEY)
    const ctx = await evaluate(`__cvd.records().get('fixture.prefix').descriptor.context.bars`)
    const times = await evaluate('__cvd.page.bars.map(b => b.time)')
    const Ti = times[times.length - 300]
    const beyond = times[times.length - 300 + ctx + 1]
    const preBeyond = Object.keys(pre).filter(x => Number(x) > beyond).length
    await evaluate(`(() => { if (window.__delay) return; const h = __cvd.host(); const orig = h.compute.bind(h); window.__delay = { hold: null, held: 0 };
      h.compute = async (...a) => { const r = await orig(...a); const d = window.__delay.hold; if (d) { window.__delay.held++; await d.p } return r } })()`)
    await evaluate(`(() => { let r; const p = new Promise(x => r = x); window.__delay.hold = { p, r }; window.__delay.held = 0
      window.__pw = { on: false, writes: [] }; const s = __cvd.renderers().get(${JSON.stringify(KEY)}).seriesHandle()
      if (!s.__pwWrapped) { s.__pwWrapped = true; const u = s.update.bind(s), d = s.setData.bind(s)
        s.update = p => { if (window.__pw.on) window.__pw.writes.push([p.time, p.value ?? null]); return u(p) }
        s.setData = arr => { if (window.__pw.on) for (const p of arr) window.__pw.writes.push([p.time, p.value ?? null]); return d(arr) } } })()`)
    const last = ds().at(-1)
    pushLive({ ...last, price_close: last.price_close + 3 })
    await waitFor(`window.__delay.held > 0`, 30000)
    control.invalid.add(Ti)
    await evaluate(`window.__pw.on = true`)
    pushRaw({ rev: nextRev(), series: 'spot-v1', action: 'invalidate', invalid_time: Ti, epoch: cur })
    await waitFor(`__cvd.poison().includes(${Ti})`)
    const held = await evaluate(`(() => { const col = __cvd.values().get('fixture.prefix/sum|source') ?? new Map(); const s = __cvd.renderers().get(${JSON.stringify(KEY)}).seriesHandle().data();
      const pre = ${JSON.stringify(pre)}; return { valuesAtOrAfter: [...col.keys()].filter(t => t >= ${Ti}).length, valuesBeyondCtx: [...col.keys()].filter(t => t > ${beyond}).length,
        staleSeriesBeyond: s.filter(p => p.time > ${beyond} && p.value !== undefined && Object.is(p.value, pre[p.time])).length } })()`)
    await evaluate(`(() => { const d = window.__delay.hold; window.__delay.hold = null; d && d.r() })()`)
    await sleep(500); await waitFor(settled, 30000)
    const w = await evaluate(`(() => { window.__pw.on = false; return window.__pw.writes })()`)
    const post = await valuesOf(KEY)
    const staleWrites = w.filter(([tm, v]) => tm >= Ti && v != null && Object.is(v, pre[tm])).length
    const stalePost = Object.entries(post).filter(([tm, v]) => Number(tm) >= Ti && Object.is(v, pre[tm])).length
    const prePreserved = Object.entries(pre).filter(([tm]) => Number(tm) < Ti).every(([tm, v]) => post[tm] === undefined || Object.is(post[tm], v))
    const r = { imported: n.kind, ctx, Ti, beyond, preBeyond, held, writes: w.length, staleWrites, stalePost, postAtOrAfter: Object.keys(post).filter(x => Number(x) >= Ti).length, prePreserved,
      plan: await evaluate(`__cvd.compute().runtime('fixture.prefix').lastPlan`) }
    console.log('WITNESS b_prefix_interior', JSON.stringify(r))
    check('b_prefix_interior_invalidation_withholds_beyond_context', n.kind === 'ok' && ctx === 50 && preBeyond > 100 && held.valuesAtOrAfter === 0 && held.valuesBeyondCtx === 0 && held.staleSeriesBeyond === 0 &&
      staleWrites === 0 && stalePost === 0 && prePreserved, JSON.stringify(r))
    done('b_prefix_interior_invalidation_withholds_beyond_context')
    control.invalid.delete(Ti)
  }
}
