// Toggle persistence identity / durable-completion acceptance
// (PARENT-B-REV7-TOGGLE). Real app, real IndexedDB, real Workers. The only
// test-owned hook wraps the app's localDb.put to HOLD a specific write until
// released (then it runs the REAL IDB put; a "fail" release flips the real
// IDBObjectStore.put fault injection for exactly that call). The durable
// reload barrier is the app's per-operation receipt (state 'committed' =
// IDB transaction complete), never a sleep.
export const TOGGLE_CASES = [
  'toggle_disable_retry_durable_receipt_then_reload',
  'toggle_aba_old_failure_never_overrides_latest_disable',
  'toggle_during_held_replacement_latest_disable_wins',
  'toggle_pending_then_uninstall_no_resurrection',
  'params_write_snapshot_then_failed_disable_reverts_to_committed',
]

export async function runToggleCases(t) {
  const { evaluate, waitFor, check, importFile, toggle, sleep, FILES, openMenu, rowSel, reopen, click, call } = t
  const ran = new Set()
  const hook = `(() => { if (!window.__idbPatched) { window.__idbPatched = 1; window.__idbFail = {};
      for (const m of ['put', 'delete']) { const o = IDBObjectStore.prototype[m]; IDBObjectStore.prototype[m] = function (...a) { if (window.__idbFail[m]) throw new DOMException('injected ' + m + ' failure', 'QuotaExceededError'); return o.apply(this, a) } } }
    const db = __cvd.db(); if (db.__held) return; const orig = db.put.bind(db)
    const H = window.__hold = { arm: 0, started: 0, settled: 0, gates: [] }
    db.__held = 1
    db.put = rec => { H.started++; const g = H.arm > 0 ? (H.arm--, (() => { let r; const p = new Promise(x => { r = x }); const e = { p, release: r, rec: { id: rec.id, enabled: rec.enabled, version: rec.version } }; H.gates.push(e); return e })()) : null
      return (async () => { let fail = false; if (g) fail = await g.p
        if (fail) window.__idbFail.put = true
        try { return await orig(rec) } finally { if (fail) window.__idbFail.put = false } })().finally(() => { H.settled++ }) } })()`
  const install = () => evaluate(hook)
  const release = (i, fail) => evaluate(`window.__hold.gates[${i}].release(${fail})`)
  const receipts = id => evaluate(`(__cvd.toggleReceipts() || []).filter(r => r.id === ${JSON.stringify(id)}).map(r => ({ seq: r.seq, requested: r.requested, state: r.state }))`)
  // Durable barrier: every toggle receipt for id is terminal AND every put has settled.
  const durable = async id => {
    const hasReceipts = await evaluate('!!__cvd.toggleReceipts()')
    if (hasReceipts) await waitFor(`(__cvd.toggleReceipts()).filter(r => r.id === ${JSON.stringify(id)}).every(r => r.state !== 'pending')`, 15000)
    await waitFor('window.__hold.started === window.__hold.settled', 15000)
  }
  const state = id => evaluate(`({ enabled: __cvd.records().get(${JSON.stringify(id)})?.enabled, version: __cvd.records().get(${JSON.stringify(id)})?.version,
      checked: document.querySelector('${rowSel(id)} input[type=checkbox]')?.checked, running: __cvd.compute().running().includes(${JSON.stringify(id)}) })`)
  const stored = id => evaluate(`new Promise(res => { const q = indexedDB.open('cvd-local-plugins'); q.onsuccess = () => { const d = q.result; const names = [...d.objectStoreNames]; const tx = d.transaction(names[0]); const g = tx.objectStore(names[0]).get(${JSON.stringify(id)}); g.onsuccess = () => { const r = g.result; d.close(); res(r ? { enabled: r.enabled, version: r.version } : null) } } })`)
  const fresh = async () => { await reopen(); await install() }

  await install()
  let n = await importFile(FILES.flow); check('toggle_flow_imported', n.kind === 'ok', JSON.stringify(n))
  await toggle('flow'); await durable('flow')
  await waitFor(`__cvd.compute().running().includes('flow')`, 15000)

  // 1. Parent failure: failed disable reverted, retry disable, reload only after the durable receipt.
  {
    await evaluate(`window.__idbFail.put = true`)
    const nb = await evaluate('__cvd.notice().seq')
    await toggle('flow')
    await waitFor(`__cvd.notice().seq > ${nb} && __cvd.notice().kind === 'error'`)
    const n1 = await evaluate('__cvd.notice()')
    await durable('flow')
    await waitFor(`__cvd.compute().running().includes('flow')`, 10000).catch(() => {})
    const st = await state('flow')
    await evaluate(`window.__idbFail.put = false`)
    await toggle('flow')
    const sync = await state('flow')                  // immediate: stopped before any disk I/O
    const transient = await receipts('flow')          // honest: last op pending/committed
    await durable('flow')
    const rc = (await receipts('flow')).at(-1)
    const disk = await stored('flow')
    await fresh()
    const after = await state('flow')
    const r = { n1: n1.text, st, sync, transientLast: transient.at(-1), rc, disk, after }
    console.log('WITNESS toggle_disable_retry_durable_receipt_then_reload', JSON.stringify(r))
    check('toggle_disable_retry_durable_receipt_then_reload', /disable not saved.*reverted/.test(n1.text) && st.enabled && st.checked && st.running
      && sync.enabled === false && !sync.running && rc?.state === 'committed' && rc?.requested === false && disk?.enabled === false && after.enabled === false && !after.running, JSON.stringify(r))
    ran.add('toggle_disable_retry_durable_receipt_then_reload')
  }
  // re-enable durably for the ABA case
  await toggle('flow'); await durable('flow'); await waitFor(`__cvd.compute().running().includes('flow')`, 15000)

  // 2. ABA: hold first disable; enable; disable; FAIL the old op; later ops finish.
  {
    await evaluate('window.__hold.arm = 1')
    await toggle('flow')                     // op1 disable (held)
    await waitFor('window.__hold.gates.length === 1')
    await toggle('flow')                     // op2 enable (queued)
    await toggle('flow')                     // op3 disable (queued)
    const mid = await state('flow')
    await release(0, true)                   // old op fails (real IDB fault)
    await durable('flow')
    const fin = await state('flow')
    const rcs = (await receipts('flow')).slice(-3)
    const disk = await stored('flow')
    await fresh()
    const after = await state('flow')
    const r = { mid, fin, rcs, disk, after }
    console.log('WITNESS toggle_aba_old_failure_never_overrides_latest_disable', JSON.stringify(r))
    check('toggle_aba_old_failure_never_overrides_latest_disable', mid.enabled === false && !mid.running && fin.enabled === false && fin.checked === false && !fin.running
      && rcs.length === 3 && rcs[0].state === 'superseded' && rcs[1].state === 'committed' && rcs[2].state === 'committed' && rcs[2].requested === false
      && disk?.enabled === false && after.enabled === false && !after.running, JSON.stringify(r))
    ran.add('toggle_aba_old_failure_never_overrides_latest_disable')
  }

  // 3. Replacement put held; user disables meanwhile; replacement must keep the disable.
  {
    n = await importFile(FILES.prefix1); check('toggle_prefix_v1_imported', n.kind === 'ok', JSON.stringify(n))
    await toggle('fixture.prefix'); await durable('fixture.prefix')
    await waitFor(`__cvd.compute().running().includes('fixture.prefix')`, 15000)
    { // v2 accepts scale 4 (v1 default rejected by v2's param check)
      await openMenu(); await click(`${rowSel('fixture.prefix')} [data-action=params]`)
      await evaluate(`(() => { const i = document.querySelector('${rowSel('fixture.prefix')} [data-param=scale]'); i.focus(); i.select() })()`)
      await call('Input.insertText', { text: '4' })
      const b = await evaluate('__cvd.notice().seq')
      await click(`${rowSel('fixture.prefix')} [data-action=apply]`)
      await waitFor(`__cvd.notice().seq > ${b} && __cvd.notice().kind !== 'busy'`)
      const pn = await evaluate('__cvd.notice()'); check('toggle_prefix_scale4_saved', pn.kind === 'ok', JSON.stringify(pn))
    }
    await evaluate('window.__hold.arm = 1; window.__hold.gates = []')
    const before = await evaluate('__cvd.notice().seq')
    await openMenu()
    const { root } = await t.call('DOM.getDocument', { depth: -1 })
    const { nodeId } = await t.call('DOM.querySelector', { nodeId: root.nodeId, selector: '.plugin-import-input' })
    await t.call('DOM.setFileInputFiles', { nodeId, files: [FILES.prefix2] })
    await waitFor(`window.__hold.gates.length === 1 || (__cvd.notice().seq > ${before} && __cvd.notice().kind !== 'busy')`, 20000)
    if (!await evaluate('window.__hold.gates.length === 1')) throw new Error('replacement put not reached: ' + JSON.stringify(await evaluate('__cvd.notice()')))
    await toggle('fixture.prefix')                               // user disables now
    const mid = await state('fixture.prefix')
    await release(0, false)
    await waitFor(`(n => n.seq > ${before} && n.kind !== 'busy' && n.op && n.op.type === 'import')(__cvd.notice())`, 20000)
    const notice = await evaluate('__cvd.notice()')
    await durable('fixture.prefix')
    await sleep(300)
    const fin = await state('fixture.prefix')
    const rc = (await receipts('fixture.prefix')).at(-1)
    const disk = await stored('fixture.prefix')
    await fresh()
    const after = await state('fixture.prefix')
    const r = { mid, notice: notice.text, fin, rc, disk, after }
    console.log('WITNESS toggle_during_held_replacement_latest_disable_wins', JSON.stringify(r))
    check('toggle_during_held_replacement_latest_disable_wins', mid.enabled === false && !mid.running && /Replaced/.test(notice.text)
      && fin.enabled === false && fin.checked === false && !fin.running && fin.version === disk?.version && disk?.enabled === false && rc?.state === 'committed'
      && after.enabled === false && !after.running && after.version === fin.version, JSON.stringify(r))
    ran.add('toggle_during_held_replacement_latest_disable_wins')
  }

  // 5. Params put held (payload enabled=true); disable meanwhile; params COMMITS,
  // queued disable FAILS: memory/checkbox/Worker must revert to the value the
  // params write actually committed (true). Retry disable -> committed -> reload false,
  // and the stale 'not saved — reverted' notice is no longer the current outcome.
  {
    const id = 'fixture.prefix'
    await toggle(id); await durable(id)                      // enable durably
    await waitFor(`__cvd.compute().running().includes('${id}')`, 15000)
    const disk0 = await stored(id)
    await evaluate('window.__hold.arm = 2; window.__hold.gates = []')
    await openMenu(); await click(`${rowSel(id)} [data-action=params]`)
    await evaluate(`(() => { const i = document.querySelector('${rowSel(id)} [data-param=scale]'); i.focus(); i.select() })()`)
    await call('Input.insertText', { text: '6' })   // v2 requires even scale
    await click(`${rowSel(id)} [data-action=apply]`)
    await waitFor('window.__hold.gates.length === 1')
    const paramsPayload = await evaluate('window.__hold.gates[0].rec')
    await toggle(id)                                        // disable: off now, put queued
    const mid = await state(id)
    const nb = await evaluate('__cvd.notice().seq')
    await release(0, false)                                 // params commits (enabled=true)
    await waitFor('window.__hold.gates.length === 2', 15000)
    const togglePayload = await evaluate('window.__hold.gates[1].rec')
    await release(1, true)                                  // disable FAILS (real IDB fault)
    await durable(id)
    await waitFor(`__cvd.notice().seq > ${nb} && __cvd.notice().kind === 'error'`, 15000)
    const n1 = await evaluate('__cvd.notice()')
    await waitFor(`__cvd.compute().running().includes('${id}')`, 10000).catch(() => {})
    const fin = await state(id)
    const disk1 = await stored(id)
    const rc1 = (await receipts(id)).at(-1)
    // retry disable; wait for ITS committed receipt, then reload
    await toggle(id)
    const rcRetrySeq = (await receipts(id)).at(-1).seq
    await durable(id)
    const rc2 = (await receipts(id)).find(r => r.seq === rcRetrySeq)
    const n2 = await evaluate('__cvd.notice()')
    const disk2 = await stored(id)
    await fresh()
    const after = await state(id)
    const r = { disk0, paramsPayload, mid, togglePayload, n1: n1.text, fin, disk1, rc1, rc2, n2: { kind: n2.kind, text: n2.text, op: n2.op }, disk2, after }
    console.log('WITNESS params_write_snapshot_then_failed_disable_reverts_to_committed', JSON.stringify(r))
    check('params_write_snapshot_then_failed_disable_reverts_to_committed',
      disk0?.enabled === true && paramsPayload.enabled === true && mid.enabled === false && !mid.running && togglePayload.enabled === false
      && /disable not saved.*reverted/.test(n1.text) && rc1?.state === 'failed'
      && fin.enabled === true && fin.checked === true && fin.running && disk1?.enabled === true
      && rc2?.state === 'committed' && rc2?.requested === false && n2.kind === 'ok' && /disabled \(saved\)/.test(n2.text) && n2.op?.type === 'toggle' && n2.op?.id === id
      && disk2?.enabled === false && after.enabled === false && !after.running, JSON.stringify(r))
    ran.add('params_write_snapshot_then_failed_disable_reverts_to_committed')
  }

  // 4. Toggle held, uninstall queued behind it, then old toggle completes: no resurrection.
  {
    await evaluate('window.__hold.arm = 1; window.__hold.gates = []')
    await toggle('fixture.prefix')            // enable, held
    await waitFor('window.__hold.gates.length === 1')
    await openMenu(); await t.call('Runtime.evaluate', { expression: `document.querySelector('${rowSel('fixture.prefix')} [data-action=remove]').click()` })
    await release(0, true)                    // held toggle fails
    await waitFor(`!__cvd.records().has('fixture.prefix') && /Removed/.test(__cvd.notice().text)`, 15000)
    await durable('fixture.prefix')
    const disk = await stored('fixture.prefix')
    await fresh()
    const after = await evaluate(`({ has: __cvd.records().has('fixture.prefix'), row: !!document.querySelector('${rowSel('fixture.prefix')}'), running: __cvd.compute().running().includes('fixture.prefix') })`)
    const r = { disk, after }
    console.log('WITNESS toggle_pending_then_uninstall_no_resurrection', JSON.stringify(r))
    check('toggle_pending_then_uninstall_no_resurrection', disk === null && !after.has && !after.row && !after.running, JSON.stringify(r))
    ran.add('toggle_pending_then_uninstall_no_resurrection')
  }
  for (const c of TOGGLE_CASES) if (!ran.has(c)) check(c, false, 'case did not run')
}
