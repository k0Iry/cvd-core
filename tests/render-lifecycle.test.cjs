const { test } = require('node:test')
const assert = require('node:assert/strict')
const { harness, bar, rawBar, ind, seed, flowRe } = require('./app-harness.cjs')

test('long history renders a bounded window and shifts it without losing cache coordinates', () => {
    const h = harness()
    h.run('modeConnectionsReady = true')
    const bars = Array.from({ length: 16000 }, (_, index) => bar((index + 1) * 60))
    h.run(`setBars(${JSON.stringify(bars)}, {from:7000,to:7020})`)
    h.flushFrames()
    assert.equal(h.run('page.bars.length'), 16000)
    assert.equal(h.series[0].data.length, 3200)
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":7000,"to":7020}')
    const start = h.run('renderedWindow.start')
    h.writes.length = 0
    // Chart logical indices are global cache indices (stable timestamp backbone).
    h.emit('chart', 'range', { from: start + 1500, to: start + 1520 })
    h.flushFrames()
    assert.equal(h.writes.includes('setData'), false, 'dragging inside the buffer does not redraw data')
    h.run('page.reachedStart = true; page.reachedEnd = true')
    h.emit('chart', 'range', { from: start + 100, to: start + 120 })
    h.flushFrames()
    assert.notEqual(h.run('renderedWindow.start'), start)
    assert.equal(h.series[0].data.length, 3200)
    assert.equal(h.run('currentChartRange().from'), start + 100)
    assert.equal(h.run('currentChartRange().to'), start + 120)
})

test('historical windows ignore live tail chart writes and retain complete new data', () => {
    const h = harness()
    h.run('modeConnectionsReady = true')
    h.run(`setBars(${JSON.stringify(Array.from({ length: 16000 }, (_, index) => bar((index + 1) * 60)))}, {from:7000,to:7020})`)
    h.flushFrames(); h.writes.length = 0
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(960060, 150))}})`)
    assert.equal(h.run('page.bars.length'), 16001)
    assert.equal(h.run('page.newestTime'), 960060)
    // Only the timestamp backbone extends (one incremental update); no price/
    // CVD/Flow writes and no resubmission while viewing history.
    assert.deepEqual([...h.writes], ['update'])
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":7000,"to":7020}')
    h.run('setBars(page.bars, {from:15970,to:16000})')
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":15970,"to":16000}', 'explicit range still moves the viewport')
    assert.equal(h.series[0].data.at(-1).close, 150)
    assert.equal(h.series[0].data.length, 3200)
})

test('long-history prepend preserves the viewport moved during the request without resubmitting unchanged window data', async () => {
    const h = harness()
    const bars = Array.from({ length: 16000 }, (_, index) => bar((index + 10000) * 60))
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify(bars)}, {from:7000,to:7020})`)
    h.flushFrames()
    h.run('let historyResponse; fetch = () => new Promise(resolve => { historyResponse = resolve })')
    const loading = h.run('loadMoreBefore()')
    h.run('chart.timeScale().setVisibleLogicalRange(chartRangeForCache({from:7100,to:7120}))')
    const priceData = h.series[0].data
    h.writes.length = 0
    h.run(`historyResponse({ok:true,json:async()=>({bars:${JSON.stringify(Array.from({ length: 400 }, (_, index) => bar((index + 9600) * 60)))},has_more_before:true,has_more_after:false,realtime_enabled:true})})`)
    await loading; h.flushFrames()
    assert.equal(h.run('page.bars.length'), 16400)
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":7500,"to":7520}')
    assert.equal(h.series[0].data, priceData, 'new history outside the window does not reset series data')
    // Prepend changes membership: exactly one backbone resubmission, no window data.
    assert.equal(h.writes.filter(value => value === 'setData').length, 1)
    assert.equal(h.run('deltaZeroSeries').data.length, 16400)
    assert.equal(h.run('page.bars[Math.floor(currentChartRange().from)].time'), bars[7100].time)
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.run(`setBars([${JSON.stringify(bar(9599 * 60))}, ...page.bars], shiftRange(currentChartRange(), 1))`)
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(26000 * 60))}})`)
    h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    assert.equal(h.run('currentChartRange().from'), 7501)
    assert.equal(h.run('page.bars[Math.floor(currentChartRange().from)].time'), bars[7100].time)
    assert.equal(h.series[0].data.length, 3200)
})

test('prepend shifts a queued window transition instead of restoring the stale pre-prepend index', () => {
    const h = harness()
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify(Array.from({ length: 16000 }, (_, index) => bar((index + 10000) * 60)))}, {from:7000,to:7020})`)
    h.flushFrames()
    const start = h.run('renderedWindow.start')
    h.run('page.reachedStart = true; page.reachedEnd = true')
    h.emit('chart', 'range', { from: start + 100, to: start + 120 })
    assert.equal(h.run('pendingChartRange.from'), start + 100)
    const before = h.run('page.bars[Math.floor(currentChartRange().from)].time')
    h.run(`setBars([${JSON.stringify(bar(9999 * 60))}, ...page.bars], shiftRange(currentChartRange(), 1))`)
    h.flushFrames()
    assert.equal(h.run('currentChartRange().from'), start + 101)
    assert.equal(h.run('page.bars[Math.floor(currentChartRange().from)].time'), before)
    assert.equal(h.run('pendingChartRange'), null)
})

