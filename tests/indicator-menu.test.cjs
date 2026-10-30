// Compact indicator multiselect: one trigger in #plugin-controls, a bounded
// popup (outside the overflow:auto toolbar) with native checkbox rows that
// are reconciled in place. Uses the synthetic descriptors in app-harness;
// real public plugin output is exercised by tests/browser-interaction.mjs.
const { test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { harness, seed, TEST_DESCRIPTORS, entry, out } = require('./app-harness.cjs')

const deepText = el => typeof el === 'string' ? el : (el.textContent || '') + (el.children ?? []).map(deepText).join('')
const trigger = h => { const t = h.run('pluginControlsEl.children[0]'); return Object.assign(t, { label: deepText(t) }) }
const rows = h => h.run('__pluginRows()')
const row = (h, id) => h.run(`__pluginRows().find(r => r.dataset.plugin === ${JSON.stringify(id)})`)

test('single trigger reports selected count; popup closed by default; rows are native checkboxes', () => {
    const h = harness()
    assert.equal(h.run('pluginControlsEl.children.length'), 1, 'one compact trigger, not a pill per plugin')
    const t = trigger(h)
    assert.equal(t.tagName, 'button')
    assert.equal(t.type, 'button')
    assert.equal(t.attributes['aria-expanded'], 'false')
    assert.equal(t.attributes['aria-controls'], 'plugin-menu')
    assert.match(t.label, /Indicators · 3/)
    assert.equal(h.run('pluginMenuEl.hidden'), true)
    assert.deepEqual(JSON.parse(JSON.stringify(rows(h).map(r => r.dataset.plugin))), ['cvd', 'flow', 'bands'])
    for (const r of rows(h)) { assert.equal(r.tagName, 'label'); assert.equal(r.children[0].type, 'checkbox') }
    assert.match(row(h, 'flow').title, /Flow 1\.0\.0/)
    assert.equal(h.run('pluginMenuEl.attributes.role'), undefined, 'no misleading menu role')
})

test('open, toggle several (stays open), count + visibility update, prefs persist, no reload', () => {
    const h = harness(); seed(h)
    h.run('globalThis.__fetches = 0; fetch = async () => { __fetches++; return { ok: true, json: async () => ({}) } }')
    trigger(h).onclick()
    assert.equal(h.run('pluginMenuEl.hidden'), false)
    assert.equal(trigger(h).attributes['aria-expanded'], 'true')
    const before = rows(h).map(r => r.children[0])
    for (const id of ['cvd', 'bands']) { const i = row(h, id).children[0]; i.checked = false; i.onchange() }
    assert.equal(h.run('pluginMenuEl.hidden'), false, 'multi-select keeps popup open')
    assert.deepEqual(rows(h).map(r => r.children[0]), before, 'rows reconciled in place, not rebuilt')
    assert.match(trigger(h).label, /Indicators · 1/)
    assert.equal(h.run('cvdSeries.options.visible'), false)
    assert.equal(h.run('localRecords.get("cvd").enabled'), false)
    assert.equal(h.run('__fetches'), 0, 'no data reload from toggling')
    assert.equal(h.writes.includes('range'), false, 'viewport untouched')
    h.run('renderPluginControls()')
    assert.equal(h.run('pluginMenuEl.hidden'), false, 'ordinary rerender keeps popup open')
    trigger(h).onclick()
    assert.equal(h.run('pluginMenuEl.hidden'), true)
})

test('Escape closes and returns focus; outside pointer closes; inside pointer does not', () => {
    const h = harness()
    trigger(h).onclick()
    h.run('__menuKeydown({ key: "Escape", preventDefault() {} })')
    assert.equal(h.run('pluginMenuEl.hidden'), true)
    assert.equal(h.run('document.activeElement'), trigger(h))
    trigger(h).onclick()
    h.run('__menuPointer({ target: __pluginRows()[0] })')
    assert.equal(h.run('pluginMenuEl.hidden'), false)
    h.run('__menuPointer({ target: chartEl })')
    assert.equal(h.run('pluginMenuEl.hidden'), true)
})

test.skip('RETIRED(server indicator transport; browser computes locally, see B-browser.md) empty, pending and failed entries are understandable and disabled', () => {
    const empty = harness({}, { descriptors: [] })
    assert.match(trigger(empty).label, /Indicators · 0/)
    assert.match(empty.run('__menuEmpty().textContent'), /No indicators available/)
    const h = harness({}, { descriptors: [...TEST_DESCRIPTORS,
        { id: 'x.pending', status: 'starting', descriptor: null },
        { id: 'x.bad', status: 'failed', descriptor: null, error: { code: 'incompatible' } }] })
    assert.equal(row(h, 'x.pending').children[0].disabled, true)
    assert.match(row(h, 'x.bad').title, /failed incompatible/)
    assert.match(trigger(h).label, /Indicators · 3/)
})

test('rows are keyed generically for arbitrary plugin ids', () => {
    const many = Array.from({ length: 30 }, (_, i) => entry({ id: `synthetic.${i}`, version: '0.0.1',
        name: `Synthetic overflow descriptor ${i} with a deliberately long label`, outputs: [out('v', 'V', 'separate', null, [], 'line')] }))
    const h = harness({}, { descriptors: many })
    assert.equal(rows(h).length, 30)
    assert.match(trigger(h).label, /Indicators · 30/)
})

// ---- review round 2: disclosure semantics, keyboard traversal, visual viewport
test('trigger is a disclosure, not a menu popup; tooltip says selected', () => {
    const h = harness()
    const t = trigger(h)
    assert.equal(t.attributes['aria-haspopup'], undefined, 'no aria-haspopup: content is a checkbox group, not a menu')
    assert.equal(t.attributes['aria-expanded'], 'false')
    assert.equal(t.attributes['aria-controls'], 'plugin-menu')
    assert.equal(h.run('__menuGroup().attributes.role'), 'group')
    assert.ok(h.run('__menuGroup().attributes["aria-labelledby"]'))
    assert.match(t.title, /3 of 3 indicators selected/)
    assert.doesNotMatch(t.title, /shown/)
})

test('Tab order follows CURRENT rendered rows; Shift+Tab from first returns to trigger; trigger Shift+Tab / empty Tab close', () => {
    const h = harness({}, { descriptors: [...TEST_DESCRIPTORS, { id: 'x.pending', status: 'starting', descriptor: null }] })
    const key = (target, shiftKey = false) => h.run(`(() => { let p = false; __menuKeydown({ key: 'Tab', shiftKey: ${shiftKey}, target: ${target}, preventDefault() { p = true } }); return p })()`)
    trigger(h).onclick()
    // Tab from trigger enters first enabled row.
    assert.equal(key('indicatorMenu.trigger'), true)
    assert.equal(h.run('document.activeElement'), row(h, 'cvd').children[0])
    // Shift+Tab from first row returns to trigger, popup stays open.
    assert.equal(key('__control("cvd")', true), true)
    assert.equal(h.run('document.activeElement'), trigger(h))
    assert.equal(h.run('pluginMenuEl.hidden'), false)
    // Tab from LAST enabled row (bands; x.pending is disabled) closes.
    key('__control("bands")')
    assert.equal(h.run('pluginMenuEl.hidden'), true)
    // Shift+Tab from trigger leaves backwards natively and closes.
    trigger(h).onclick()
    assert.equal(key('indicatorMenu.trigger', true), false, 'native Shift+Tab not prevented')
    assert.equal(h.run('pluginMenuEl.hidden'), true)
    const empty = harness({}, { descriptors: [] })
    empty.run('indicatorMenu.trigger.onclick()')
    assert.equal(empty.run(`(() => { let p = false; __menuKeydown({ key: 'Tab', target: indicatorMenu.trigger, preventDefault() { p = true } }); return p })()`), false, 'empty: Tab leaves trigger natively')
    assert.equal(empty.run('pluginMenuEl.hidden'), true, 'empty: closes on leaving')
})

test('rendered-order edges are used after reorder (not Map insertion order)', () => {
    const h = harness()
    h.run(`indicatorMenu.render([['bands','B'],['cvd','C'],['flow','F']].map(([id, name]) => ({ id, name, meta: '', title: name, checked: true, disabled: false })), 3)`)
    assert.deepEqual(JSON.parse(JSON.stringify(rows(h).map(r => r.dataset.plugin))), ['bands', 'cvd', 'flow'])
    trigger(h).onclick()
    h.run(`__menuKeydown({ key: 'Tab', target: indicatorMenu.trigger, preventDefault() {} })`)
    assert.equal(h.run('document.activeElement'), row(h, 'bands').children[0], 'first = first rendered row')
    h.run(`__menuKeydown({ key: 'Tab', target: __control('cvd'), preventDefault() {} })`)
    assert.equal(h.run('pluginMenuEl.hidden'), false, 'cvd is no longer last')
    h.run(`__menuKeydown({ key: 'Tab', target: __control('flow'), preventDefault() {} })`)
    assert.equal(h.run('pluginMenuEl.hidden'), true, 'flow is last rendered row')
})

// SYNTHETIC geometry: pure placement math with a fabricated visual viewport
// offset (pinch-zoom/pan). Actual browser coverage is in browser-interaction.mjs.
test('placement clamps to the visual viewport origin and extent (synthetic offsets)', () => {
    const h = harness()
    const place = (rect, vv, layoutH = 900) => h.run(`placeIndicatorMenu(${JSON.stringify(rect)}, ${JSON.stringify(vv)}, ${layoutH})`)
    // No offset: below the trigger.
    const a = place({ left: 100, top: 10, bottom: 40 }, { x: 0, y: 0, width: 1440, height: 900 })
    assert.equal(a.left, 100); assert.equal(a.top, 44)
    // Visual viewport panned right/down: popup stays inside it.
    const vv = { x: 600, y: 300, width: 400, height: 400 }
    const b = place({ left: 100, top: 10, bottom: 40 }, vv)
    assert.ok(b.left >= vv.x + 8 && b.left + b.width <= vv.x + vv.width - 8, JSON.stringify(b))
    assert.ok(b.top >= vv.y + 8 && b.top + b.maxHeight <= vv.y + vv.height - 8 + 1e-9, JSON.stringify(b))
    // Trigger near bottom of the visible region: opens upward within it.
    const c = place({ left: 650, top: 640, bottom: 670 }, vv)
    assert.equal(c.up, true)
    assert.ok(c.bottomPx >= 900 - (vv.y + vv.height) && c.maxHeight <= 640 - vv.y - 12, JSON.stringify(c))
})
