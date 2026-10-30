// Actual-app VALIDITY cases (E V1/V2). Included by tests/local-wasm-browser.mjs
// when CVD_LOCAL_SUITE=validity. REAL: app.js, shared cache, Rust coordinator
// WASM, Rust browser host WASM, public cvd + sample plugin WASM in Workers, LWC
// series. MOCK: the loopback HTTP/WS server (synthetic bars); its invalidate
// frames are checked key-for-key against frames serialized by the ACTUAL
// producer push_publish path (CVD_PRODUCER_VALIDITY_FRAMES, Rust test
// export_producer_validity_frames). Every case name below is required.
import { AUTHORITY_CASES, runAuthorityCases } from './local-validity-authority-cases.inc.mjs'
import { ADMISSION_CASES, runAdmissionCases } from './local-validity-admission-cases.inc.mjs'
export const VALIDITY_CASES = [
  'producer_frame_shape_matches',
  'ws_context_invalid_refuses_only_spanning_window',
  'ws_invalid_removes_base_and_series_point',
  'ws_stale_older_rev_frame_cannot_resurrect',
  'ws_newer_valid_repair_restores_exact_values',
  'held_stale_result_released_after_invalidation_is_fenced',
  'live_window_invalid_withdraws_after_bucket',
  'http_invalid_times_after_reload_poisons',
  'http_repair_after_reload_restores',
  ...AUTHORITY_CASES,
  ...ADMISSION_CASES,
]