test('live windows grow within the buffer and compact in batches while retaining the latest bar', () => {
    const h = harness()
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify(Array.from({ length: 3200 }, (_, index) => bar((index + 1) * 60)))}, {from:3170,to:3199})`)
    h.flushFrames(); h.writes.length = 0
    h.run(`applyTailBars(${JSON.stringify(Array.from({ length: 100 }, (_, index) => bar((index + 3201) * 60)))})`)
    h.flushFrames()
    assert.equal(h.series[0].data.length, 3300)
    assert.equal(h.writes.filter(value => value === 'setData').length, 0, 'base series keep incremental updates while LEVELS work remains throttled')
    h.run('chart.timeScale().setVisibleLogicalRange(chartRangeForCache({from:3270,to:3299}))')
    h.run(`applyTailBars(${JSON.stringify(Array.from({ length: 500 }, (_, index) => bar((index + 3301) * 60)))})`)
    h.flushFrames()
    assert.equal(h.series[0].data.length, 3200)
    assert.equal(h.series[0].data.at(-1).time, 3800 * 60)
    assert.equal(h.run('page.bars.length'), 3800)
    assert.equal(h.run('currentChartRange().from'), 3270)
})

test('window boundaries retain prior-close candle color and wider zoom coverage', () => {
    const h = harness()
    const bars = Array.from({ length: 16000 }, (_, index) => bar((index + 1) * 60, 100 + index))
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify(bars)}, {from:7000,to:7020})`)
    h.flushFrames()
    assert.equal(h.series[0].data[0].color, h.run('PRICE_UP_COLOR'))
    assert.equal(h.run('renderedWindow.bars.length'), 3200)
    h.run('setBars(page.bars, {from:4000,to:10000})')
    h.flushFrames()
    assert.equal(h.series[0].data.length, 7025)
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":4000,"to":10000}')
})

test('LEVELS reuses curves and skips unchanged refreshes, including price-only changes', () => {
    const h = harness(); seed(h)
    const count = h.series.length
    h.run('refreshSegmentedNow()')
    assert.equal(h.series.length, count)
    assert.deepEqual(h.writes, [])
    h.run('page.bars[1].price_close = 105; refreshSegmentedNow()')
    assert.deepEqual(h.writes, [])
    h.run(`setLevelsData(${JSON.stringify([bar(30), bar(60), bar(120)])})`)
    assert.equal(h.series.length, count, 'prepend retains all six native LEVELS series')
    assert.equal(h.writes.filter(value => value === 'setData').length, 6)
    assert.equal(h.writes.includes('removeSeries'), false)
    assert.equal(h.run('levelsRenderedSeries.every(entry => entry.data.length === 3 && entry.data[0].time === 30)'), true)
})

test('LEVELS preserves gaps, aggregate values, styles and last-segment markers while pooling curves', () => {
    const h = harness(); seed(h)
    const bars = [60, 120, 180, 240, 300, 360].map(time => bar(time))
    bars[2] = ind({ ...rawBar(180), lt: [null, null, null, null], lb: [null, null, null, null] })
    h.run(`setLevelsData(${JSON.stringify(bars)})`)
    assert.equal(h.run('levelsRenderedSeries.length'), 12)
    assert.equal(h.run('levelsRenderedSeries.every((entry, index) => entry.marker === (index % 2 === 1))'), true)
    assert.equal(h.run('levelsRenderedSeries.every(entry => entry.data.every(point => point.time !== 180))'), true)
    // Per-layer widths come from the descriptor (SDK OutputSpec.widths).
    assert.equal(h.run('JSON.stringify(levelsRenderedSeries.map(entry => entry.lineWidth))'), '[2,2,2,2,4,4,4,4,6,6,6,6]')
    h.writes.length = 0
    const aggregate = [rawBar(60), rawBar(120)].map(item => ind({ ...item, alt: [null, 210, 220, 230], alb: [null, 190, 180, 170] }))
    h.run(`aggToggle.checked = true; setLevelsData(${JSON.stringify(aggregate)})`)
    assert.equal(h.run('levelsRenderedSeries.length'), 6)
    assert.equal(h.writes.filter(value => value === 'removeSeries').length, 6)
    assert.equal(h.run('levelsRenderedSeries.every(entry => entry.marker && entry.data.length === 2)'), true)
    assert.equal(h.run('JSON.stringify(levelsRenderedSeries.map(entry => entry.data[0].value))'), '[210,190,220,180,230,170]')
    h.run('levelsToggle.checked = false; levelsToggle.onchange(); setLevelsData(page.bars)')
    assert.equal(h.run('levelsRenderedSeries.length'), 0)
})

test('history merge retains only same-value observation; other incoming (non-observed) fields win', () => {
    const h = harness()
    const fields = ['po','ph','pl','price_close','cvd_usdt','delta_usdt','apo','aph','apl','apc','ac','ad','bvb','abvb','svb','asvb','buy_volume_usdt','agb','sell_volume_usdt','ags','trades']
    const observed = {time:60, quality:{observed_at_ms:100000,status_observed_at_ms:null,coverage:'unverified',sources:[{exchange:'binance',market:'spot'}]}, note:[1], extra:[2]}
    fields.forEach((key, i) => { observed[key] = i + 1 })
    h.run(`setBars(${JSON.stringify([observed])})`)
    const incoming = {...observed, note:[3], extra:[null]}; delete incoming.quality
    const read = bar => h.run(`JSON.stringify(mergeHistoryBars([${JSON.stringify(bar)}])[0])`)
    const retained = JSON.parse(read(incoming))
    assert.equal(retained.quality.retained, true)
    assert.deepEqual(retained.note, [3])
    assert.deepEqual(retained.extra, [null])
    assert.equal(JSON.parse(read({...incoming, agb:42})).quality, undefined)
    assert.equal(JSON.parse(read({...incoming, quality:null})).quality, null)
    assert.equal(JSON.parse(h.run(`JSON.stringify(mergeHistoryBars([${JSON.stringify({...incoming, time:120})}]).at(-1))`)).quality, undefined)
})

