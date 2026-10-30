// Remaining B acceptance (PARENT-B-REV6-P2): nonzero-context older-page /
// shared warm-up witness, registry transactions with injected IndexedDB
// failures, delayed mode-switch fencing, menu keyboard/geometry and
// host-unavailable. Included by tests/local-wasm-browser.mjs with
// CVD_LOCAL_SUITE=p2. Real public WASM (CVD, Flow, and the PUBLIC generic
// prefix-sum fixture tests/fixtures/wasm/prefix-plugin, context 50) in real
// Workers; trusted CDP mouse/keyboard input. Every case below is required.
export const P2_CASES = [
  'prefix_initial_tail_plan_nonzero_ctx_exact',
  'trusted_left_pan_older_page_shared_warmup_quiet',
  'older_page_values_exact_own_origin_unaffected_bit_identical',
  'three_plugins_share_requests_no_duplicate_feeds',
  'return_right_no_recompute',
  'older_bookkeeping_bounded_to_current_basis',
  'params_change_after_older_pages_recomputes_whole_history',
  'replace_params_rejects_migrated_user_params_keeps_previous',
  'replace_params_rejection_survives_reload',
  'replacement_accepted_recomputes_whole_history',
  'idb_put_failure_params_truthful_retry_reload',
  'idb_put_failure_disable_truthful_retry_reload',
  'idb_put_failure_import_truthful_retry_reload',
  'idb_delete_failure_uninstall_truthful_retry_reload',
  'delayed_mode_switch_result_fenced',
  'menu_keyboard_reordered_rows_tab_order',
  'menu_all_disabled_keyboard_escape_outside',
  'menu_many_rows_scroll_bounds_synthetic',
  'menu_empty_keyboard',
  'params_panel_short_narrow_bounds',
  'menu_light_dark_screenshots',
  'host_unavailable_price_feed_usable_actionable',
]

