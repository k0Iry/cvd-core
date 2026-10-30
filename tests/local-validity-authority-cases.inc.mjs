// Actual-app VALIDITY authority/catch-up/page-seam cases (E run6: H1/R1/P1 +
// held-interval witness). Invoked from runValidityCases. REAL: app.js shared
// cache/validity authority, Rust coordinator + host WASM, public plugins in
// Workers, LWC series and the DOM pane readout. MOCK (SYNTHETIC bars): the
// loopback HTTP/WS server. P1 serves the ACTUAL producer response bodies from
// CVD_PRODUCER_VALIDITY_PAGES (Rust postgres_router_validity_authority_and_
// page_seams through the production app_router) verbatim except that `epoch`
// is re-bound to the session's established incarnation (labelled below).
export const AUTHORITY_CASES = [
  'h1_delayed_same_epoch_invalid_after_valid_is_fenced',
  'h1_delayed_old_epoch_after_restart_cannot_poison_or_roll_back',
  'r1_buffered_old_time_repair_applies_with_quiet_recovery',
  'p1_real_producer_leading_seam_consumed',
  'p1_real_producer_trailing_seam_consumed',
  'p1_real_producer_all_invalid_cursor_advances',
  'held_interval_series_and_readout_witness',
]

export async function runAuthorityCases(t) {
  const { evaluate, waitFor, check, valuesOf, settled, sleep, control, ds, pushLive, pushRaw, nextRev, dropSockets, releaseHeld, producerPages, done } = t
  const SAMPLE = 'sample.channel/sma'
  const has = time => evaluate(`__cvd.page.bars.some(b => b.time === ${time})`)
  const seriesHas = (key, time) => evaluate(`(() => { const r = __cvd.renderers().get(${JSON.stringify(key)}); const p = r && r.seriesHandle().data().find(p => p.time === ${time}); return !!p && p.value !== undefined })()`)
  const poisoned = time => evaluate(`__cvd.poison().includes(${time})`)
  const marks = control.marks ??= new Map()
  const epoch0 = await evaluate('__cvd.epoch()')

  // H1a: request-time snapshot (invalid mark rev r1) is HELD; meanwhile the
  // producer repairs live at r2 > r1 (no poison ever reached the browser).
  {
    const bars = await evaluate('__cvd.page.bars.map(b => b.time)')
    const Th = bars[bars.length - 60]
    const r1 = nextRev()
    control.invalid = new Set([...(control.invalid ?? []), Th]); marks.set(Th, { rev: r1, valid: false })
    control.holdNext = 1
    const req = evaluate(`__cvd.fetchSkip(50, null, ${Th - 600}).then(d => d.bars.map(b => b.time))`)
    await waitFor(`true`); await sleep(200)
    control.invalid.delete(Th)
    pushLive(ds().find(b => b.time === Th))           // valid rev r2
    marks.set(Th, { rev: r1 + 1, valid: true })
    await sleep(300); await waitFor(settled)
    const va = await evaluate(`__cvd.validAuth(${Th})`)
    const held = releaseHeld()
    const got = await req
    await sleep(300); await waitFor(settled)
    const r = { Th, r1, va, held, got: got.includes(Th), base: await has(Th), poison: await poisoned(Th), pt: await seriesHas('cvd/cvd', Th) }
    console.log('WITNESS h1_same_epoch', JSON.stringify(r))
    check('h1_delayed_same_epoch_invalid_after_valid_is_fenced', held.length === 1 && va && va.rev > r1 && r.base && !r.poison && r.pt, JSON.stringify(r))
    done('h1_delayed_same_epoch_invalid_after_valid_is_fenced')
    marks.delete(Th)
  }

  // H1b: old-epoch snapshot held across a producer restart (new epoch on a
  // NEW connection). Releasing it must neither poison nor roll back.
  {
    const bars = await evaluate('__cvd.page.bars.map(b => b.time)')
    const Tx = bars[bars.length - 70]
    control.invalid.add(Tx); marks.set(Tx, { rev: 1_000_000, valid: false })
    control.holdNext = 1
    const req = evaluate(`__cvd.fetchSkip(50, null, ${Tx - 600}).then(d => d.bars.map(b => b.time), e => 'err:' + e.message)`)
    await sleep(300)
    control.invalid.delete(Tx); marks.delete(Tx)
    const newEpoch = epoch0 + 2
    control.epoch = newEpoch
    dropSockets()                                      // producer restart
    await waitFor(`document.body && __cvd.page.bars.length > 0`, 5000)
    await sleep(3600)                                  // app reconnect delay (3 s)
    pushLive(ds().at(-1))                              // first frame of the new connection
    await waitFor(`__cvd.epoch() === ${newEpoch}`, 15000)
    const held = releaseHeld()
    const got = await req
    await sleep(300); await waitFor(settled, 30000)
    const r = { Tx, held: held.length, got, epoch: await evaluate('__cvd.epoch()'), base: await has(Tx), poison: await poisoned(Tx), pt: await seriesHas('cvd/cvd', Tx) }
    console.log('WITNESS h1_old_epoch', JSON.stringify(r))
    check('h1_delayed_old_epoch_after_restart_cannot_poison_or_roll_back', r.held === 1 && r.epoch === newEpoch && r.base && !r.poison && r.pt && got === 'err:Foreign producer epoch', JSON.stringify(r))
    done('h1_delayed_old_epoch_after_restart_cannot_poison_or_roll_back')
  }

  // R1: poison Tr (rev a); during reconnect catch-up (history HELD) a valid
  // repair (rev b > a) for an old time <= closedThrough is buffered; after
  // the catch-up page (still reporting the old invalidation) the buffered
  // repair must apply, then the feed goes QUIET (no later frame).
  {
    const bars = await evaluate('__cvd.page.bars.map(b => b.time)')
    const Tr = bars[bars.length - 25]
    const a = nextRev()
    control.invalid.add(Tr); marks.set(Tr, { rev: a, valid: false })
    pushRaw({ rev: a, series: 'spot-v1', action: 'invalidate', invalid_time: Tr, epoch: control.epoch })
    await waitFor(`!__cvd.page.bars.some(b => b.time === ${Tr}) && ${settled}`)
    control.holdNext = 1
    dropSockets()
    for (let i = 0; i < 80 && !(control.held?.length); i++) await sleep(100)   // catch-up request is held
    const heldBefore = control.held?.length ?? 0
    pushLive(ds().find(b => b.time === Tr))           // buffered valid repair, rev b
    await sleep(300)
    const stillOut = !(await has(Tr))
    releaseHeld()                                      // page snapshot predates the repair
    control.invalid.delete(Tr); marks.delete(Tr)
    await sleep(500); await waitFor(settled, 30000)
    await waitFor(`__cvd.compute().runtime('sample.channel').state === 'ready' && ${settled}`, 30000)
    const r = { Tr, heldBefore, stillOut, base: await has(Tr), poison: await poisoned(Tr), cvd: await seriesHas('cvd/cvd', Tr), sample: await seriesHas(SAMPLE, Tr), cvdVal: (await valuesOf('cvd/cvd'))[Tr], want: ds().find(b => b.time === Tr).cvd_usdt }
    console.log('WITNESS r1_buffered_repair', JSON.stringify(r))
    check('r1_buffered_old_time_repair_applies_with_quiet_recovery', heldBefore === 1 && stillOut && r.base && !r.poison && r.cvd && r.sample && Object.is(r.cvdVal, r.want), JSON.stringify(r))
    done('r1_buffered_old_time_repair_applies_with_quiet_recovery')
  }

  // P1: real producer page bodies (epoch re-bound — LABEL: only the `epoch`
  // field differs from the Rust-serialized body).
  {
    const ep = await evaluate('__cvd.epoch()')
    const rebind = p => ({ ...p, epoch: ep })
    const tt = v => v.bars.map(b => b.time)
    if (!producerPages) { check('p1_real_producer_pages_present', false, 'CVD_PRODUCER_VALIDITY_PAGES missing') }
    else {
      const L = producerPages.after_leading, A = producerPages.after_all_invalid, B = producerPages.before_trailing
      const step = 60, t2 = L.coverage_from - 1
      control.script = [rebind(L)]
      let got = await evaluate(`__cvd.fetchSkip(2, null, ${t2}).then(d => d.bars.map(b => b.time))`)
      let r = { got, want: tt(L), inv: L.invalid_times, poison: await evaluate(`__cvd.poison().filter(t => ${JSON.stringify(L.invalid_times)}.includes(t))`) }
      check('p1_real_producer_leading_seam_consumed', JSON.stringify(got) === JSON.stringify(tt(L)) && L.invalid_times.length === 1 && r.poison.length === 1, JSON.stringify(r))
      done('p1_real_producer_leading_seam_consumed')
      control.script = [rebind(B)]
      got = await evaluate(`__cvd.fetchSkip(1, ${t2 + 2 * step}, null).then(d => d.bars.map(b => b.time))`)
      r = { got, want: tt(B), inv: B.invalid_times, poison: await evaluate(`__cvd.poison().filter(t => ${JSON.stringify(B.invalid_times)}.includes(t))`) }
      check('p1_real_producer_trailing_seam_consumed', JSON.stringify(got) === JSON.stringify(tt(B)) && B.invalid_times.length === 1 && r.poison.length === 1, JSON.stringify(r))
      done('p1_real_producer_trailing_seam_consumed')
      // All-invalid page with has_more_after: the next request must use the
      // producer's coverage_to (SYNTHETIC continuation body, labelled).
      const cont = { ...rebind(L), bars: [{ ...L.bars[0], time: A.coverage_to + step }], invalid_times: [], validity: [], has_more_after: false, coverage_from: A.coverage_to + 1, coverage_to: null }
      control.script = [rebind(A), cont]
      const n0 = control.history.length
      got = await evaluate(`__cvd.fetchSkip(2, null, ${A.coverage_from - 1}).then(d => d.bars.map(b => b.time))`)
      const reqs = control.history.slice(n0)
      r = { got, reqs, coverage_to: A.coverage_to }
      check('p1_real_producer_all_invalid_cursor_advances', reqs.length === 2 && reqs[1].includes(`after=${A.coverage_to}`) && got[0] === A.coverage_to + step && A.bars.length === 0 && A.has_more_after === true, JSON.stringify(r))
      done('p1_real_producer_all_invalid_cursor_advances')
    }
  }

  // Held-interval witness: hold a live compute, invalidate the tail-context
  // bucket, record EVERY series write and EVERY DOM pane-readout text during
  // the interval until settle; none may carry a value for/after the bucket.
  {
    await evaluate(`(() => { if (window.__delay) return; const h = __cvd.host(); const orig = h.compute.bind(h); window.__delay = { hold: null, held: 0 };
      h.compute = async (...a) => { const r = await orig(...a); const d = window.__delay.hold; if (d) { window.__delay.held++; await d.p } return r } })()`)
    const last = ds().at(-1)
    const v0 = (await valuesOf(SAMPLE))[last.time]
    const staleText = v0 === undefined ? null : await evaluate(`__cvd.fmtReadout(${JSON.stringify(SAMPLE)}, ${v0})`)
    await evaluate(`(() => { let r; const p = new Promise(x => r = x); window.__delay.hold = { p, r }; window.__delay.held = 0
      window.__wit = { on: false, T: null, writes: [], texts: [] }
      const gone = () => window.__wit.T != null && __cvd.poison().includes(window.__wit.T)
      for (const [key, r] of __cvd.renderers()) { if (typeof r?.seriesHandle !== "function") continue; const s = r.seriesHandle(); if (!s || s.__witWrapped) continue; s.__witWrapped = true
        const u = s.update.bind(s), d = s.setData.bind(s)
        s.update = (p, ...rest) => { if (window.__wit.on) window.__wit.writes.push([key, p.time, p.value, gone()]); return u(p, ...rest) }
        s.setData = arr => { if (window.__wit.on) { const g = gone(); for (const p of arr) window.__wit.writes.push([key, p.time, p.value, g]) } return d(arr) } }
      const els = [...document.querySelectorAll('[data-pane-summary]')]
      window.__witObs = new MutationObserver(() => { if (window.__wit.on && gone()) for (const e of els) window.__wit.texts.push(e.textContent) })
      for (const e of els) window.__witObs.observe(e, { childList: true, characterData: true, subtree: true }) })()`)
    pushLive({ ...last, cvd_usdt: last.cvd_usdt + 7, ac: last.ac + 7, price_close: last.price_close + 2 })
    await waitFor(`window.__delay.held > 0`)
    const Tw = ds().at(-4).time
    control.invalid.add(Tw)
    await evaluate(`window.__wit.T = ${Tw}; window.__wit.on = true`)
    pushRaw({ rev: nextRev(), series: 'spot-v1', action: 'invalidate', invalid_time: Tw, epoch: control.epoch })
    await waitFor(`!__cvd.page.bars.some(b => b.time === ${Tw})`)
    await evaluate(`(() => { const d = window.__delay.hold; window.__delay.hold = null; d && d.r() })()`)
    await sleep(400); await waitFor(settled, 30000)
    const w = await evaluate(`(() => { window.__wit.on = false; window.__witObs.disconnect(); for (const e of document.querySelectorAll('[data-pane-summary]')) window.__wit.texts.push(e.textContent); return window.__wit })()`)
    // Only writes made AFTER the browser accepted the invalidation (bucket poisoned) count.
    const afterInv = w.writes.filter(x => x[3])
    const badWrites = afterInv.filter(([key, time, value]) => value != null && ((key === 'cvd/cvd' && time === Tw) || (key.startsWith('sample.channel/') && time >= Tw)))
    const badTexts = staleText ? w.texts.filter(x => x.includes(staleText)) : []
    const r = { Tw, writes: w.writes.length, afterInv: afterInv.length, badWrites: badWrites.slice(0, 5), texts: w.texts.length, staleText, badTexts: badTexts.slice(0, 3), held: await evaluate('window.__delay.held') }
    console.log('WITNESS held_interval', JSON.stringify(r))
    check('held_interval_series_and_readout_witness', r.held > 0 && r.afterInv > 0 && badWrites.length === 0 && badTexts.length === 0 && w.texts.length > 0 && staleText !== null, JSON.stringify(r))
    done('held_interval_series_and_readout_witness')
    control.invalid.delete(Tw)
  }
}