test('history tail batch updates existing base series and preserves visible and deferred ranges', () => {
    const h = harness(); seed(h)
    const priceData = h.series[0].data
    const cvdData = h.series[1].data
    h.run(`setMergedBars(mergeHistoryBars(${JSON.stringify([bar(120, 101), bar(180, 102), bar(240, 103)])}), {from:3,to:9})`)
    assert.equal(h.series[0].data, priceData, 'price tail does not replace the full data array')
    assert.equal(h.series[1].data, cvdData, 'CVD tail does not replace the full data array')
    assert.equal(priceData.length, 4)
    assert.equal(priceData.at(-1).close, 103)
    assert.equal(h.run('JSON.stringify(chart.timeScale().getVisibleLogicalRange())'), '{"from":3,"to":9}')
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.writes.length = 0
    h.run(`setMergedBars(mergeHistoryBars(${JSON.stringify([bar(300, 104), bar(360, 105)])}), {from:4,to:10})`)
    assert.equal(h.writes.length, 0, 'hidden batch defers all chart writes')
    assert.equal(h.run('page.newestTime'), 360)
    h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    assert.equal(h.series[0].data.length, 6)
    assert.equal(h.series[0].data.at(-1).close, 105)
    assert.equal(h.run('JSON.stringify(chart.timeScale().getVisibleLogicalRange())'), '{"from":4,"to":10}')
})

test('hidden history reconciliation updates state/title without chart writes or queued frames', () => {
    const h = harness(); seed(h)
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.writes.length = 0
    for (let i = 0; i < 20; i++) {
        h.run(`setBars(${JSON.stringify([bar(60), bar(120, 101 + i)])}, {from:0,to:20})`)
    }
    assert.equal(h.run('page.bars.at(-1).price_close'), 120)
    assert.match(h.document.title, /120\.00/)
    assert.equal(h.writes.length, 0, 'background REST must not rebuild chart series')
    assert.equal(h.frames.size, 0, 'background REST must not accumulate RAF callbacks')
})

test('hidden realtime ticks update data/title but defer every chart write', () => {
    const h = harness(); seed(h)
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.writes.length = 0
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(180, 125))}})`)
    assert.equal(h.run('page.newestTime'), 180)
    assert.match(h.document.title, /125\.00/)
    assert.match(h.run('flowSummaryEl.textContent'), flowRe('6', '5', '1'))
    assert.equal(h.writes.length, 0)
    assert.equal(h.frames.size, 0)
})

test('visibility/focus/pageshow coalesce deferred history into one full render and preserve range', () => {
    const h = harness(); seed(h)
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.run(`setBars(${JSON.stringify([bar(60), bar(120), bar(180)])}, {from:3,to:9})`)
    h.writes.length = 0
    h.document.hidden = false
    h.emit('document', 'visibilitychange')
    h.emit('window', 'focus')
    h.emit('window', 'pageshow', { persisted: true })
    h.flushFrames()
    assert.equal(h.writes.filter(value => value === 'setData').length, 10,
        'price/CVD/Delta/zero + six continuous LEVELS segments rendered once; hidden Buy-Sell stays dirty')
    assert.equal(h.run('JSON.stringify(chart.timeScale().getVisibleLogicalRange())'), '{"from":3,"to":9}')
    assert.equal(h.frames.size, 0)
})

test('hiding cancels already queued LEVELS work instead of replaying it on return', () => {
    const h = harness(); seed(h)
    h.run('lastSegmentedRefreshAt = 0; scheduleSegmentedRefresh()')
    assert.ok(h.frames.size > 0)
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.writes.length = 0; h.flushFrames()
    assert.equal(h.writes.length, 0)
    assert.equal(h.frames.size, 0)
})

test('visible live ticks retain incremental updates after initial paint', () => {
    const h = harness(); seed(h)
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(180, 125))}})`)
    assert.ok(h.writes.includes('update'))
    assert.equal(h.writes.filter(value => value === 'update').length, 4,
        'visible price/CVD/Delta/zero update incrementally; hidden Buy-Sell does not')
    assert.equal(h.writes.includes('setData'), false)
})

test('mode selection toggles existing series without data reload or chart rebuild', () => {
    const h = harness(); seed(h)
    h.run('flowModeSel.value = "flow/buy_sell"; flowModeSel.onchange()')
    assert.equal(h.run('flowModeSel.value'), 'flow/buy_sell')
    assert.equal(h.writes.filter(value => value === 'applyOptions').length, 2)
    assert.equal(h.writes.filter(value => value === 'setData').length, 1, 'only the newly selected dirty Buy-Sell series is filled')
    assert.deepEqual(JSON.parse(JSON.stringify(h.run('buySellSeries.data'))).map(point => point.time), [60, 120])
    assert.equal(h.frames.size, 0)
})