export async function runValidityCases(t) {
  const { evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds, pushLive, pushRaw, nextRev, FILES, producerFrames, reopen, consoleErrors, external } = t
  const ran = new Set()
  const done = name => ran.add(name)
  const SAMPLE = 'sample.channel/sma'
  const state = id => evaluate(`(r => r && { state: r.state, code: r.code })(__cvd.compute().runtime(${JSON.stringify(id)}))`)
  const has = time => evaluate(`__cvd.page.bars.some(b => b.time === ${time})`)
  const seriesHas = (key, time) => evaluate(`(() => { const r = __cvd.renderers().get(${JSON.stringify(key)}); const p = r && r.seriesHandle().data().find(p => p.time === ${time}); return !!p && p.value !== undefined })()`)
  const authoritativeCvdExact = async () => {
    const a = new Map(await evaluate('__cvd.page.bars.map(b => [b.time, b.cvd_usdt])'))
    const v = await valuesOf('cvd/cvd'); let bad = 0, n = 0
    for (const [tm, x] of Object.entries(v)) { n++; if (!Object.is(a.get(Number(tm)), x)) bad++ }
    return { bad, n }
  }
  // No sample output may depend on (have context spanning) bucket T.
  const sampleSpanning = (T, ctx) => evaluate(`(() => { const bars = __cvd.page.bars; const col = __cvd.values().get('${SAMPLE}|source') ?? new Map();
    let bad = 0; for (const tm of col.keys()) { const i = bars.findIndex(b => b.time === tm); if (i < 0) { bad++; continue }
      const from = bars[Math.max(0, i - ${ctx})].time; if (${T} >= from && ${T} <= tm) bad++ } return { bad, n: col.size } })()`)
  const invFrame = time => ({ rev: nextRev(), series: 'spot-v1', action: 'invalidate', invalid_time: time, epoch: 7 })

  // 0. Frame shape = actual producer serialization.
  {
    const p = producerFrames?.find(f => f.action === 'invalidate')
    const ours = invFrame(1)
    check('producer_frame_shape_matches', !!p && JSON.stringify(Object.keys(p).sort()) === JSON.stringify(Object.keys(ours).sort()) && p.series === ours.series && !('bar' in p) && Number.isInteger(p.invalid_time),
      JSON.stringify({ p, ours }))
    done('producer_frame_shape_matches')
  }

  let n = await importFile(FILES.cvd); check('v_cvd_imported', n.kind === 'ok', JSON.stringify(n))
  n = await importFile(FILES.sample); check('v_sample_imported', n.kind === 'ok', JSON.stringify(n))
  await toggle('cvd'); await toggle('sample.channel')
  await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && (__cvd.values().get('${SAMPLE}|source')?.size ?? 0) > 0 && ${settled}`, 60000)
  const ctx = await evaluate(`__cvd.records().get('sample.channel').descriptor.context.bars`)
  const len0 = await evaluate(`__cvd.records().get('sample.channel').params.length`)
  const sampleBefore = await valuesOf(SAMPLE)
  const cvdPlan = await evaluate(`__cvd.compute().runtime('cvd').lastPlan`)
  const samplePlan = await evaluate(`__cvd.compute().runtime('sample.channel').lastPlan`)
  const bars0 = await evaluate('__cvd.page.bars.map(b => b.time)')
  console.log('WITNESS setup', JSON.stringify({ ctx, len0, bars: bars0.length, cvdPlan, samplePlan, sampleN: Object.keys(sampleBefore).length }))

  // 1. Context-only invalid bucket: inside sample's actual warm-up prefix,
  //    before cvd's requested range (cvd context 0). Only sample is refused.
  const iRange = bars0.indexOf(cvdPlan.range_from_time)
  const Tctx = bars0[Math.max(samplePlan.trimmed_from + 1, iRange - 3)]
  {
    check('v_tctx_in_context_region', Tctx < cvdPlan.range_from_time && Tctx >= bars0[samplePlan.trimmed_from], JSON.stringify({ Tctx, cvdPlan, samplePlan }))
    control.invalid = new Set([Tctx])
    pushRaw(invFrame(Tctx))
    await waitFor(`!__cvd.page.bars.some(b => b.time === ${Tctx}) && ${settled}`)
    await waitFor(`${settled}`)
    const st = await state('sample.channel'), stc = await state('cvd')
    const span = await sampleSpanning(Tctx, ctx)
    const exact = await authoritativeCvdExact()
    const cvdN = Object.keys(await valuesOf('cvd/cvd')).length
    console.log('WITNESS ctx_invalid', JSON.stringify({ Tctx, st, stc, span, exact, cvdN }))
    check('ws_context_invalid_refuses_only_spanning_window', st.code === 'invalid_input_window' && st.state === 'unavailable' && span.bad === 0 && span.n > 0 && stc.code !== 'invalid_input_window' && exact.bad === 0 && cvdN > 100,
      JSON.stringify({ st, stc, span, exact, cvdN }))
    done('ws_context_invalid_refuses_only_spanning_window')
  }

  // 2. Interior bucket inside the requested range: base row + series point removed.
  const bars1 = await evaluate('__cvd.page.bars.map(b => b.time)')
  const T = bars1[bars1.length - 40]
  const origT = ds().find(b => b.time === T)
  {
    const before = { base: await has(T), cvdPt: await seriesHas('cvd/cvd', T) }
    const rev = nextRev()
    control.invalid.add(T)
    pushRaw({ rev, series: 'spot-v1', action: 'invalidate', invalid_time: T, epoch: 7 })
    await waitFor(`!__cvd.page.bars.some(b => b.time === ${T}) && ${settled}`)
    const after = { base: await has(T), cvdPt: await seriesHas('cvd/cvd', T), cvdVal: (await valuesOf('cvd/cvd'))[T], samplePt: await seriesHas(SAMPLE, T) }
    console.log('WITNESS interior_invalid', JSON.stringify({ T, before, after }))
    check('ws_invalid_removes_base_and_series_point', before.base && before.cvdPt && !after.base && !after.cvdPt && after.cvdVal === undefined && !after.samplePt, JSON.stringify({ before, after }))
    done('ws_invalid_removes_base_and_series_point')
    // 3. A frame older than the invalidation (rev lower) must not resurrect T.
    pushRaw({ rev: rev - 1, series: 'spot-v1', action: 'replace_last', bar: origT, epoch: 7 })
    await sleep(400); await waitFor(settled)
    check('ws_stale_older_rev_frame_cannot_resurrect', !(await has(T)) && !(await seriesHas('cvd/cvd', T)), 'stale frame resurrected T')
    done('ws_stale_older_rev_frame_cannot_resurrect')
  }

  // 4. Newer valid repairs (T then Tctx): exact restoration of all values.
  {
    control.invalid.delete(T); pushLive(origT)
    const origCtx = ds().find(b => b.time === Tctx)
    control.invalid.delete(Tctx); pushLive(origCtx)
    await waitFor(`__cvd.page.bars.some(b => b.time === ${T}) && __cvd.page.bars.some(b => b.time === ${Tctx}) && ${settled}`)
    await waitFor(`__cvd.compute().runtime('sample.channel').state === 'ready' && ${settled}`, 30000)
    const sampleAfter = await valuesOf(SAMPLE)
    let diff = 0, common = 0
    for (const [tm, v] of Object.entries(sampleBefore)) if (tm in sampleAfter) { common++; if (!Object.is(sampleAfter[tm], v)) diff++ }
    const exact = await authoritativeCvdExact()
    const pts = { cvd: await seriesHas('cvd/cvd', T), sample: await seriesHas(SAMPLE, T) }
    console.log('WITNESS repair', JSON.stringify({ common, diff, exact, pts, poison: await evaluate('__cvd.poison()').catch(() => 'n/a') }))
    check('ws_newer_valid_repair_restores_exact_values', diff === 0 && common > 100 && exact.bad === 0 && pts.cvd && pts.sample && (await state('sample.channel')).code === '',
      JSON.stringify({ common, diff, exact, pts }))
    done('ws_newer_valid_repair_restores_exact_values')
  }

  // 5. Held stale compute released AFTER an invalidation (dangerous interval).
  //    6. live window spanning the newest-bucket invalidation withdraws values at/after it.
  {
    await evaluate(`(() => { if (window.__delay) return; const h = __cvd.host(); const orig = h.compute.bind(h); window.__delay = { hold: null, held: 0 };
      h.compute = async (...a) => { const r = await orig(...a); const d = window.__delay.hold; if (d) { window.__delay.held++; await d.p } return r } })()`)
    await evaluate(`(() => { let r; const p = new Promise(x => r = x); window.__delay.hold = { p, r }; window.__delay.held = 0 })()`)
    const last = ds().at(-1)
    pushLive({ ...last, cvd_usdt: last.cvd_usdt + 5, ac: last.ac + 5, price_close: last.price_close + 1 })
    await waitFor(`window.__delay.held > 0`)
    const Tl = ds().at(-3).time
    control.invalid.add(Tl)
    pushRaw(invFrame(Tl))
    await waitFor(`!__cvd.page.bars.some(b => b.time === ${Tl})`)
    await evaluate(`(() => { const d = window.__delay.hold; window.__delay.hold = null; d && d.r() })()`)
    await sleep(300); await waitFor(settled, 30000)
    const cvdV = await valuesOf('cvd/cvd'), smV = await valuesOf(SAMPLE)
    const sampleAtOrAfter = Object.keys(smV).map(Number).filter(x => x >= Tl)
    const cvdAt = cvdV[Tl]
    const span = await sampleSpanning(Tl, ctx)
    const pts = { cvd: await seriesHas('cvd/cvd', Tl), sample: await seriesHas(SAMPLE, Tl), sampleTail: await seriesHas(SAMPLE, ds().at(-1).time) }
    const st = await state('sample.channel')
    console.log('WITNESS held_live_invalid', JSON.stringify({ Tl, sampleAtOrAfter, cvdAt, span, pts, st }))
    check('held_stale_result_released_after_invalidation_is_fenced', cvdAt === undefined && !pts.cvd && !pts.sample && span.bad === 0, JSON.stringify({ cvdAt, pts, span }))
    done('held_stale_result_released_after_invalidation_is_fenced')
    check('live_window_invalid_withdraws_after_bucket', sampleAtOrAfter.length === 0 && !pts.sampleTail && st.code === 'invalid_input_window', JSON.stringify({ sampleAtOrAfter, pts, st }))
    done('live_window_invalid_withdraws_after_bucket')

    // 7. Reload: the producer's /history.invalid_times alone must poison (no WS frame).
    await reopen()
    await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`, 60000)
    await sleep(500); await waitFor(settled, 60000)
    const r = { base: await has(Tl), poison: await evaluate('__cvd.poison()').catch(() => null), st: await state('sample.channel'),
      sampleAfter: Object.keys(await valuesOf(SAMPLE)).map(Number).filter(x => x >= Tl).length, span: await sampleSpanning(Tl, ctx), cvdAt: (await valuesOf('cvd/cvd'))[Tl] }
    console.log('WITNESS reload_http', JSON.stringify({ Tl, r }))
    check('http_invalid_times_after_reload_poisons', !r.base && r.st?.code === 'invalid_input_window' && r.sampleAfter === 0 && r.span.bad === 0 && r.cvdAt === undefined, JSON.stringify(r))
    done('http_invalid_times_after_reload_poisons')

    // 8. Repair after reload (HTTP-origin poison cleared by a newer valid WS frame).
    control.invalid.delete(Tl)
    pushLive(ds().find(b => b.time === Tl))
    await waitFor(`__cvd.page.bars.some(b => b.time === ${Tl}) && ${settled}`, 30000)
    await waitFor(`__cvd.compute().runtime('sample.channel').state === 'ready' && ${settled}`, 30000)
    const exact = await authoritativeCvdExact()
    const tailOk = await seriesHas(SAMPLE, ds().at(-1).time)
    console.log('WITNESS reload_repair', JSON.stringify({ exact, tailOk }))
    check('http_repair_after_reload_restores', exact.bad === 0 && exact.n > 100 && tailOk && (await seriesHas('cvd/cvd', Tl)), JSON.stringify({ exact, tailOk }))
    done('http_repair_after_reload_restores')
  }

  // E run6 authority / catch-up / page-seam / held-interval cases.
  // CVD_VALIDITY_RED_ADMISSION_ONLY=1 is a RED-control aid only (the gate never sets it).
  if (process.env.CVD_VALIDITY_RED_ADMISSION_ONLY !== '1') await runAuthorityCases({ ...t, done })
  // E run7 admission verdict / interval-withdrawal cases.
  await runAdmissionCases({ ...t, done })

  check('v_no_page_exceptions', consoleErrors.length === 0, consoleErrors.join('\n'))
  check('v_no_external_network', external.length === 0, external.join(','))
  const missing = VALIDITY_CASES.filter(c => !ran.has(c))
  check('validity_case_inventory_complete', missing.length === 0, 'missing: ' + missing.join(','))
}