export async function runP2Cases(t) {
  const { evaluate, waitFor, check, importFile, toggle, valuesOf, settled, sleep, control, ds, pushLive, STEP, FILES, makeBar, call, click, key, openMenu, rowSel, shot, setDataset } = t
  const ran = new Set()
  const done = name => ran.add(name)
  const CTX = 50
  const installCount = `(() => { if (window.__cnt) return; const h = __cvd.host(); const orig = h.compute.bind(h);
    window.__cnt = { calls: 0, hold: null, held: 0 };
    h.compute = async (...a) => { window.__cnt.calls++; const r = await orig(...a); const d = window.__cnt.hold; if (d) { window.__cnt.held++; await d.p } return r } })()`
  const quiet = async () => { await waitFor(settled, 30000); await sleep(400); await waitFor(settled, 30000); await waitFor('!__cvd.page.loadingOlder', 20000); await sleep(400); await waitFor(settled, 30000) }
  const seqSum = (closes, from, to) => { let a = 0; for (let i = from; i <= to; i++) a += closes[i]; return a }
  // Exact expected prefix values: value(t) = scale * seq-sum(close[origin..t]), origin = idx(rangeFrom) - CTX.
  async function verifyPrefix(scale, ranges) {
    const bars = await evaluate('__cvd.page.bars.map(b => [b.time, b.price_close])')
    const times = bars.map(b => b[0]), closes = bars.map(b => b[1])
    const vals = await valuesOf('fixture.prefix/sum')
    let n = 0, bad = 0, missing = 0, badOrigin = 0
    const covered = new Set()
    for (const { from, to } of ranges) {
      const i0 = times.indexOf(from), i1 = to == null ? times.length - 1 : times.lastIndexOf(times.filter(x => x <= to).at(-1))
      if (i0 - CTX < 0) { badOrigin++; continue }
      let acc = seqSum(closes, i0 - CTX, i0 - 1)
      for (let i = i0; i <= i1; i++) {
        acc += closes[i]; n++; covered.add(times[i])
        const v = vals[times[i]]
        if (v == null) missing++; else if (!Object.is(v, scale * acc)) bad++
      }
    }
    const extra = Object.keys(vals).map(Number).filter(x => !covered.has(x)).length
    return { n, bad, missing, badOrigin, extra }
  }

  // ---------------------------------------------------------------- paging
  control.pageCap = 600   // MORE earlier history stays available (4000 rows total)
  await t.reopen()
  await evaluate(installCount)
  let n = await importFile(FILES.prefix1); check('p2_prefix_v1_imported', n.kind === 'ok', JSON.stringify(n))
  n = await importFile(FILES.cvd); check('p2_cvd_imported', n.kind === 'ok', JSON.stringify(n))
  n = await importFile(FILES.flow); check('p2_flow_imported', n.kind === 'ok', JSON.stringify(n))
  const ids = ['fixture.prefix', 'cvd', 'flow']
  const h0 = control.history.length, ws0 = control.wsUpgrades
  for (const id of ids) { await toggle(id); await waitFor(`__cvd.records().get('${id}').enabled === true`) }
  await waitFor(`(__cvd.values().get('fixture.prefix/sum|source')?.size ?? 0) > 0`, 30000)
  await waitFor(`__cvd.compute().runtime('fixture.prefix').coverage === 'ready'`, 30000)
  await quiet()
  const reqFrom = await evaluate(`__cvd.compute().runtime('fixture.prefix').lastPlan.range_from_time`)
  {
    const p = await evaluate(`__cvd.compute().runtime('fixture.prefix').lastPlan`)
    const times = await evaluate('__cvd.page.bars.map(b => b.time)')
    const ri = times.indexOf(reqFrom)
    const v = await verifyPrefix(1, [{ from: reqFrom }])
    const warm = control.history.slice(h0).filter(q => /before=/.test(q)).length
    const r = { p, ri, v, warm, len: times.length }
    console.log('WITNESS prefix_initial_tail_plan_nonzero_ctx_exact', JSON.stringify(r))
    check('prefix_initial_tail_plan_nonzero_ctx_exact', p.output_from === CTX && p.trimmed_from === ri - CTX && p.input_to === times.length && warm >= 1 && v.n > 400 && v.bad === 0 && v.missing === 0 && v.extra === 0, JSON.stringify(r))
    done('prefix_initial_tail_plan_nonzero_ctx_exact')
  }
  // Snapshot unaffected (newer) results of all three plugins.
  const snap = async () => ({ prefix: await valuesOf('fixture.prefix/sum'), cvd: await valuesOf('cvd/cvd'), delta: await valuesOf('flow/delta') })
  const before = await snap()
  const oldest0 = await evaluate('__cvd.page.bars[0].time')
  const hPan = control.history.length, wsPan = control.wsUpgrades
  const plansN0 = await evaluate(`(__cvd.compute().runtime('fixture.prefix').plans ?? []).length`)
  // Trusted user left-pan: real mouse drags on the chart canvas (quiet source).
  const box = await evaluate(`(() => { const c = [...document.querySelectorAll('canvas')].sort((a, b) => b.width * b.height - a.width * a.height)[0].getBoundingClientRect(); return { x: c.left, y: c.top, w: c.width, h: c.height } })()`)
  let drags = 0
  for (; drags < 60 && (await evaluate('__cvd.olderPages().length')) < 1; drags++) {
    const y = box.y + box.h * 0.4, x0 = box.x + box.w * 0.2, x1 = box.x + box.w * 0.9
    await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: x0, y })
    await call('Input.dispatchMouseEvent', { type: 'mousePressed', x: x0, y, button: 'left', buttons: 1, clickCount: 1 })
    for (let k = 1; k <= 8; k++) await call('Input.dispatchMouseEvent', { type: 'mouseMoved', x: x0 + (x1 - x0) * k / 8, y, button: 'left', buttons: 1 })
    await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x: x1, y, button: 'left', buttons: 0, clickCount: 1 })
    await sleep(120)
  }
  await waitFor('__cvd.olderPages().length >= 1', 20000)
  // Stop panning; the shared warm-up for the older page's missing prefix must
  // complete WITHOUT any live tick (quiet source).
  await waitFor(`['fixture.prefix','cvd','flow'].every(id => __cvd.compute().runtime(id).coverage === 'ready')`, 30000).catch(() => {})
  await quiet()
  const pages = await evaluate('__cvd.olderPages()')
  const plans = (await evaluate(`__cvd.compute().runtime('fixture.prefix').plans`))
  const olderPlans = plans.filter(q => q.kind === 'older')
  const beforeReqs = control.history.slice(hPan).filter(q => /before=/.test(q))
  {
    // Warm-up witness: an older-page plan first lacked its prefix (output_from < CTX,
    // non-ready), then a NON-user page was prepended and it was recomputed with CTX.
    const partialFirst = olderPlans.some(q => q.output_from < CTX && q.coverage !== 'ready')
    const last = olderPlans.at(-1)
    const times = await evaluate('__cvd.page.bars.map(b => b.time)')
    const pg = pages.at(-1)
    const fi = times.indexOf(pg.from), ti = times.indexOf(pg.to)
    const warmupPages = beforeReqs.length - pages.length
    const cov = await evaluate(`['fixture.prefix','cvd','flow'].map(id => __cvd.compute().runtime(id).coverage)`)
    const r = { drags, pages, beforeReqs: beforeReqs.length, warmupPages, partialFirst, last, fi, ti, cov, oldestNow: times[0], oldest0, history: control.history.length - hPan, ws: control.wsUpgrades - wsPan }
    console.log('WITNESS trusted_left_pan_older_page_shared_warmup_quiet', JSON.stringify(r))
    check('trusted_left_pan_older_page_shared_warmup_quiet', pages.length >= 1 && partialFirst && warmupPages >= 1 && last && last.output_from === CTX &&
      last.trimmed_from === fi - CTX && last.input_to === ti + 1 && last.range_from_time === pg.from && last.range_to_time === pg.to && pg.to < oldest0 && cov.every(c => c === 'ready'), JSON.stringify(r))
    done('trusted_left_pan_older_page_shared_warmup_quiet')
  }
  {
    const v = await verifyPrefix(1, [...pages.map(p => ({ from: p.from, to: p.to })), { from: reqFrom }])
    const after = await snap()
    const same = (a, b) => { const ks = Object.keys(a).map(Number).filter(x => x >= oldest0); return { n: ks.length, kept: ks.filter(k => Object.is(a[k], b[k])).length } }
    const s = { prefix: same(before.prefix, after.prefix), cvd: same(before.cvd, after.cvd), delta: same(before.delta, after.delta) }
    const r = { v, s }
    console.log('WITNESS older_page_values_exact_own_origin_unaffected_bit_identical', JSON.stringify(r))
    check('older_page_values_exact_own_origin_unaffected_bit_identical', v.n > 500 && v.bad === 0 && v.missing === 0 && v.badOrigin === 0 && v.extra === 0 &&
      Object.values(s).every(x => x.n > 400 && x.kept === x.n), JSON.stringify(r))
    done('older_page_values_exact_own_origin_unaffected_bit_identical')
  }
  {
    const all = control.history.slice(h0)
    const uniq = new Set(all).size
    const r = { requests: all.length, uniq, ws: control.wsUpgrades - ws0, beforeReqs: beforeReqs.length, pages: pages.length }
    console.log('WITNESS three_plugins_share_requests_no_duplicate_feeds', JSON.stringify(r))
    check('three_plugins_share_requests_no_duplicate_feeds', uniq === all.length && control.wsUpgrades - ws0 === 0 && beforeReqs.length <= pages.length + 2, JSON.stringify(r))
    done('three_plugins_share_requests_no_duplicate_feeds')
  }
  {
    const c0 = await evaluate('window.__cnt.calls'); const v0 = JSON.stringify(await snap())
    await evaluate('__cvd.chart.timeScale().scrollToRealTime()'); await sleep(600); await waitFor(settled)
    const r = { calls: await evaluate('window.__cnt.calls') - c0, same: JSON.stringify(await snap()) === v0 }
    check('return_right_no_recompute', r.calls === 0 && r.same, JSON.stringify(r))
    done('return_right_no_recompute')
  }
  {
    const r = await evaluate(`(() => { const hr = __cvd.historyRevision(); const n = __cvd.olderPages().length; return ['fixture.prefix','cvd','flow'].map(id => { const rt = __cvd.compute().runtime(id); const ks = [...(rt.olderDone ?? [])]; return { id, size: ks.length, cov: rt.olderCov ? rt.olderCov.size : 0, n, allCurrent: ks.every(k => k.split('|').at(-4) === String(hr)) } }) })()`)
    check('older_bookkeeping_bounded_to_current_basis', r.every(x => x.size <= x.n && x.cov <= x.n && x.allCurrent && x.size >= 1), JSON.stringify(r))
    done('older_bookkeeping_bounded_to_current_basis')
  }

  // ---------------------------------------------------------------- params / replacement
  const ranges = [...pages.map(p => ({ from: p.from, to: p.to })), { from: reqFrom }]
  async function setParam(id, value) {
    await openMenu()
    if (!(await evaluate(`!!document.querySelector('${rowSel(id)} [data-param=scale]') && !document.querySelector('${rowSel(id)} [data-param=scale]').closest('[hidden]')`))) await click(`${rowSel(id)} [data-action=params]`)
    await evaluate(`(() => { const i = document.querySelector('${rowSel(id)} [data-param=scale]'); i.focus(); i.select() })()`)
    await call('Input.insertText', { text: String(value) })
    const before = await evaluate('__cvd.notice().seq')
    await click(`${rowSel(id)} [data-action=apply]`)
    await waitFor(`__cvd.notice().seq > ${before} && __cvd.notice().kind !== 'busy'`)
    return evaluate('__cvd.notice()')
  }
  {
    const c0 = await evaluate('window.__cnt.calls')
    n = await setParam('fixture.prefix', 3)
    await waitFor(`__cvd.compute().runtime('fixture.prefix').coverage === 'ready'`, 30000); await quiet()
    const v = await verifyPrefix(3, ranges)
    const olderAfter = (await evaluate(`__cvd.compute().runtime('fixture.prefix').plans`)).filter(q => q.kind === 'older').length
    const r = { n, v, calls: await evaluate('window.__cnt.calls') - c0, olderAfter, params: await evaluate(`__cvd.records().get('fixture.prefix').params`) }
    console.log('WITNESS params_change_after_older_pages_recomputes_whole_history', JSON.stringify(r))
    check('params_change_after_older_pages_recomputes_whole_history', n.kind === 'ok' && r.params.scale === 3 && v.n > 500 && v.bad === 0 && v.missing === 0 && v.extra === 0 && olderAfter >= 1, JSON.stringify(r))
    done('params_change_after_older_pages_recomputes_whole_history')
  }
  {
    const rec0 = await evaluate(`(r => ({ sha: r.sha256, v: r.version, p: r.params, size: r.size }))(__cvd.records().get('fixture.prefix'))`)
    const vals0 = JSON.stringify(await valuesOf('fixture.prefix/sum'))
    n = await importFile(FILES.prefix2)
    await sleep(300); await waitFor(settled)
    const rec1 = await evaluate(`(r => ({ sha: r.sha256, v: r.version, p: r.params, size: r.size }))(__cvd.records().get('fixture.prefix'))`)
    const running = await evaluate(`__cvd.compute().running().includes('fixture.prefix')`)
    const r = { n, rec0, rec1, same: JSON.stringify(await valuesOf('fixture.prefix/sum')) === vals0, running }
    console.log('WITNESS replace_params_rejects_migrated_user_params_keeps_previous', JSON.stringify(r))
    check('replace_params_rejects_migrated_user_params_keeps_previous', n.kind === 'error' && /rejects your current parameters/.test(n.text) && JSON.stringify(rec0) === JSON.stringify(rec1) && rec1.v === '1.0.0' && rec1.p.scale === 3 && r.same && running, JSON.stringify(r))
    done('replace_params_rejects_migrated_user_params_keeps_previous')
    await t.reopen(); await evaluate(installCount)
    await waitFor(`__cvd.compute().runtime('fixture.prefix')?.coverage === 'ready'`, 30000).catch(() => {}); await quiet()
    const rec2 = await evaluate(`(r => ({ sha: r.sha256, v: r.version, p: r.params, size: r.size, enabled: r.enabled }))(__cvd.records().get('fixture.prefix'))`)
    const reqFrom2 = await evaluate(`__cvd.compute().runtime('fixture.prefix').lastPlan.range_from_time`)
    const v = await verifyPrefix(3, [{ from: reqFrom2 }])
    check('replace_params_rejection_survives_reload', rec2.sha === rec0.sha && rec2.v === '1.0.0' && rec2.p.scale === 3 && rec2.enabled && v.n > 400 && v.bad === 0 && v.missing === 0, JSON.stringify({ rec2, v }))
    done('replace_params_rejection_survives_reload')
  }
  {
    // Load an older page again on this fresh page, then accept a replacement:
    // the WHOLE requested history (tail + older page) is the new generation.
    await sleep(400)
    await evaluate(`(async () => { __cvd.page.loadingOlder = false; await __cvd.loadMoreBefore() })()`)
    await waitFor('__cvd.olderPages().length >= 1', 20000)
    await waitFor(`__cvd.compute().runtime('fixture.prefix').coverage === 'ready'`, 30000).catch(() => {}); await quiet()
    const pg2 = await evaluate('__cvd.olderPages()'), rf = await evaluate(`__cvd.compute().runtime('fixture.prefix').lastPlan.range_from_time`)
    const rg = [...pg2.map(p => ({ from: p.from, to: p.to })), { from: rf }]
    const pre = await verifyPrefix(3, rg)
    n = await setParam('fixture.prefix', 4)
    await waitFor(settled)
    n = await importFile(FILES.prefix2)
    await waitFor(`__cvd.records().get('fixture.prefix').version === '2.0.0' && __cvd.compute().runtime('fixture.prefix')?.coverage === 'ready'`, 30000).catch(() => {}); await quiet()
    const v = await verifyPrefix(4, rg)
    const rec = await evaluate(`(r => ({ v: r.version, p: r.params }))(__cvd.records().get('fixture.prefix'))`)
    const r = { pre, n, v, rec, pages: pg2.length }
    console.log('WITNESS replacement_accepted_recomputes_whole_history', JSON.stringify(r))
    check('replacement_accepted_recomputes_whole_history', pre.bad === 0 && pre.missing === 0 && n.kind === 'ok' && /Replaced/.test(n.text) && rec.v === '2.0.0' && rec.p.scale === 4 && v.n > 500 && v.bad === 0 && v.missing === 0 && v.extra === 0, JSON.stringify(r))
    done('replacement_accepted_recomputes_whole_history')
  }

  // ---------------------------------------------------------------- injected IndexedDB failures
  const failOn = `(() => { if (window.__idbPatched) return; window.__idbPatched = 1; window.__idbFail = {};
    for (const m of ['put', 'delete']) { const o = IDBObjectStore.prototype[m]; IDBObjectStore.prototype[m] = function (...a) { if (window.__idbFail[m]) throw new DOMException('injected ' + m + ' failure', 'QuotaExceededError'); return o.apply(this, a) } } })()`
  const setFail = (m, on) => evaluate(failOn + `; window.__idbFail.${m} = ${on}`)
  {
    await evaluate(failOn)
    await setFail('put', true)
    const vals0 = JSON.stringify(await valuesOf('fixture.prefix/sum'))
    n = await setParam('fixture.prefix', 8)
    await sleep(300); await waitFor(settled)
    const mem = await evaluate(`__cvd.records().get('fixture.prefix').params.scale`)
    const same = JSON.stringify(await valuesOf('fixture.prefix/sum')) === vals0
    await setFail('put', false)
    const n2 = await setParam('fixture.prefix', 8)
    await waitFor(`__cvd.compute().runtime('fixture.prefix').coverage === 'ready'`, 30000).catch(() => {}); await quiet()
    await t.reopen(); await evaluate(failOn)
    const stored = await evaluate(`__cvd.records().get('fixture.prefix').params.scale`)
    const r = { n, mem, same, n2, stored }
    console.log('WITNESS idb_put_failure_params_truthful_retry_reload', JSON.stringify(r))
    check('idb_put_failure_params_truthful_retry_reload', n.kind === 'error' && /not saved/.test(n.text) && /storage/.test(n.text) && mem === 4 && same && n2.kind === 'ok' && stored === 8, JSON.stringify(r))
    done('idb_put_failure_params_truthful_retry_reload')
  }
  {
    await setFail('put', true)
    const nb = await evaluate('__cvd.notice().seq')
    await toggle('flow')
    await waitFor(`__cvd.notice().seq > ${nb} && __cvd.notice().kind === 'error'`)
    const n1 = await evaluate('__cvd.notice()')
    await waitFor(`__cvd.compute().running().includes('flow')`, 10000).catch(() => {})
    const st = await evaluate(`({ enabled: __cvd.records().get('flow').enabled, checked: document.querySelector('${rowSel('flow')} input[type=checkbox]').checked, running: __cvd.compute().running().includes('flow') })`)
    await setFail('put', false)
    await toggle('flow')
    await waitFor(`__cvd.records().get('flow').enabled === false && !__cvd.compute().running().includes('flow')`)
    // Durable barrier: this retry's own receipt reaches 'committed' (IDB tx complete) — not a sleep.
    await waitFor(`(r => r && r.requested === false && r.state === 'committed')(__cvd.toggleReceipts().filter(r => r.id === 'flow').at(-1))`, 15000)
    await t.reopen(); await evaluate(failOn)
    const after = await evaluate(`__cvd.records().get('flow').enabled`)
    const r = { n1, st, after }
    check('idb_put_failure_disable_truthful_retry_reload', /disable not saved.*reverted/.test(n1.text) && st.enabled && st.checked && st.running && after === false, JSON.stringify(r))
    done('idb_put_failure_disable_truthful_retry_reload')
  }
  {
    await setFail('put', true)
    n = await importFile(FILES.stateful)
    const row = await evaluate(`!!__cvd.records().get('fixture.stateful') || !!document.querySelector('${rowSel('fixture.stateful')}')`)
    await t.reopen(); await evaluate(failOn)
    const afterReload = await evaluate(`__cvd.records().has('fixture.stateful')`)
    await setFail('put', false)
    const n2 = await importFile(FILES.stateful)
    await t.reopen(); await evaluate(failOn)
    const final = await evaluate(`__cvd.records().has('fixture.stateful') && !!document.querySelector('${rowSel('fixture.stateful')}')`)
    const r = { n, row, afterReload, n2, final }
    check('idb_put_failure_import_truthful_retry_reload', n.kind === 'error' && /storage/.test(n.text) && !row && !afterReload && n2.kind === 'ok' && final, JSON.stringify(r))
    done('idb_put_failure_import_truthful_retry_reload')
  }
  {
    await setFail('delete', true)
    await openMenu(); await click(`${rowSel('fixture.stateful')} [data-action=remove]`)
    await waitFor(`__cvd.notice().kind === 'error' && /removal not saved/.test(__cvd.notice().text)`)
    const kept = await evaluate(`__cvd.records().has('fixture.stateful') && !!document.querySelector('${rowSel('fixture.stateful')}')`)
    await t.reopen(); await evaluate(failOn)
    const keptReload = await evaluate(`__cvd.records().has('fixture.stateful')`)
    await setFail('delete', false)
    await openMenu(); await click(`${rowSel('fixture.stateful')} [data-action=remove]`)
    await waitFor(`!__cvd.records().has('fixture.stateful') && /Removed/.test(__cvd.notice().text)`)
    await t.reopen()
    const gone = await evaluate(`!__cvd.records().has('fixture.stateful') && !document.querySelector('${rowSel('fixture.stateful')}')`)
    check('idb_delete_failure_uninstall_truthful_retry_reload', kept && keptReload && gone, JSON.stringify({ kept, keptReload, gone }))
    done('idb_delete_failure_uninstall_truthful_retry_reload')
  }

  // ---------------------------------------------------------------- delayed mode switch
  {
    await evaluate(installCount)
    await waitFor(settled, 30000)
    if (!(await evaluate(`__cvd.records().get('cvd').enabled`))) await toggle('cvd')
    await waitFor(`(__cvd.values().get('cvd/cvd|source')?.size ?? 0) > 0 && ${settled}`, 30000)
    await evaluate(`(() => { let r; const p = new Promise(x => r = x); window.__cnt.hold = { p, r }; window.__cnt.held = 0 })()`)
    const last = ds().at(-1)
    pushLive({ ...last, cvd_usdt: last.cvd_usdt + 5, ac: last.ac + 9 })
    await waitFor('window.__cnt.held > 0', 10000)
    await evaluate(`__cvd.setMode('aggregate')`)
    const srcDuring = await evaluate(`__cvd.values().has('cvd/cvd|source')`)
    await evaluate(`(() => { const d = window.__cnt.hold; window.__cnt.hold = null; d && d.r() })()`)
    await waitFor(`(__cvd.values().get('cvd/cvd|aggregate')?.size ?? 0) > 0`, 30000); await quiet()
    const src = await evaluate(`__cvd.values().has('cvd/cvd|source')`)
    const bars = await evaluate('__cvd.page.bars.map(b => [b.time, b.ac])'), av = await valuesOf('cvd/cvd', 'aggregate')
    const rf = await evaluate(`__cvd.compute().runtime('cvd').lastPlan.range_from_time`)
    let bad = 0, nIn = 0; for (const [tm, ac] of bars) if (tm >= rf) { nIn++; if (!Object.is(av[tm], ac)) bad++ } else if (av[tm] !== undefined) bad++
    const chartLast = await evaluate(`(() => { const d = __cvd.renderers().get('cvd/cvd').seriesHandle().data(); const p = d.find(p => p.time === ${last.time}); return p ? p.value : null })()`)
    await evaluate(`__cvd.setMode('source')`); await quiet()
    const r = { srcDuring, src, bad, nIn, n: bars.length, chartLast, want: last.ac + 9 }
    console.log('WITNESS delayed_mode_switch_result_fenced', JSON.stringify(r))
    check('delayed_mode_switch_result_fenced', !srcDuring && !src && bad === 0 && nIn >= 600 && Object.is(chartLast, last.ac + 9), JSON.stringify(r))
    done('delayed_mode_switch_result_fenced')
  }

  // ---------------------------------------------------------------- menu UI
  const focused = () => evaluate(`(() => { const a = document.activeElement; const row = a && a.closest && a.closest('.plugin-menu-row'); return { tag: a && a.tagName, plugin: row ? row.dataset.plugin : null, action: a && a.dataset ? a.dataset.action ?? null : null, cls: a && a.className } })()`)
  const tab = (shift = false) => call('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9, modifiers: shift ? 8 : 0 }).then(() => call('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9, modifiers: shift ? 8 : 0 }))
  const closeMenu = async () => { if (!(await evaluate(`document.getElementById('plugin-menu').hidden`))) { await key('Escape', 'Escape', 27); await waitFor(`document.getElementById('plugin-menu').hidden`) } }
  {
    // Reordered rows: reorder the record map (import order), rerender; Tab
    // follows the CURRENT DOM order of row checkboxes.
    await closeMenu()
    await evaluate(`(() => { const m = __cvd.records(); const e = [...m.entries()].reverse(); m.clear(); for (const [k, v] of e) m.set(k, v); __cvd.registryFromRecords(); __cvd.renderIndicatorMenu() })()`)
    await openMenu()
    const domOrder = await evaluate(`[...document.querySelectorAll('.plugin-menu-row')].map(r => r.dataset.plugin)`)
    await call('Runtime.evaluate', { expression: `document.querySelector('.plugin-menu-trigger').focus()` })
    const seen = []
    for (let i = 0; i < 40; i++) { await tab(); const f = await focused(); if (f.plugin && !f.action && !seen.includes(f.plugin)) seen.push(f.plugin); if (f.cls && /plugin-import-btn/.test(f.cls)) break }
    await tab(true)
    const back = await focused()
    await key('Escape', 'Escape', 27)
    const esc = await evaluate(`document.getElementById('plugin-menu').hidden && document.activeElement === document.querySelector('.plugin-menu-trigger')`)
    const r = { domOrder, seen, back, esc }
    console.log('WITNESS menu_keyboard_reordered_rows_tab_order', JSON.stringify(r))
    check('menu_keyboard_reordered_rows_tab_order', domOrder.length >= 3 && JSON.stringify(seen) === JSON.stringify(domOrder) && esc, JSON.stringify(r))
    done('menu_keyboard_reordered_rows_tab_order')
  }
  {
    for (const id of await evaluate(`[...__cvd.records().values()].filter(r => r.enabled).map(r => r.id)`)) { await toggle(id); await waitFor(`__cvd.records().get('${id}').enabled === false`) }
    await waitFor(`__cvd.compute().running().length === 0 && __cvd.host().stats().running === 0`, 15000)
    await openMenu()
    const trig = await evaluate(`document.querySelector('.plugin-menu-trigger').textContent`)
    // Keyboard: Space on a focused row checkbox enables it (trusted key), Space again disables.
    await call('Runtime.evaluate', { expression: `document.querySelector('.plugin-menu-trigger').focus()` })
    await tab()
    const f = await focused()
    await key(' ', 'Space', 32)
    await waitFor(`__cvd.records().get(${JSON.stringify(f.plugin)})?.enabled === true`, 10000).catch(() => {})
    const on = await evaluate(`__cvd.records().get(${JSON.stringify(f.plugin)})?.enabled`)
    await key(' ', 'Space', 32)
    await waitFor(`__cvd.records().get(${JSON.stringify(f.plugin)})?.enabled === false`, 10000).catch(() => {})
    const off = await evaluate(`__cvd.records().get(${JSON.stringify(f.plugin)})?.enabled === false`)
    // Outside click closes.
    await call('Input.dispatchMouseEvent', { type: 'mousePressed', x: 700, y: 600, button: 'left', buttons: 1, clickCount: 1 })
    await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x: 700, y: 600, button: 'left', buttons: 0, clickCount: 1 })
    const outside = await evaluate(`document.getElementById('plugin-menu').hidden`)
    const r = { trig, f, on, off, outside, host: await evaluate('__cvd.host().stats()') }
    check('menu_all_disabled_keyboard_escape_outside', /· 0/.test(trig) && !!f.plugin && on === true && off && outside && r.host.running === 0, JSON.stringify(r))
    done('menu_all_disabled_keyboard_escape_outside')
  }
  {
    // UI-only layout fixture: 30 SYNTHETIC descriptor rows (copies of an
    // existing record's descriptor under new ids, never enabled, no Worker).
    // They are honestly labelled synthetic — not independent WASM imports.
    await evaluate(`(() => { const m = __cvd.records(); const base = m.get('cvd'); for (let i = 0; i < 30; i++) { const id = 'synthetic.row' + String(i).padStart(2, '0'); m.set(id, { ...base, id, name: 'Synthetic row ' + i, enabled: false, descriptor: { ...base.descriptor, id, name: 'Synthetic row ' + i } }) } __cvd.registryFromRecords(); __cvd.renderIndicatorMenu() })()`)
    await openMenu()
    const g = await evaluate(`(() => { const p = document.getElementById('plugin-menu'); const b = p.getBoundingClientRect(); return { rows: p.querySelectorAll('.plugin-menu-row').length, top: b.top, bottom: b.bottom, left: b.left, right: b.right, vw: innerWidth, vh: innerHeight, scroller: [p, ...p.querySelectorAll('*')].some(e => e.scrollHeight > e.clientHeight + 4 && /auto|scroll/.test(getComputedStyle(e).overflowY)) } })()`)
    const lastSel = rowSel('synthetic.row29') + ' input[type=checkbox]'
    await call('Runtime.evaluate', { expression: `document.querySelector('${lastSel}').focus()` })
    const visible = await evaluate(`(() => { const p = document.getElementById('plugin-menu').getBoundingClientRect(); const e = document.querySelector('${lastSel}').getBoundingClientRect(); return e.top >= p.top - 1 && e.bottom <= p.bottom + 1 })()`)
    await shot('p2-menu-many-rows-synthetic')
    await closeMenu()
    const running = await evaluate(`__cvd.compute().running().filter(id => id.startsWith('synthetic.')).length`)
    await evaluate(`(() => { const m = __cvd.records(); for (const k of [...m.keys()]) if (k.startsWith('synthetic.')) m.delete(k); __cvd.registryFromRecords(); __cvd.renderIndicatorMenu() })()`)
    const r = { g, visible, running }
    console.log('WITNESS menu_many_rows_scroll_bounds_synthetic', JSON.stringify(r))
    check('menu_many_rows_scroll_bounds_synthetic', g.rows >= 33 && g.top >= 0 && g.bottom <= g.vh && g.left >= 0 && g.right <= g.vw && g.scroller && visible && running === 0, JSON.stringify(r))
    done('menu_many_rows_scroll_bounds_synthetic')
  }
  {
    await call('Emulation.setDeviceMetricsOverride', { width: 360, height: 420, deviceScaleFactor: 1, mobile: false })
    await sleep(300)
    await openMenu()
    await click(`${rowSel('fixture.prefix')} [data-action=params]`)
    await sleep(150)
    const g = await evaluate(`(() => { const p = document.getElementById('plugin-menu'); const b = p.getBoundingClientRect(); const i = document.querySelector('${rowSel('fixture.prefix')} [data-param=scale]'); i.scrollIntoView({ block: 'nearest' }); const ib = i.getBoundingClientRect(); const ap = document.querySelector('${rowSel('fixture.prefix')} [data-action=apply]').getBoundingClientRect(); return { top: b.top, bottom: b.bottom, left: b.left, right: b.right, vw: innerWidth, vh: innerHeight, inTop: ib.top, inBottom: ib.bottom, apRight: ap.right, apBottom: ap.bottom } })()`)
    await shot('p2-params-short-narrow')
    const ok = g.top >= 0 && g.bottom <= g.vh && g.left >= 0 && g.right <= g.vw && g.inTop >= g.top - 1 && g.inBottom <= g.bottom + 1 && g.apRight <= g.vw
    for (const scheme of ['light', 'dark']) {
      await call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme }] })
      await sleep(300); await shot(`p2-params-short-narrow-${scheme}`)
    }
    const bg = await evaluate(`getComputedStyle(document.getElementById('plugin-menu')).backgroundColor`)
    await call('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'light' }] }); await sleep(200)
    const bgLight = await evaluate(`getComputedStyle(document.getElementById('plugin-menu')).backgroundColor`)
    await closeMenu()
    await call('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
    check('params_panel_short_narrow_bounds', ok, JSON.stringify(g))
    check('menu_light_dark_screenshots', bg !== bgLight, JSON.stringify({ bg, bgLight }))
    done('params_panel_short_narrow_bounds'); done('menu_light_dark_screenshots')
  }
  {
    // Empty registry keyboard: remove every record (real Remove clicks).
    for (const id of await evaluate(`[...__cvd.records().keys()]`)) {
      await openMenu(); await click(`${rowSel(id)} [data-action=remove]`)
      await waitFor(`!__cvd.records().has(${JSON.stringify(id)})`)
    }
    await closeMenu(); await openMenu()
    await call('Runtime.evaluate', { expression: `document.querySelector('.plugin-menu-trigger').focus()` })
    await tab()
    const f = await focused()
    await key('Escape', 'Escape', 27)
    const r = { f, rows: await evaluate(`document.querySelectorAll('.plugin-menu-row').length`), empty: await evaluate(`!!document.querySelector('[data-empty]')`), closed: await evaluate(`document.getElementById('plugin-menu').hidden && document.activeElement === document.querySelector('.plugin-menu-trigger')`) }
    check('menu_empty_keyboard', r.rows === 0 && r.empty && /plugin-import-btn/.test(f.cls ?? '') && r.closed, JSON.stringify(r))
    done('menu_empty_keyboard')
  }

  // ---------------------------------------------------------------- host unavailable
  {
    control.hostMissing = true
    const hu0 = control.history.length
    await call('Page.navigate', { url: (await evaluate('location.href')) })
    await waitFor(`!!globalThis.__cvd && __cvd.page.bars.length > 0`, 20000)
    await sleep(800)
    const tm = ds().at(-1).time + STEP
    pushLive({ ...makeBar(ds().length), time: tm })
    await waitFor(`__cvd.page.bars.at(-1).time === ${tm}`, 10000).catch(() => {})
    const appended = await evaluate(`__cvd.page.bars.at(-1).time === ${tm}`)
    const n1 = await importFile(FILES.prefix1)
    const state = await evaluate(`__cvd.pluginState()`)
    const bogus = await evaluate(`[...__cvd.renderers().keys()].length`)
    await openMenu(); await shot('p2-host-unavailable')
    const busy = await evaluate(`__cvd.notice().kind`)
    control.hostMissing = false
    const r = { appended, n1, state, renderers: bogus, busy, hist: control.history.length - hu0 }
    console.log('WITNESS host_unavailable_price_feed_usable_actionable', JSON.stringify(r))
    check('host_unavailable_price_feed_usable_actionable', appended && n1.kind === 'error' && /reload the page to retry/.test(n1.text) && /unsupported/.test(n1.text) && /Indicators unavailable/.test(state ?? '') && bogus === 0 && busy !== 'busy', JSON.stringify(r))
    done('host_unavailable_price_feed_usable_actionable')
    await t.reopen()
  }
  control.pageCap = 0

  const missing = P2_CASES.filter(c => !ran.has(c))
  check('p2_case_inventory_complete', missing.length === 0 && ran.size === P2_CASES.length, 'missing: ' + missing.join(','))
  check('p2_no_external_network', t.external.length === 0, t.external.join(','))
  check('p2_no_page_exceptions', t.consoleErrors.length === 0, t.consoleErrors.join('\n'))
}