test('flow summary follows the crosshair and returns to the latest bar', () => {
    const h = harness()
    const first = ind({ ...rawBar(60), buy_volume_usdt: 8, sell_volume_usdt: 5, delta_usdt: 3 })
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify([first, bar(120)])})`)
    h.flushFrames()
    h.emit('chart', 'crosshair', { time: 60, sourceEvent: {} })
    assert.match(h.run('flowSummaryEl.textContent'), flowRe('8', '5', '3'))
    h.emit('chart', 'crosshair', {})
    assert.match(h.run('flowSummaryEl.textContent'), flowRe('6', '5', '1'))
})

test('dataset reload resets historical flow hover while visible or hidden', async () => {
    for (const hidden of [false, true]) {
        const h = harness(); seed(h)
        h.emit('chart', 'crosshair', { time: 60, sourceEvent: {} })
        h.document.hidden = hidden
        if (hidden) h.emit('document', 'visibilitychange')
        h.writes.length = 0
        const latest = ind({ ...rawBar(240), buy_volume_usdt: 30, sell_volume_usdt: 10, delta_usdt: 20 })
        h.run(`fetch = async () => ({ok:true,json:async()=>({bars:${JSON.stringify([bar(60), latest])},has_more_before:false,has_more_after:false,realtime_enabled:false})})`)
        await h.run('reload()')
        assert.equal(h.run('hoveredFlowTime'), null)
        assert.match(h.run('flowSummaryEl.textContent'), flowRe('30', '10', '20'))
        if (hidden) {
            assert.equal(h.writes.length, 0)
            h.document.hidden = false
            h.emit('document', 'visibilitychange')
        }
        h.flushFrames()
        h.emit('chart', 'crosshair', { time: 60 }) // LWC replays cursor during setData/rescale.
        assert.equal(h.run('hoveredFlowTime'), null)
        assert.match(h.run('flowSummaryEl.textContent'), flowRe('30', '10', '20'))
        h.emit('chart', 'crosshair', { time: 60, sourceEvent: {} })
        assert.equal(h.run('hoveredFlowTime'), 60)
        assert.match(h.run('flowSummaryEl.textContent'), flowRe('6', '5', '1'))
    }
})

test('aggregate hover never falls back to valid single-source flow', () => {
    const h = harness(); seed(h)
    h.emit('chart', 'crosshair', { time: 60, sourceEvent: {} })
    h.run('aggToggle.checked = true; updateFlowReadout()')
    assert.match(h.run('flowSummaryEl.textContent'), flowRe('unavailable', 'unavailable', 'unavailable'))
    h.run('Object.assign(page.bars[0], {agb: 10, ags: 7, ad: 3}); __project([page.bars[0]]); updateFlowReadout()')
    assert.match(h.run('flowSummaryEl.textContent'), flowRe('10', '7', '3'))
})

test('hidden mode changes defer chart writes and restore latest summary in one render', () => {
    const h = harness(); seed(h)
    h.document.hidden = true; h.emit('document', 'visibilitychange'); h.writes.length = 0
    h.run('flowModeSel.value = "flow/buy_sell"; flowModeSel.onchange()')
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(180, 125))}})`)
    assert.equal(h.writes.length, 0)
    assert.equal(h.frames.size, 0)
    assert.match(h.run('flowSummaryEl.textContent'), flowRe('6', '5', '1'))
    h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    assert.equal(h.writes.filter(value => value === 'setData').length, 10, 'selected Buy-Sell replaces Delta in the visible set')
    assert.deepEqual(JSON.parse(JSON.stringify(h.run('buySellSeries.data'))).map(point => point.time), [60, 120, 180])
    assert.equal(h.run('flowModeSel.value'), 'flow/buy_sell')
})

test('pending prepend viewport survives subsequent hidden polling', async () => {
    const h = harness(); seed(h)
    h.run('chart.timeScale().setVisibleLogicalRange({from:0,to:1})')
    h.document.hidden = true; h.emit('document', 'visibilitychange')
    h.run(`fetch = async () => ({ok:true,json:async()=>({bars:${JSON.stringify([bar(30)])},has_more_before:true,has_more_after:false,realtime_enabled:false})})`)
    await h.run('loadMoreBefore()')
    assert.equal(h.run('JSON.stringify(pendingChartRange)'), '{"from":1,"to":2}')
    h.run(`fetch = async () => ({ok:true,json:async()=>({bars:${JSON.stringify([bar(180)])},has_more_before:true,has_more_after:false,realtime_enabled:false})})`)
    await h.run('pollDelayedUpdates()')
    assert.equal(h.run('JSON.stringify(pendingChartRange)'), '{"from":1,"to":2}')
    h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    assert.equal(h.run('JSON.stringify(chart.timeScale().getVisibleLogicalRange())'), '{"from":1,"to":2}')
})

test('pagehide blocks late ticks even before document.hidden changes', () => {
    const h = harness(); seed(h)
    assert.equal(h.document.hidden, false)
    h.emit('window', 'pagehide', { persisted: true })
    h.writes.length = 0
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(180, 125))}})`)
    assert.equal(h.writes.length, 0)
    assert.equal(h.frames.size, 0)
    assert.match(h.document.title, /125\.00/)
    h.emit('window', 'pageshow', { persisted: true }); h.flushFrames()
    assert.equal(h.writes.filter(v => v === 'setData').length, 10)
})

test('pane labels wait for autosize and coalesce resize work behind the hidden guard', () => {
    const h = harness(); seed(h)
    h.run('paneLabels.style.gridTemplateRows = "old"')
    h.emit('resize', 'chart')
    h.emit('resize', 'chart')
    assert.equal(h.run('paneLabels.style.gridTemplateRows'), 'old')
    assert.equal(h.frames.size, 1)
    h.flushFrames()
    assert.equal(h.run('paneLabels.style.gridTemplateRows'), '100px 100px 100px')
    h.emit('resize', 'chart')
    h.document.hidden = true
    h.emit('document', 'visibilitychange')
    assert.equal(h.frames.size, 0)
    h.emit('resize', 'chart')
    assert.equal(h.frames.size, 0)
})

test('quality badge selects hovered evidence without cross-source bleed', () => {
    const h = harness()
    const q = (contribution, recovery, observed = Date.now()) => ({ observed_at_ms: observed, status_observed_at_ms: observed, bar_state:'elapsed', coverage:'unverified', expected_sources:2, present_sources: contribution === 'missing' ? 1 : 2,
        sources:[{exchange:'binance',market:'spot',bar_time:60,contribution:'same_bucket',recovery:'live',last_trade_ms:null}, {exchange:'coinbase',market:'spot',bar_time:60,contribution,recovery,last_trade_ms:null}] })
    const first = {...bar(60), quality:q('missing','recovering')}
    const second = {...bar(120), quality:q('same_bucket','live')}
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify([first, second])})`)
    h.emit('chart','crosshair',{time:60,sourceEvent:{}})
    assert.equal(h.run('qualityStateEl.textContent'), 'Source ·', 'peer missing does not mark selected Binance source missing')
    h.emit('chart','crosshair',{})
    h.run('aggToggle.checked = true; refreshModeData()')
    assert.equal(h.run('qualityStateEl.textContent'), 'Quality ·')
    h.emit('chart','crosshair',{time:60,sourceEvent:{}})
    assert.equal(h.run('qualityStateEl.textContent'), 'Quality missing')
    assert.match(h.run('qualityStateEl.title'), /coinbase/)
    h.run('page.bars[1].quality.observed_at_ms -= 120000; page.bars[1].quality.status_observed_at_ms -= 120000; hoveredFlowTime = null')
    h.writes.length = 0
    h.timers.get(h.run('qualityTimer'))()
    assert.equal(h.run('qualityStateEl.textContent'), 'Quality stale')
    assert.equal(h.writes.length, 0, 'stale label timer never repaints chart')
    h.run('aggToggle.checked = false; updateFlowReadout()')
    assert.equal(h.run('qualityStateEl.textContent'), 'Source stale')
    h.run('exchangeSel.value = "coinbase"; hoveredFlowTime = 60; updateFlowReadout()')
    assert.equal(h.run('qualityStateEl.textContent'), 'Source missing')
    h.run('hoveredFlowTime = null; page.bars = []; updateFlowReadout()')
    assert.equal(h.run('qualityStateEl.textContent'), 'Quality unknown')
});

test('early pageshow/focus cannot connect before initial history establishes selection and mode', () => {
    const h = harness()
    h.run('ensureModeConnections = realEnsureModeConnections')
    assert.doesNotThrow(() => {
        h.emit('window', 'pageshow', { persisted: false })
        h.emit('window', 'focus')
    })
    assert.equal(h.timers.size, 0)
    assert.equal(h.frames.size, 0)
})

test('unselected flow mode receives no chart data until revealed', () => {
    const h = harness(); seed(h)
    const snapshot = name => JSON.parse(JSON.stringify(h.run(`${name}.data`)))
    const buySellBefore = snapshot('buySellSeries')
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(180, 125))}})`)
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(180, 126))}})`)
    assert.deepEqual(snapshot('buySellSeries'), buySellBefore, 'unselected Buy-Sell untouched')
    assert.equal(h.writes.filter(v => v === 'update').length, 7, 'price/CVD/Delta twice; zero backbone only for the new timestamp')
    h.writes.length = 0
    h.run('flowModeSel.value = "flow/buy_sell"; flowModeSel.onchange()')
    assert.equal(h.writes.filter(v => v === 'setData').length, 1)
    assert.deepEqual(snapshot('buySellSeries').map(p => p.time), [60, 120, 180])
    h.writes.length = 0
    h.run('flowModeSel.value = "flow/delta"; flowModeSel.onchange()')
    assert.equal(h.writes.includes('setData'), false, 'delta stayed current')
})

test('hover lookup uses indexed bar and skips unchanged readout DOM writes', () => {
    const h = harness(); seed(h)
    h.run('page.bars.find = () => { throw new Error("linear scan") }')
    h.emit('chart', 'crosshair', { time: 60, sourceEvent: {} })
    let sets = 0
    h.run('flowSummaryEl')
    h.run(`(() => { let t = flowSummaryEl.textContent; Object.defineProperty(flowSummaryEl, 'textContent', { get: () => t, set: v => { globalThis.__sets = (globalThis.__sets || 0) + 1; t = v } }) })()`)
    h.emit('chart', 'crosshair', { time: 60, sourceEvent: {} })
    sets = h.run('globalThis.__sets || 0')
    assert.equal(sets, 0)
})

function fragmentedBars(count, gapEvery, extra = {}) {
    return Array.from({ length: count }, (_, i) => {
        const gap = i % gapEvery === 0
        const p = 100 + i
        return ind({ ...rawBar((i + 1) * 60, p), ...extra,
            lt: gap ? [null, null, null, null] : [null, p + 2, p + 3, p + 4], lb: gap ? [null, null, null, null] : [null, p - 2, p - 3, p - 4],
            alt: gap ? [null, null, null, null] : [null, p + 20, p + 30, p + 40], alb: gap ? [null, null, null, null] : [null, p - 20, p - 30, p - 40] })
    })
}
function levelsCanonical(h) {
    // Canonicalize live LEVELS series back to native-equivalent segments.
    return JSON.parse(h.run(`JSON.stringify(levelsSlots.map(slot => slot ? [...(slot.history ? levelsHistorySegments(slot.history.series.data) : []), ...(slot.tail ? [slot.tail.series.data] : [])] : null))`))
}
function expectedLevels(bars, aggregate) {
    const out = []
    for (const layer of [1, 2, 3]) for (const key of aggregate ? ['alt', 'alb'] : ['lt', 'lb']) {
        const segments = []; let run = []
        for (const b of bars) {
            const v = b[key][layer]
            if (Number.isFinite(v)) run.push({ time: b.time, value: v })
            else { if (run.length > 1) segments.push(run); run = [] }
        }
        if (run.length > 1) segments.push(run)
        out[(layer - 1) * 2 + (key.endsWith('b') ? 1 : 0)] = segments
    }
    return out
}

test('fragmented LEVELS stay bounded: native tail + one custom history per layer/side, exact segment parity', () => {
    const h = harness(); seed(h)
    const bars = fragmentedBars(800, 10)
    h.writes.length = 0
    h.run(`setLevelsData(${JSON.stringify(bars)})`)
    assert.equal(h.run('levelsRenderedSeries.length'), 12, 'six layer/sides x (history + tail), independent of ~80 gaps')
    assert.ok(h.series.filter(item => !item.removed && (item.type === 'Line' || item.type === 'Custom') && h.run('levelsRenderedSeries').some(e => e.series === item)).length <= 12)
    assert.equal(h.run('levelsRenderedSeries.filter(e => e.kind === "tail").every(e => e.series.type === "Line" && e.series.options.lastValueVisible === true && e.series.options.crosshairMarkerVisible === true && e.marker)'), true)
    assert.equal(h.run('levelsRenderedSeries.filter(e => e.kind === "history").every(e => e.series.type === "Custom" && e.series.options.lastValueVisible === false && e.series.options.priceLineVisible === false && !e.marker)'), true)
    assert.equal(h.run('JSON.stringify(levelsRenderedSeries.map(e => [e.series.options.color, e.series.options.lineWidth]))'),
        JSON.stringify([2, 2, 2, 2, 4, 4, 4, 4, 6, 6, 6, 6].map((w, i) => [i % 4 < 2 ? '#eb5757' : '#27ae60', w])))
    assert.deepEqual(levelsCanonical(h), Array.from(expectedLevels(bars, false), x => x ?? null))
    assert.equal(h.run('levelsRenderedSeries.filter(e => e.kind === "history").every(e => e.series.data[0].start === true && e.series.data.every(p => Number.isFinite(p.value)))'), true, 'explicit starts, no faked gap values')
    // Unchanged refresh: no setData / add / remove.
    h.writes.length = 0
    const count = h.series.length
    h.run(`setLevelsData(${JSON.stringify(bars)})`)
    assert.deepEqual(h.writes, [])
    assert.equal(h.series.length, count)
    // Aggregate switch uses alt/alb, still bounded and in parity.
    h.run(`aggToggle.checked = true; setLevelsData(${JSON.stringify(bars)})`)
    assert.equal(h.run('levelsRenderedSeries.length'), 12)
    assert.deepEqual(levelsCanonical(h), Array.from(expectedLevels(bars, true), x => x ?? null))
    assert.equal(h.writes.filter(v => v === 'removeSeries').length, 0, 'series reused across aggregate switch')
    // Toggle off removes everything; on recreates bounded set.
    h.run('levelsToggle.checked = false; levelsToggle.onchange()')
    assert.equal(h.run('levelsRenderedSeries.length + levelsSlots.length'), 0)
    h.run(`page.bars = ${JSON.stringify(bars)}; renderedWindow = { start: 0, bars: page.bars }; levelsToggle.checked = true; levelsToggle.onchange()`)
    assert.equal(h.run('levelsRenderedSeries.length'), 12)
    // Continuous data keeps six native tails only.
    h.run(`aggToggle.checked = false; setLevelsData(${JSON.stringify(fragmentedBars(50, 1000))})`)
    assert.equal(h.run('levelsRenderedSeries.length'), 6)
    assert.equal(h.run('levelsRenderedSeries.every(e => e.kind === "tail")'), true)
})

test('toggles while hidden or page-leaving make no chart writes and flush once on restore', () => {
    const h = harness(); seed(h)
    h.document.hidden = true; h.emit('document', 'visibilitychange'); h.writes.length = 0
    h.run('levelsToggle.checked = false; levelsToggle.onchange()')
    h.run('flowModeSel.value = "flow/buy_sell"; flowModeSel.onchange()')
    assert.deepEqual(h.writes, [], 'hidden toggles do not touch the chart')
    assert.equal(h.frames.size, 0)
    h.document.hidden = false; h.emit('document', 'visibilitychange'); h.flushFrames()
    assert.deepEqual(JSON.parse(JSON.stringify(h.run('deltaZeroSeries.data'))).map(p => p.time), [60, 120])
    assert.equal(h.run('levelsRenderedSeries.length'), 0)
    assert.equal(h.run('buySellSeries.options.visible'), true)
    assert.deepEqual(JSON.parse(JSON.stringify(h.run('buySellSeries.data'))).map(p => p.time), [60, 120])
    // pagehide before document.hidden flips: same guarantee.
    h.emit('window', 'pagehide', { persisted: true }); h.writes.length = 0
    h.run('levelsToggle.checked = true; levelsToggle.onchange()')
    assert.deepEqual(h.writes, [])
    h.emit('window', 'pageshow', { persisted: true }); h.flushFrames()
    assert.equal(h.run('levelsRenderedSeries.length'), 6, 'LEVELS enabled while away are drawn on restore')
    assert.equal(h.run('visualOptionsPending'), false)
})

// Final P1: merged history ranges are reanchored by timestamp, not raw index.
function times(h, expr = 'page.bars') { return JSON.parse(h.run(`JSON.stringify(${expr}.map(b => b.time))`)) }
function startAt(h, list, range) {
    h.run(`modeConnectionsReady = true; setBars(${JSON.stringify(list.map(t => bar(t)))}, ${JSON.stringify(range)})`)
    h.flushFrames(); h.writes.length = 0
}
function respond(bars, extra = {}) {
    return `({ok:true,json:async()=>({bars:${JSON.stringify(bars)},has_more_before:true,has_more_after:false,realtime_enabled:true,...${JSON.stringify(extra)}})})`
}

test('loadMoreAfter interior insert keeps the timestamp anchor captured at response time', async () => {
    const h = harness(); startAt(h, [60, 180, 240], { from: 0, to: 1 })
    h.run('page.confirmedTime = 60; let historyResponse; fetch = () => new Promise(resolve => { historyResponse = resolve })')
    const loading = h.run('lastAfterFetchAt = 0; loadMoreAfter()')
    h.run('chart.timeScale().setVisibleLogicalRange({from:1.25,to:2.5})') // user moved while pending
    h.run(`historyResponse(${respond([bar(120), bar(180), bar(240)])})`)
    await loading; h.flushFrames()
    assert.deepEqual(times(h), [60, 120, 180, 240])
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":2.25,"to":3.5}')
    assert.deepEqual(times(h, 'deltaZeroSeries.data'), [60, 120, 180, 240])
})

test('catchUpHistory and hidden polling reanchor current/pending ranges for interior inserts', async () => {
    const h = harness(); startAt(h, [60, 180, 240], { from: 1, to: 2 })
    h.run('page.confirmedTime = 60')
    h.run(`fetch = async () => ${respond([bar(120)])}`)
    await h.run('catchUpHistory(wsGeneration, new Map())')
    h.flushFrames()
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":2,"to":3}')
    assert.equal(h.run('page.bars[Math.floor(currentChartRange().from)].time'), 180)

    const g = harness(); startAt(g, [60, 180, 240, 300], { from: 0, to: 1 })
    g.document.hidden = true; g.emit('document', 'visibilitychange')
    g.run('pendingChartRange = {from:1.5,to:2.75}')
    g.run(`fetch = async () => ${respond([bar(120), bar(360)])}`)
    await g.run('pollDelayedUpdates()')
    assert.equal(g.run('JSON.stringify(pendingChartRange)'), '{"from":2.5,"to":3.75}')
    g.document.hidden = false; g.emit('document', 'visibilitychange'); g.flushFrames()
    assert.equal(g.run('JSON.stringify(chart.timeScale().getVisibleLogicalRange())'), '{"from":2.5,"to":3.75}')
    assert.deepEqual(times(g, 'deltaZeroSeries.data'), [60, 120, 180, 240, 300, 360])
})

test('reanchoring covers inserts inside range, append, prepend overscroll and empty initial load', async () => {
    const h = harness(); startAt(h, [60, 120, 240, 300], { from: 0.5, to: 2.5 })
    h.run(`setMergedBars(mergeHistoryBars(${JSON.stringify([bar(180)])}), currentChartRange())`); h.flushFrames()
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":0.5,"to":2.5}', 'insert after anchor keeps anchor and span')
    h.run(`setMergedBars(mergeHistoryBars(${JSON.stringify([bar(360)])}), currentChartRange())`); h.flushFrames()
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":0.5,"to":2.5}', 'append is index-stable')
    h.run('chart.timeScale().setVisibleLogicalRange({from:3.5,to:7})') // right overscroll
    h.run(`setMergedBars(mergeHistoryBars(${JSON.stringify([bar(30)])}), currentChartRange())`); h.flushFrames()
    assert.equal(h.run('JSON.stringify(currentChartRange())'), '{"from":4.5,"to":8}')

    const p = harness(); startAt(p, [120, 180], { from: -3.5, to: 1 })
    p.run(`fetch = async () => ${respond([bar(0), bar(60)])}`)
    await p.run('lastBeforeFetchAt = 0; loadMoreBefore()'); p.flushFrames()
    assert.equal(p.run('JSON.stringify(currentChartRange())'), '{"from":-1.5,"to":3}', 'prepend shifts once, overscroll preserved')

    const e = harness()
    e.run(`modeConnectionsReady = true; fetch = async () => ${respond([bar(60), bar(120)])}`)
    await e.run('loadInitial()'); e.flushFrames()
    assert.deepEqual(times(e), [60, 120])
})

test('timeline backbone tracks full timestamp membership, not just length/first/last', () => {
    const h = harness(); startAt(h, [60, 120, 180, 240, 300], { from: 2, to: 3 })
    h.run('setBars(page.bars.map((b, i) => i === 2 ? {...b, time: 181} : b), {from:2,to:3})')
    assert.deepEqual(times(h, 'deltaZeroSeries.data'), [60, 120, 181, 240, 300])
    h.writes.length = 0
    h.run('const zero = deltaZeroSeries.data; setBars(page.bars.map(b => ({...b, price_close: 101})), {from:2,to:3})')
    assert.equal(h.run('deltaZeroSeries.data === zero'), true, 'value-only replacement does not resubmit backbone')
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(360))}})`)
    h.run(`applyRealtimePayload({bar:${JSON.stringify(bar(360, 105))}})`)
    assert.deepEqual(times(h, 'deltaZeroSeries.data'), [60, 120, 181, 240, 300, 360])
    h.run('setBars(page.bars.map((b, i) => i === 1 ? {...b, time: 121} : b), {from:2,to:3})')
    assert.deepEqual(times(h, 'deltaZeroSeries.data'), [60, 121, 181, 240, 300, 360], 'snapshot follows tail appends')
})

test('public+test descriptors give exactly three panes: Price(+segmented), CVD, Flow (Delta | Buy-Sell); backbone is host-owned on Price', () => {
    const h = harness(); seed(h)
    const panes = [...new Set(h.series.filter(item => !item.removed).map(item => item.pane))].sort()
    assert.deepEqual(panes, [0, 1, 2])
    assert.equal(h.run('cvdSeries.pane'), 1)
    assert.equal(h.run('deltaPaneSeries.pane'), 2)
    assert.equal(h.run('buySellSeries.pane'), 2)
    assert.equal(h.run('deltaZeroSeries.pane'), 0, 'backbone never lives in a removable plugin pane')
    assert.equal(h.run('deltaZeroSeries.options.visible'), false)
    assert.equal(h.series.some(item => item.options.priceScaleId === 'distance'), false, 'no Strength scale')
    assert.ok(h.series.filter(item => item.pane === 0 && !item.removed).length > 1, 'LEVELS on price pane')
})

test('dataset reset clears native crosshair before series data is removed', () => {
    for (const reset of ['setBars([])', 'initialViewportPending = true; setBars(page.bars)']) {
        const h = harness()
        seed(h); h.flushFrames(); h.writes.length = 0
        h.run(reset)
        const clear = h.writes.indexOf('clearCrosshair')
        const firstData = h.writes.findIndex(w => w === 'setData' || w === 'removeSeries')
        assert.ok(clear >= 0, `${reset}: crosshair cleared`)
        assert.ok(firstData < 0 || clear < firstData, `${reset}: clear precedes data removal: ${h.writes.join(',')}`)
    }
})

test('ordinary live bar updates keep native crosshair', () => {
    const h = harness()
    seed(h); h.flushFrames(); h.writes.length = 0
    h.run(`setBars([...page.bars, ${JSON.stringify(bar(180))}])`)
    assert.ok(!h.writes.includes('clearCrosshair'))
})

for (const hidden of [false, true]) for (const [from, to] of [['flow/buy_sell', 'flow/delta'], ['flow/delta', 'flow/buy_sell']]) {
    test(`source reset clears unselected Flow series (${hidden ? 'hidden reload→resume' : 'visible reload'}, last ${to})`, async () => {
        const h = harness()
        seed(h)
        h.run(`flowModeSel.value='${from}';updateFlowMode();flowModeSel.value='${to}';updateFlowMode()`)
        const other = to === 'flow/delta' ? 'buySellSeries' : 'deltaPaneSeries'
        const selected = to === 'flow/delta' ? 'deltaPaneSeries' : 'buySellSeries'
        assert.deepEqual(JSON.parse(h.run(`JSON.stringify(${other}.data.map(p => p.time))`)), [60, 120])
        h.document.hidden = hidden
        if (hidden) h.emit('document', 'visibilitychange')
        h.run(`fetch = async () => ({ ok: true, json: async () => ({ series: 'spot-v1', bars: ${JSON.stringify([bar(300), bar(600)])}, has_more_before: false, has_more_after: false, realtime_enabled: true }) })`)
        h.writes.length = 0
        await h.run('reload()')
        if (hidden) {
            assert.deepEqual(h.writes.filter(w => ['setData', 'removeSeries', 'update', 'clearCrosshair'].includes(w)), [], 'hidden reload makes zero chart writes')
            h.document.hidden = false; h.emit('document', 'visibilitychange')
        }
        h.flushFrames()
        assert.deepEqual(JSON.parse(h.run('JSON.stringify(page.bars.map(b => b.time))')), [300, 600])
        assert.deepEqual(JSON.parse(h.run(`JSON.stringify(${other}.data.map(p => p.time))`)), [], 'stale hidden Flow has no old timestamps')
        assert.deepEqual(JSON.parse(h.run(`JSON.stringify(${selected}.data.map(p => p.time))`)), [300, 600])
        const clear = h.writes.indexOf('clearCrosshair'), firstData = h.writes.indexOf('setData')
        assert.ok(clear >= 0 && clear < firstData, 'crosshair cleared before data reset')
        h.run(`flowModeSel.value='${from}';updateFlowMode()`)
        assert.deepEqual(JSON.parse(h.run(`JSON.stringify(${other}.data.map(p => p.time))`)), [300, 600], 'revealed mode matches new cache')
    })
}
