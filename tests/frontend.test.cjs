const { test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const vm = require('node:vm')


function prefsHarness(metaPayload, failMeta = false) {
    function select(options) {
        let allowed = options, value = options[0], markup = '';
        return {
            get value() { return value },
            set value(v) { value = allowed.includes(v) ? v : '' },
            get innerHTML() { return markup },
            set innerHTML(html) { markup = html; allowed = [...html.matchAll(/value="([^"]+)"/g)].map(m => m[1]); value = allowed[0] || '' },
        }
    }
    const saved = {};
    const context = vm.createContext({
        console: { warn() {} },
        fetch: async () => { if (failMeta) throw new Error('offline'); return { ok: true, json: async () => metaPayload } },
        exchangeSel: select(['binance','coinbase','bybit']),
        marketSel: select(['spot','usdm']), symbolSel: select(['BTC','ETH']), tfSel: select(['1m','5m','15m','30m','1h']),
        sourceSeriesEl: {textContent: '', title: ''}, accessKeyInput: {value: ''}, aggToggle: {},
        pluginPrefs: { enabled: {}, modes: {} },
        localStorage: {setItem: (k,v) => saved[k] = v},
    });
    const config = fs.readFileSync(require('node:path').join(__dirname,'../static/js/config.js'),'utf8').replaceAll('export const','const').replaceAll('export function','function');
    const legacy = fs.readFileSync(require('node:path').join(__dirname,'../static/js/legacy-prefs.mjs'),'utf8').replaceAll('export function','function');
    vm.runInContext(config + '\n' + legacy + '\n' + section('const fallbackInstruments','function mkChart') + '\n' + section('function marketLabel','const chart =') + '\n' + section('function sanitizePluginPrefs','function pluginEnabled'), context);
    return { run: code => vm.runInContext(code, context), saved };
}

test('explicit exchange changes preserve derivative intent without reinterpreting bad preferences', async () => {
    const {run}=prefsHarness({contract_series:'contracts-drb-v1',exchanges:[]});
    await run('loadMeta()');
    run('applyPrefs({exchange:"coinbase",market:"usdc",symbol:"BTC"})');
    run('exchangeSel.value="binance";updateOptionsForExchange(true)');
    assert.equal(run('marketSel.value'),'usdm');
    run('exchangeSel.value="coinbase";updateOptionsForExchange(true)');
    assert.equal(run('marketSel.value'),'usdc');
    run('applyPrefs({exchange:"binance",market:"usdc",symbol:"BTC"})');
    assert.equal(run('marketSel.value'),'spot','invalid saved pairing is not a manual exchange change');
});

test('retired exchange cannot reappear from stale metadata or fallback configuration', async () => {
    const payload = {exchanges: ['bybit','binance','coinbase'].map(exchange => ({exchange, markets:['spot','usdm'], symbols:['BTC','ETH']}))};
    for (const failMeta of [false,true]) {
        const {run} = prefsHarness(payload,failMeta);
        await run('loadMeta()');
        assert.equal(run('Object.keys(exchangeMeta).includes("bybit")'),false);
        run('applyPrefs({exchange:"bybit",market:"usdm",symbol:"ETH",tf:"1h",accessKey:"test-key",showAgg:true})');
        assert.equal(run('exchangeSel.value'),'binance');
        assert.equal(run('marketSel.value'),'usdm');
        assert.equal(run('symbolSel.value'),'ETH');
        assert.equal(run('tfSel.value'),'1h');
        assert.equal(run('accessKeyInput.value'),'test-key');
        assert.equal(run('aggToggle.checked'),true);
        run('prefsHydrated=true; savePrefs()');
    }
});

test('preference migration remains usable when storage writes are denied', async () => {
    const {run} = prefsHarness({exchanges:[]});
    await run('loadMeta()');
    run('applyPrefs({exchange:"bybit",symbol:"ETH"}); prefsHydrated=true; localStorage.setItem=()=>{throw new Error("quota")};');
    assert.doesNotThrow(() => run('savePrefs()'));
    assert.equal(run('exchangeSel.value'),'binance');
});

test('valid Coinbase preferences survive retirement and invalid selections become valid', async () => {
    const {run} = prefsHarness({exchanges:[]});
    await run('loadMeta()');
    run('applyPrefs({exchange:"coinbase",market:"usdm",symbol:"ETH",tf:"5m"})');
    assert.equal(run('exchangeSel.value'),'coinbase');
    assert.equal(run('marketSel.value'),'usdc');
    assert.match(run('sourceSeriesEl.textContent'), /independent.*series/i);
    run('applyPrefs({exchange:"bybit",market:"bad",symbol:"bad",tf:"bad"})');
    assert.equal(run('exchangeSel.value'),'binance');
    assert.equal(run('marketSel.value'),'spot');
    assert.equal(run('symbolSel.value'),'BTC');
    assert.equal(run('tfSel.value'),'1m');
});

test('plugin mode/enable preferences persist generically; invalid entries are dropped', async () => {
    const {run, saved} = prefsHarness({exchanges:[]});
    await run('loadMeta()');
    run('applyPrefs({flowMode:"buy-sell", showLevels:false, tf:"5m", plugins:{enabled:{p:false, q:"yes"}, modes:{"pane:group":"p/out", bad:7}}})');
    run('prefsHydrated=true; savePrefs()');
    const prefs = JSON.parse(saved.cvd_prefs_v1);
    assert.deepEqual(prefs.plugins, {enabled:{p:false}, modes:{'pane:group':'p/out'}}, 'existing plugins block wins; no re-migration');
    assert.equal(Object.hasOwn(prefs, 'flowMode'), false);
    assert.equal(prefs.tf, '5m', 'useful settings survive');
});

test('one-time legacy migration keeps old Buy-Sell mode and LEVELS visibility, then never re-applies', async () => {
    const {run, saved} = prefsHarness({exchanges:[]});
    await run('loadMeta()');
    run('applyPrefs({exchange:"binance",market:"spot",symbol:"ETH",tf:"15m",accessKey:"k",showLevels:false,showAgg:true,flowMode:"buy-sell"}); prefsHydrated=true; savePrefs()');
    const prefs = JSON.parse(saved.cvd_prefs_v1);
    assert.deepEqual(prefs.plugins, {enabled:{levels:false}, modes:{'flow:flow':'flow/buy_sell'}});
    assert.equal(prefs.tf, '15m'); assert.equal(prefs.symbol, 'ETH'); assert.equal(prefs.showAgg, true); assert.equal(prefs.accessKey, 'k');
    assert.equal(Object.hasOwn(prefs, 'flowMode') || Object.hasOwn(prefs, 'showLevels'), false);
    run('applyPrefs(' + JSON.stringify({...prefs, plugins:{enabled:{levels:true},modes:{}}}) + '); savePrefs()');
    assert.deepEqual(JSON.parse(saved.cvd_prefs_v1).plugins, {enabled:{levels:true}, modes:{}}, 'later user choices are not overwritten');
});

const source = fs.readFileSync(require('node:path').join(__dirname, '../static/js/app.js'), 'utf8')
function section(start, end) { return source.slice(source.indexOf(start), source.indexOf(end)) }
function harness() {
    const requests = []
    const timers = new Map()
    const sockets = []
    let nextTimer = 0
    const noop = () => {}
    const updates = []
    const series = { update: point => updates.push(point) }
    class MockSocket {
        static OPEN = 1
        static CLOSING = 2
        static CLOSED = 3
        readyState = 1
        constructor(url) { this.url = url; sockets.push(this) }
        send() {}
        close() { this.readyState = 3; this.onclose?.() }
    }
    const context = vm.createContext({
        URL, AbortController, DOMException, console, Map, Date,
        document: { hidden: false }, chartNeedsRender: false, fullWrites: 0, readoutWrites: 0, timeReads: 0,
        pendingChartRange: null, initialViewportPending: false, cancelVisualWork: noop,
        renderedWindow: { start: 0, bars: [] }, chartRangeForCache: range => range, refreshRenderWindow: noop,
        currentChartRange: () => null,
        visualWorkSuspended: () => false,
        location: { origin: 'http://localhost', protocol: 'http:', host: 'localhost' },
        fetch: (url, options) => new Promise(resolve => requests.push({ url, options, resolve })),
        requestAnimationFrame: callback => callback(),
        setInterval: (callback, delay) => { timers.set(++nextTimer, { callback, delay }); return nextTimer },
        clearInterval: timer => timers.delete(timer), setTimeout: noop, WebSocket: MockSocket,
        exchangeSel: { value: 'binance' }, symbolSel: { value: 'BTC' }, marketSel: { value: 'spot' }, tfSel: { value: '1m' },
        statusEl: {}, currentAccessKey: () => '', historyPageSize: () => 2,
        chart: { timeScale: () => ({ getVisibleLogicalRange: () => null }) },
        updateDocumentTitle: noop, resetViewport: noop, syncPaneLayout: noop,
        clearScheduledSegmentedRefresh: noop, scheduleSegmentedRefresh: noop, applyHistoryIndicators: noop, noteDataChanged: noop, invalidateLocalAt: noop,
        indicatorStore: { reset: noop, resetFences: noop, pruneBefore: noop, noteBaseRev: noop, baseSuperseded: () => false }, noteBaseRev: noop, parseIndicatorFrame: () => null, handleIndicatorEvent: noop, resyncIndicators: noop,
        pluginStatus: {}, renderPluginControls: noop,
        // Stands in for the two default public plugin outputs (CVD line + selected Flow).
        updatePluginPoint: bar => { series.update(bar); series.update(bar) },
        priceSeries: series, backboneSeries: series,
        pricePointForBar: bar => bar, updateFlowReadout: () => context.readoutWrites++, useAggregateMode: () => false,
        CONFIG: { FETCH_COOLDOWN: 0, DELAY_POLL_MS: 1000 }, warmupPrepending: false, localOlderPages: [],
    })
    vm.runInContext(fs.readFileSync(require('node:path').join(__dirname, '../static/js/quality-display.mjs'), 'utf8').replaceAll('export function', 'function'), context)
    const config = fs.readFileSync(require('node:path').join(__dirname,'../static/js/config.js'),'utf8').replaceAll('export const','const').replaceAll('export function','function');
    vm.runInContext(config + '\nCONFIG.DELAY_POLL_MS=1000; CONFIG.FETCH_COOLDOWN=0;\n' + section('const fallbackInstruments', 'function savePrefs') + '\n' + section('let programmaticRangeChange', 'function emptyHistoryPayload')
        + '\n' + section('function assertSourceSeries', 'function updateOptionsForExchange')
        + '\n' + section('let timelineTimes', 'function selectRenderWindow')
        + '\nfunction setBars(bars) { fullWrites++; page.bars = bars; renderedWindow.bars = bars.slice(); page.oldestTime = bars[0]?.time ?? null; page.newestTime = bars.at(-1)?.time ?? null }\n'
        + section('async function fetchHistory', 'chart.timeScale().subscribeVisibleLogicalRangeChange')
        + section('function stopDelayedPolling', 'function suspendUpdates')
        + '\nensureModeConnections = () => {};', context)
    return { context, requests, timers, sockets, updates, run: code => vm.runInContext(code, context) }
}
function respond(request, bars, extra = {}) {
    request.resolve({ ok: true, json: async () => ({ bars, realtime_enabled: true, has_more_after: false, ...extra }) })
}

test('long default-chart history skips identical data and only submits changed tail', () => {
    const { run, updates } = harness()
    run('page.bars = Array.from({length:16000}, (_, index) => ({time:index+1, price_close:1, iv:["ready"], quality:null})); page.newestTime=16000')
    run('renderedWindow.bars = page.bars.slice()')
    run('for (const bar of page.bars) { const time=bar.time;Object.defineProperty(bar,"time",{enumerable:true,get(){timeReads++;return time}}) }')
    run('setMergedBars(mergeHistoryBars([{quality:null, iv:["ready"], price_close:1, time:16000}]))')
    assert.equal(run('fullWrites'), 0)
    assert.equal(updates.length, 0)
    assert.ok(run('timeReads') < 30, 'unchanged overlap does not scan all history timestamps')
    run('setMergedBars(mergeHistoryBars([{time:16001,price_close:3}, {time:16000,price_close:2,iv:["ready"],quality:null}]))')
    assert.equal(run('fullWrites'), 0)
    assert.equal(updates.length, 8, 'four base series updates per changed bar (price, CVD, selected flow, zero)')
    assert.equal(run('page.bars.length'), 16001)
    assert.equal(run('page.bars.at(-2).price_close'), 2)
    assert.equal(run('readoutWrites'), 1, 'one readout per history tail batch')
    run('setMergedBars(mergeHistoryBars([{time:120,price_close:1,quality:null,cvd_usdt:42}]))')
    assert.equal(run('fullWrites'), 1, 'historical CVD correction requires full render')
    run('applyRealtimePayload({bar:{time:16001,price_close:3}})')
    assert.equal(updates.length, 8, 'identical realtime payload skips chart updates')
    run('setMergedBars(mergeHistoryBars([{time:16001,price_close:3,quality:null}]))')
    assert.equal(updates.length, 11, 'explicit quality withdrawal is a tail change (same-time: zero backbone untouched)')
    run('marketSel.value="coinm";setMergedBars(mergeHistoryBars([{time:16002,price_close:4}]))')
    assert.equal(updates.length, 15, 'validated contract history uses the same tail renderer')
    run('chartNeedsRender=true;setMergedBars(mergeHistoryBars([{time:16002,price_close:4}]))')
    assert.equal(run('fullWrites'), 1, 'identical history does not force another pending render')
})

test('old selection response cannot overwrite a newer reload even when abort is ignored', async () => {
    const { run, requests } = harness()
    const old = run('reload()')
    run('symbolSel.value = "ETH"')
    const current = run('reload()')
    assert.equal(requests[0].options.signal.aborted, true)
    respond(requests[1], [{ time: 2, symbol: 'ETH' }])
    await current
    respond(requests[0], [{ time: 1, symbol: 'BTC' }])
    await old
    assert.equal(run('page.bars[0].symbol'), 'ETH')
})

test('HTTP errors remain errors rather than pretending history is empty', async () => {
    const { run, requests } = harness()
    const pending = run('fetchHistory(2)')
    requests[0].resolve({ ok: false, status: 503 })
    await assert.rejects(pending, /503/)
})

test('reconnect paginates with overlap and closed history wins over buffered partial bars', async () => {
    const { run, requests } = harness()
    run('setBars([{time: 60, value: "partial"}]); page.confirmedTime = 60')
    const pending = run('catchUpHistory(wsGeneration, new Map([[60, {bar: {time: 60, value: "stale"}}], [240, {bar: {time: 240, value: "live"}}]]))')
    assert.equal(requests[0].url.searchParams.get('after'), '59')
    respond(requests[0], [{ time: 60, value: 'closed' }, { time: 120 }], { has_more_after: true })
    await new Promise(resolve => setImmediate(resolve))
    assert.equal(requests[1].url.searchParams.get('after'), '120')
    respond(requests[1], [{ time: 180 }])
    assert.equal(await pending, true)
    assert.equal(run('JSON.stringify(page.bars.map(bar => bar.time))'), '[60,120,180,240]')
    assert.equal(run('page.bars[0].value'), 'closed')
})

test('closed socket catchup cannot mutate the chart', async () => {
    const { run, requests } = harness()
    const pending = run('catchUpHistory(wsGeneration, new Map())')
    run('wsGeneration += 1')
    respond(requests[0], [{ time: 60 }])
    assert.equal(await pending, false)
    assert.equal(run('page.bars.length'), 0)
    assert.equal(run('page.confirmedTime'), null)
})

test('empty chart accepts first realtime bar', () => {
    const { run } = harness()
    run('applyRealtimePayload({bar: {time: 60}})')
    assert.equal(run('page.oldestTime'), 60)
    assert.equal(run('page.newestTime'), 60)
})

test('persistence lag cannot advance the confirmed cursor past missing history', async () => {
    const { run, requests } = harness()
    run('applyHistoryPage({bars: [{time: 60}], realtime_enabled: true}); applyRealtimePayload({bar: {time: 240}})')
    const first = run('catchUpHistory(wsGeneration, new Map())')
    assert.equal(requests[0].url.searchParams.get('after'), '59')
    respond(requests[0], [{ time: 60 }])
    await first
    assert.equal(run('page.confirmedTime'), 60)
    const retry = run('catchUpHistory(wsGeneration, new Map())')
    assert.equal(requests[1].url.searchParams.get('after'), '59')
    respond(requests[1], [{ time: 60 }, { time: 120 }, { time: 180 }])
    await retry
    assert.equal(run('JSON.stringify(page.bars.map(bar => bar.time))'), '[60,120,180,240]')
    assert.equal(run('page.confirmedTime'), 180)
})

test('forward pagination merges missing bars even when realtime advances during the request', async () => {
    const { run, requests } = harness()
    run('applyHistoryPage({bars: [{time: 60}], has_more_after: true, realtime_enabled: true})')
    const pending = run('loadMoreAfter()')
    run('applyRealtimePayload({bar: {time: 240}})')
    respond(requests[0], [{ time: 120 }, { time: 180 }])
    await pending
    assert.equal(run('JSON.stringify(page.bars.map(bar => bar.time))'), '[60,120,180,240]')
    assert.equal(run('page.confirmedTime'), 180)
})

test('empty delayed history can fill on a later poll', async () => {
    const { run, requests } = harness()
    run('applyHistoryPage({bars: [], realtime_enabled: false})')
    const pending = run('pollDelayedUpdates()')
    respond(requests[0], [{ time: 60 }], { realtime_enabled: false })
    await pending
    assert.equal(run('page.newestTime'), 60)
    assert.equal(run('page.loadingNewer'), false)
})

test('live sockets periodically reconcile history and release timers on stop', async () => {
    const { run, requests, timers, sockets } = harness()
    run('applyHistoryPage({bars: [{time: 60}], realtime_enabled: true}); connectWs()')
    const opened = sockets[0].onopen()
    respond(requests[0], [{ time: 60 }])
    await opened
    const reconcile = [...timers.values()].find(timer => timer.delay === 1000)
    assert.ok(reconcile)
    reconcile.callback()
    sockets[0].onmessage({ data: JSON.stringify({ bar: { time: 180 } }) })
    respond(requests[1], [{ time: 60 }, { time: 120 }])
    await new Promise(resolve => setImmediate(resolve))
    assert.equal(run('JSON.stringify(page.bars.map(bar => bar.time))'), '[60,120,180]')
    assert.equal(run('page.confirmedTime'), 120)
    run('stopRealtimeSocket()')
    assert.equal(timers.size, 0)
})

test('Coinbase meta, stale meta, and fallback never resurrect retired usdm', async () => {
    for (const [payload, fail] of [
        [{contract_series:'contracts-drb-v1',exchanges:[{exchange:'coinbase',markets:['spot','usdc'],symbols:['BTC','ETH']}]},false],
        [{contract_series:'contracts-drb-v1',exchanges:[{exchange:'coinbase',markets:['spot'],symbols:['BTC','ETH']}]},false],
        [{contract_series:'old',exchanges:[{exchange:'coinbase',markets:['spot','usdm'],symbols:['BTC']}]},false],
        [{},true],
    ]) {
        const {run} = prefsHarness(payload, fail);
        await run('loadMeta()');
        assert.equal(run('exchangeMeta.coinbase.markets.includes("usdm")'),false);
        run('applyPrefs({exchange:"coinbase",market:"usdm",symbol:"ETH"})');
        const available = run('exchangeMeta.coinbase.markets.includes("usdc")');
        assert.equal(run('marketSel.value'), available ? 'usdc' : 'spot');
        assert.match(run('sourceSeriesEl.textContent'), available ? /independent.*series/i : /separate/i);
        run('applyPrefs({exchange:"coinbase",market:"unknown"})');
        assert.equal(run('marketSel.value'),'spot');
    }
});

const instrument = (exchange, market, symbol) => {
    const quote = exchange === 'coinbase' ? (market === 'spot' ? 'USD' : 'USDC') : market.endsWith('_usdc') ? 'USDC' : market === 'coinm' ? 'USD' : 'USDT';
    const category = ['spot', 'spot_usdc'].includes(market) ? 'spot' : 'contract';
    return {exchange, market, symbol, native_symbol: exchange === 'binance' ? (market === 'coinm' ? `${symbol}USD_PERP` : `${symbol}${quote}`) : market === 'spot' ? `${symbol}-USD` : `${symbol}_USDC-PERPETUAL`,
        category, contract_kind: category === 'spot' ? null : market === 'coinm' ? 'inverse' : 'linear', quote_asset: quote,
        settlement_asset: category === 'spot' ? null : market === 'coinm' ? symbol : quote};
};
test('only validated producer instruments expose new combinations; old metadata stays legacy-only', async () => {
    const payload = {spot_series:'spot-v1', contract_series:'contracts-drb-v1', exchanges:[{exchange:'binance', markets:['spot','usdm','spot_usdc','usdm_usdc'],symbols:['BTC','ETH']}],
        instruments:[instrument('binance','spot','BTC'), instrument('binance','usdm','BTC'), instrument('binance','spot_usdc','BTC'), instrument('binance','usdm_usdc','ETH'),
            instrument('coinbase','spot_usdc','BTC'), {...instrument('binance','spot_usdc','ETH'), quote_asset:'USDT'}]};
    const {run} = prefsHarness(payload); await run('loadMeta()');
    assert.equal(run('exchangeMeta.binance.markets.includes("spot_usdc")'),true);
    run('applyPrefs({exchange:"binance",market:"spot_usdc",symbol:"BTC"})');
    assert.equal(run('marketSel.value'),'spot_usdc');
    assert.match(run('marketSel.innerHTML'), /value="spot_usdc">USDC spot<\/option>/);
    assert.match(run('marketSel.innerHTML'), /value="usdm">USDT linear perpetual<\/option>/);
    assert.match(run('sourceSeriesEl.textContent'), /USDC spot/);
    run('symbolSel.value="ETH";updateOptionsForSymbol()');
    assert.match(run('marketSel.innerHTML'), /value="usdm_usdc">USDC linear perpetual<\/option>/);
    assert.equal(run('marketSel.value'),'usdm_usdc', 'invalid market/symbol pairing cannot be constructed');
    run('applyPrefs({exchange:"binance",market:"usdm_usdc",symbol:"ETH"})');
    assert.equal(run('marketSel.value'),'usdm_usdc');
    assert.match(run('sourceSeriesEl.textContent'), /USDC linear perpetual/);
    for (const meta of [{...payload, instruments:undefined}, {...payload, spot_series:'stale'}, {...payload, instruments:[{...instrument('binance','spot_usdc','BTC'),native_symbol:'BTCUSDT'}]}]) {
        const old = prefsHarness(meta); await old.run('loadMeta()');
        assert.equal(old.run('exchangeMeta.binance.markets.includes("spot_usdc")'),false);
    }
});

test('new spot uses spot-v1 and refuses absent series and mismatched source identity', async () => {
    const {run,requests,sockets} = harness();
    run('marketSel.value="spot_usdc"; availableInstruments=[...availableInstruments,SOURCE_INSTRUMENTS["binance:spot_usdc:BTC"]]');
    let pending=run('fetchHistory(2)');
    assert.equal(requests.at(-1).url.searchParams.get('series'),'spot-v1');
    respond(requests.at(-1),[{time:60}],{});
    await assert.rejects(pending,/series/i);
    pending=run('fetchHistory(2)');
    respond(requests.at(-1),[{time:60}],{series:'spot-v1',exchange:'binance',market:'spot'});
    await assert.rejects(pending,/source/i);
    run('connectWs()');
    assert.equal(new URL(sockets[0].url).searchParams.get('series'),'spot-v1');
    sockets[0].onmessage({data:JSON.stringify({series:'spot-v1',market:'spot',bar:{time:60}})});
    assert.equal(sockets[0].readyState,3);
    assert.equal(run('page.bars.length'),0);
});

test('new-source empty history is labeled unknown, never complete or zero', () => {
    const {run}=harness();
    run('availableInstruments=[...availableInstruments,SOURCE_INSTRUMENTS["binance:usdm_usdc:BTC"]]; marketSel.value="usdm_usdc"; applyHistoryPage({bars:[],has_more_before:false,has_more_after:false,realtime_enabled:true})');
    assert.equal(run('statusEl.textContent'),'no stored history yet');
    assert.equal(run('page.bars.length'),0);
    run('applyRealtimePayload({series:"contracts-drb-v1",bar:{time:60,cvd_usdt:5}})');
    assert.equal(run('statusEl.textContent'),'live · history starting');
});

test('USDT→USDC switch fences late same-series history and old socket frames; delayed poll stays on USDC', async () => {
    const {run,requests,sockets} = harness();
    run('availableInstruments=[...availableInstruments,SOURCE_INSTRUMENTS["binance:spot_usdc:BTC"]]');
    const old = run('reload()');
    run('connectWs()');
    const oldSocket = sockets.at(-1);
    run('marketSel.value="spot_usdc"');
    const current = run('reload()');
    assert.equal(requests[0].options.signal.aborted,true);
    assert.equal(oldSocket.readyState,3);
    assert.equal(requests[1].url.searchParams.get('market'),'spot_usdc');
    assert.equal(requests[1].url.searchParams.get('series'),'spot-v1');
    respond(requests[1],[{time:60,cvd_usdt:7}],{series:'spot-v1'});
    await current;
    respond(requests[0],[{time:60,cvd_usdt:900}],{series:'spot-v1'});
    await old;
    oldSocket.onmessage({data:JSON.stringify({series:'spot-v1',bar:{time:120,cvd_usdt:999}})});
    assert.equal(run('page.bars[0].cvd_usdt'),7);
    assert.equal(run('page.bars.length'),1);
    run('realtimeEnabled=false');
    const poll=run('pollDelayedUpdates()');
    assert.equal(requests[2].url.searchParams.get('market'),'spot_usdc');
    assert.equal(requests[2].url.searchParams.get('series'),'spot-v1');
    respond(requests[2],[],{series:'spot-v1',realtime_enabled:false});
    await poll;
});

test('unsupported venue/market pair is refused before opening HTTP or WS', async () => {
    const {run,requests,sockets}=harness();
    run('exchangeSel.value="coinbase"; marketSel.value="spot_usdc"');
    await assert.rejects(run('fetchHistory(2)'), /Unsupported source/);
    assert.throws(() => run('connectWs()'), /Unsupported source/);
    assert.equal(requests.length,0);
    assert.equal(sockets.length,0);
});

test('contract history URL carries series and refuses wrong or missing responses before chart changes', async () => {
    const {run,requests} = harness();
    run('marketSel.value="usdc"; exchangeSel.value="coinbase"');
    for (const series of [undefined, 'spot-v1', 'old']) {
        const pending = run('fetchHistory(2)');
        assert.equal(requests.at(-1).url.searchParams.get('series'),'contracts-drb-v1');
        respond(requests.at(-1), [{time:60,cvd_usdt:100}], {series});
        await assert.rejects(pending, /series/i);
        assert.equal(run('page.bars.length'),0);
    }
    const pending = run('fetchHistory(2)');
    respond(requests.at(-1), [{time:60}], {series:'contracts-drb-v1'});
    assert.equal((await pending).bars[0].time,60);
});

test('legacy spot payload works but contradictory series fails in HTTP and realtime', async () => {
    const {run,requests} = harness();
    let pending = run('fetchHistory(2)'); respond(requests.at(-1), [{time:60}]);
    assert.equal((await pending).bars[0].time,60);
    pending = run('fetchHistory(2)'); respond(requests.at(-1), [{time:120}], {series:'contracts-drb-v1'});
    await assert.rejects(pending,/series/i);
    assert.throws(() => run('applyRealtimePayload({series:"contracts-drb-v1",bar:{time:60}})'),/series/i);
    assert.equal(run('page.bars.length'),0);
    run('applyRealtimePayload({bar:{time:60}})');
    assert.equal(run('page.bars.length'),1);
});

test('source change aborts old HTTP and socket; wrong-series WS never buffers or paints', async () => {
    const {run,requests,sockets} = harness();
    const old = run('reload()');
    run('marketSel.value="usdc"; exchangeSel.value="coinbase"');
    const next = run('reload()');
    assert.equal(requests[0].options.signal.aborted,true);
    respond(requests[1],[{time:60}],{series:'contracts-drb-v1'});
    await next; respond(requests[0],[{time:30}]); await old;
    assert.equal(run('page.bars[0].time'),60);
    run('connectWs()');
    assert.equal(new URL(sockets[0].url).searchParams.get('series'),'contracts-drb-v1');
    sockets[0].onmessage({data:JSON.stringify({bar:{time:120}})});
    assert.equal(sockets[0].readyState,3);
    assert.equal(run('page.bars.length'),1);
});


test('socket source switch closes old feed and rejects wrong series during catchup', async () => {
    const {run,requests,sockets} = harness();
    run('connectWs()');
    sockets[0].onmessage({data:JSON.stringify({series:'contracts-drb-v1',bar:{time:30}})});
    assert.equal(sockets[0].readyState,3);
    run('connectWs()');
    const previous = sockets[1];
    run('marketSel.value="usdc"; exchangeSel.value="coinbase"; connectWs()');
    assert.equal(previous.readyState,3);
    const opened = sockets[2].onopen();
    sockets[2].onmessage({data:JSON.stringify({bar:{time:60}})});
    assert.equal(sockets[2].readyState,3);
    respond(requests[0],[],{series:'contracts-drb-v1'});
    await opened;
    assert.equal(run('page.bars.length'),0);
});

test('history merge touches only the changed suffix yet stays correct for prepend, gaps and out-of-order corrections', () => {
    const { run } = harness()
    run('page.bars = Array.from({length:16000}, (_, index) => ({time:(index+1)*60, price_close:1})); page.newestTime=960000')
    run('globalThis.original = page.bars; for (const bar of page.bars) { const time=bar.time;Object.defineProperty(bar,"time",{enumerable:true,get(){timeReads++;return time}}) }')
    run('timeReads = 0; globalThis.same = mergeHistoryBars([{time:960000, price_close:1}])')
    assert.equal(run('same === original'), true, 'identical page keeps array identity')
    run('timeReads = 0; globalThis.tail = mergeHistoryBars([{time:960060,price_close:5},{time:959940,price_close:2}])')
    assert.ok(run('timeReads') < 200, `tiny changed suffix avoids whole-cache Map/sort (${run('timeReads')} time reads)`)
    assert.equal(run('tail !== original'), true, 'changed page is a new reference')
    assert.equal(run('tail.length'), 16001)
    assert.equal(run('original.length'), 16000, 'cached array is not mutated in place')
    assert.equal(run('tail[15998].price_close'), 2)
    assert.equal(run('tail.at(-1).time'), 960060)
    assert.equal(run('tail.slice(0, 15998).every((bar, index) => bar === original[index])'), true, 'untouched prefix keeps bar identity')
    run('page.bars = tail')
    const merged = run('JSON.stringify(mergeHistoryBars([{time:30,price_close:9},{time:6000,price_close:8},{time:90,price_close:7},{time:90,price_close:6},{time:960090,price_close:4}]).map(b=>[b.time,b.price_close]).filter(([t])=>t<=120||t===6000||t>=960060))')
    assert.equal(merged, JSON.stringify([[30,9],[60,1],[90,6],[120,1],[6000,8],[960060,5],[960090,4]]), 'prepend, gap insert, duplicate-last-wins and append')
    const sorted = run('(() => { const out = mergeHistoryBars([{time:6030,price_close:3},{time:45,price_close:3}]); return out.every((b,i)=>i===0||out[i-1].time<b.time) && out.length === 16003 })()')
    assert.equal(sorted, true)
})

test('producer-derived instruments fixture matches frontend descriptors exactly (A types.rs INSTRUMENTS)', async () => {
    const { SOURCE_INSTRUMENTS, validInstrument, sourceSeries } = await import('../static/js/config.js')
    const produced = JSON.parse(fs.readFileSync(require('node:path').join(__dirname, 'fixtures/usdc-producer-instruments.json'), 'utf8'))
    assert.equal(produced.length, Object.keys(SOURCE_INSTRUMENTS).length)
    for (const item of produced) assert.ok(validInstrument(item), JSON.stringify(item))
    const usdcSpot = produced.filter(i => i.market === 'spot_usdc')
    assert.deepEqual(usdcSpot.map(i => [i.category, i.contract_kind, i.quote_asset, sourceSeries(i.market)]),
        [['spot', null, 'USDC', 'spot-v1'], ['spot', null, 'USDC', 'spot-v1']])
    assert.ok(produced.filter(i => i.market === 'usdm_usdc').every(i => i.category === 'contract' && i.settlement_asset === 'USDC' && sourceSeries(i.market) === 'contracts-drb-v1'))
    assert.ok(!produced.some(i => i.exchange === 'bybit' || (i.exchange === 'coinbase' && i.market === 'usdm')))
    for (const field of ['quote_asset', 'native_symbol', 'category'])
        assert.ok(!validInstrument({ ...produced.find(i => i.market === 'usdm_usdc'), [field]: 'USDT' }), field)
})

// ---- V2 validity: producer invalid buckets at the browser boundary (harness: real app.js sections, mocked chart) ----
const times = run => JSON.parse(run('JSON.stringify(page.bars.map(b => b.time))'))
const poison = run => JSON.parse(run('JSON.stringify([...invalidPoison.keys()].sort((a,b)=>a-b))'))

test('validity: HTTP invalid_times poisons and removes the cached bucket; neither a stale NOR a later-started unmarked page resurrects it; only a revision-bound valid mark repairs', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60,v:1},{time:120,v:2},{time:180,v:3}])')
    const older = run('fetchHistory(2, null, 0)')          // request A starts BEFORE the invalidation
    const p = run('fetchHistory(2, null, 59)')             // request B reports 120 invalid
    respond(requests[1], [{ time: 180, v: 3 }], { invalid_times: [120] })
    await p
    assert.deepEqual(times(run), [60, 180], 'invalid bucket removed, not zero-filled')
    assert.deepEqual(poison(run), [120])
    respond(requests[0], [{ time: 120, v: 'stale' }, { time: 180, v: 3 }])
    const a = await older
    assert.deepEqual(a.bars.map(b => b.time), [180], 'stale earlier-started page row for 120 dropped')
    assert.deepEqual(poison(run), [120])
    // NEGATIVE control (retired request-order semantics): a page that merely
    // STARTED later carries no revision proof; the DB/outbox may lag.
    const newer = run('fetchHistory(2, null, 59)')
    respond(requests[2], [{ time: 120, v: 'unmarked' }])
    const c = await newer
    assert.deepEqual(c.bars.map(b => b.time), [], 'unmarked later-started row cannot repair')
    assert.deepEqual(poison(run), [120])
    // POSITIVE: a revision-bound valid mark (producer overlay) repairs.
    const marked = run('fetchHistory(2, null, 59)')
    respond(requests[3], [{ time: 120, v: 'fixed' }], { epoch: 7, validity: [{ time: 120, rev: 4, valid: true }] })
    const d = await marked
    assert.deepEqual(d.bars.map(b => b.v), ['fixed'])
    assert.deepEqual(poison(run), [], 'revision-bound valid mark repairs')
    // No resurrection: the stale invalid observation (rev 0 DB fact) replayed after the repair stays fenced.
    const replay = run('fetchHistory(2, null, 59)')
    respond(requests[4], [], { epoch: 7, invalid_times: [120] })
    await replay
    assert.deepEqual(poison(run), [], 'DB-only invalid fact older than the proven repair is fenced')
})

test('validity: WS invalidate removes the bucket, older-rev frames stay fenced, newer-rev valid repair re-inserts (interior + tail)', () => {
    const { run } = harness()
    run('setBars([{time:60,v:1},{time:120,v:2},{time:180,v:3}])')
    run('applyRealtimePayload({rev:10, action:"invalidate", invalid_time:120})')
    assert.deepEqual(times(run), [60, 180])
    assert.deepEqual(poison(run), [120])
    run('applyRealtimePayload({rev:9, action:"replace_last", bar:{time:120,v:"old"}})')
    assert.deepEqual(times(run), [60, 180], 'stale pre-invalidation frame cannot resurrect')
    run('applyRealtimePayload({rev:10, action:"invalidate", invalid_time:120})')
    assert.deepEqual(poison(run), [120], 'duplicate invalidation idempotent')
    run('applyRealtimePayload({rev:11, action:"append", bar:{time:120,v:"fixed"}})')
    assert.deepEqual(times(run), [60, 120, 180])
    assert.equal(run('page.bars[1].v'), 'fixed')
    assert.deepEqual(poison(run), [])
    // tail bucket invalidated then repaired
    run('applyRealtimePayload({rev:12, action:"invalidate", invalid_time:180})')
    assert.deepEqual(times(run), [60, 120])
    run('applyRealtimePayload({rev:13, action:"replace_last", bar:{time:180,v:"tail"}})')
    assert.deepEqual(times(run), [60, 120, 180])
    assert.equal(run('page.bars[2].v'), 'tail')
    assert.throws(() => run('applyRealtimePayload({rev:14, action:"invalidate", invalid_time:"x"})'))
})

test('validity: reconnect alone (wsGeneration) is NOT a producer restart; only a new connection delivering a new epoch may repair an old-counter poison', () => {
    const { run } = harness()
    run('setBars([{time:60},{time:120}])')
    run('adoptConnectionEpoch({epoch:11}, 1); applyRealtimePayload({epoch:11, rev:500, action:"invalidate", invalid_time:120})')
    // NEGATIVE: same producer, reconnected; a rev-3 frame is OLDER than rev 500.
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:11}, 2)')
    run('applyRealtimePayload({epoch:11, rev:3, action:"append", bar:{time:120,v:"stale"}})')
    assert.deepEqual(times(run), [60], 'reconnect without new epoch cannot repair')
    assert.deepEqual(poison(run), [120])
    // POSITIVE: a newer connection delivers a new producer epoch; its rev>0 repairs.
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:22}, 3)')
    assert.equal(run('producerEpoch'), 22)
    run('applyRealtimePayload({epoch:22, rev:3, action:"append", bar:{time:120,v:"after-restart"}})')
    assert.deepEqual(times(run), [60, 120])
    assert.deepEqual(poison(run), [])
})

test('validity H1: a delayed OLD-epoch observation neither rolls back the incarnation nor poisons/repairs (HTTP and WS)', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60},{time:120},{time:180}])')
    run('adoptConnectionEpoch({epoch:11}, 1)')
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:22}, 2)')          // producer restarted
    run('applyRealtimePayload({epoch:22, rev:5, action:"invalidate", invalid_time:180})')
    // delayed old-epoch HTTP page: invalid mark with a numerically larger rev, and a valid "repair" of 180
    const old = run('fetchHistory(2, null, 59)')
    respond(requests[0], [{ time: 120, v: 'x' }, { time: 180, v: 'old-epoch' }], { epoch: 11, validity: [{ time: 120, rev: 999, valid: false }, { time: 180, rev: 999, valid: true }], invalid_times: [120] })
    const a = await old.catch(e => e)
    assert.equal(run('producerEpoch'), 22, 'incarnation not rolled back')
    assert.deepEqual(poison(run), [180], 'old-epoch invalid does not poison; old-epoch valid does not repair')
    assert.equal(a.foreignEpoch, true, "foreign page is rejected whole before any merge (no rows, flags or cursors)")
    // delayed old-epoch WS-shaped frame (e.g. buffered) also cannot repair
    run('applyRealtimePayload({epoch:11, rev:999, action:"append", bar:{time:180,v:"old"}})')
    assert.deepEqual(poison(run), [180])
    run('applyRealtimePayload({epoch:11, rev:999, action:"invalidate", invalid_time:60})')
    assert.deepEqual(poison(run), [180], 'foreign invalidation records nothing')
    run('applyRealtimePayload({epoch:22, rev:6, action:"replace_last", bar:{time:180,v:"repair"}})')
    assert.deepEqual(poison(run), [], 'current-epoch newer rev repairs')
})

test('validity H1: every proven valid observation fences older invalid observations (same epoch, with and without prior poison; previous epoch)', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60},{time:120},{time:180}])')
    run('adoptConnectionEpoch({epoch:7}, 1)')
    // no poison ever: valid rev12 observed live
    run('applyRealtimePayload({epoch:7, rev:12, action:"replace_last", bar:{time:120,v:"r12"}})')
    let p = run('fetchHistory(2, null, 59)')
    respond(requests[0], [], { epoch: 7, validity: [{ time: 120, rev: 10, valid: false }] })
    await p
    assert.deepEqual(poison(run), [], 'delayed invalid rev10 after valid rev12 is fenced without prior poison')
    assert.ok(times(run).includes(120))
    // after poison deletion: poison rev5, repair rev12, then delayed invalid rev10
    run('applyRealtimePayload({epoch:7, rev:5, action:"invalidate", invalid_time:180})')
    p = run('fetchHistory(2, null, 59)')
    respond(requests[1], [{ time: 180, v: 'r12' }], { epoch: 7, validity: [{ time: 180, rev: 12, valid: true }] })
    await p
    assert.deepEqual(poison(run), [])
    run('applyRealtimePayload({epoch:7, rev:10, action:"invalidate", invalid_time:180})')
    assert.deepEqual(poison(run), [], 'repair proof survives poison deletion')
    // previous-epoch delayed invalid after restart does not poison either
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:8}, 2)')
    run('applyRealtimePayload({epoch:7, rev:50, action:"invalidate", invalid_time:120})')
    assert.deepEqual(poison(run), [])
    // a genuinely newer invalidation still applies
    run('applyRealtimePayload({epoch:8, rev:1, action:"invalidate", invalid_time:120})')
    assert.deepEqual(poison(run), [120])
})

const vOf = (run, t) => run(`page.bars.find(b => b.time === ${t})?.v ?? null`)
test('admission A1: a held OLD-epoch VALID page row never overwrites a current-epoch row, nor fakes exhaustion', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60,v:"a"},{time:120,v:"cur-120"}])')
    run('adoptConnectionEpoch({epoch:11}, 1)')
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:22}, 2)')
    run('applyRealtimePayload({epoch:22, rev:3, action:"replace_last", bar:{time:120,v:"new-120"}})')
    const p = run('fetchHistory(2, null, 59)')
    respond(requests[0], [{ time: 60, v: 'old-60' }, { time: 120, v: 'old-120' }], { epoch: 11, has_more_before: false, has_more_after: false })
    const data = await p.catch(e => e)
    assert.equal(data.foreignEpoch, true, "foreign page rejected whole")
    assert.equal(data.name, "AbortError", "never surfaced as a page: no rows and no exhaustion flags applied")
    assert.equal(run('page.reachedStart'), false, 'no false exhaustion after applying the foreign page')
    assert.equal(vOf(run, 120), 'new-120')
    assert.equal(vOf(run, 60), 'a')
})

test('admission A2: a delayed OLDER same-epoch valid row (WS rev or unmarked DB row) cannot overwrite a newer proven row', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60,v:"a"},{time:120,v:"seed"}])')
    run('adoptConnectionEpoch({epoch:7}, 1)')
    run('applyRealtimePayload({epoch:7, rev:12, action:"replace_last", bar:{time:120,v:"r12"}})')
    run('applyRealtimePayload({epoch:7, rev:10, action:"replace_last", bar:{time:120,v:"r10"}})')
    assert.equal(vOf(run, 120), 'r12', 'older live rev rejected')
    const p = run('fetchHistory(2, null, 59)')
    respond(requests[0], [{ time: 120, v: 'db-lag' }, { time: 60, v: 'a2' }], { epoch: 7 })
    const data = await p
    assert.deepEqual(data.bars.map(b => [b.time, b.v]), [[60, 'a2']], 'unmarked lagging DB row cannot overwrite proven r12')
    run('applyRealtimePayload({epoch:7, rev:13, action:"replace_last", bar:{time:120,v:"r13"}})')
    assert.equal(vOf(run, 120), 'r13', 'genuinely newer row still admitted')
})

test('admission A3: a foreign-epoch invalidation neither deletes a current valid row nor poisons', () => {
    const { run } = harness()
    run('setBars([{time:60,v:"a"},{time:120,v:"cur"}])')
    run('adoptConnectionEpoch({epoch:11}, 1)')
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:22}, 2)')
    run('applyRealtimePayload({epoch:22, rev:4, action:"replace_last", bar:{time:120,v:"r4"}})')
    run('applyRealtimePayload({epoch:11, rev:99, action:"invalidate", invalid_time:120})')
    assert.deepEqual(times(run), [60, 120], 'rejected frame cannot delete')
    assert.equal(vOf(run, 120), 'r4')
    assert.deepEqual(poison(run), [])
})

test('admission A4: OLD-epoch validAuth does not suppress a CURRENT-epoch DB invalid fact', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60,v:"a"},{time:120,v:"b"}])')
    run('adoptConnectionEpoch({epoch:11}, 1)')
    run('applyRealtimePayload({epoch:11, rev:40, action:"replace_last", bar:{time:120,v:"r40"}})')
    run('wsGeneration += 1; adoptConnectionEpoch({epoch:22}, 2)')
    const p = run('fetchHistory(2, null, 59)')
    respond(requests[0], [], { epoch: 22, invalid_times: [120] })
    await p
    assert.deepEqual(poison(run), [120], 'current-epoch DB invalid applies')
    assert.deepEqual(times(run), [60])
})

test('withdrawal B1: invalidation withholds every enabled-plugin output at/after the bucket (context is warm-up, not an influence bound), keeps earlier ones', () => {
    const ctx = vm.createContext({ Map, Math, page: { bars: Array.from({ length: 200 }, (_, i) => ({ time: (i + 1) * 60 })) },
        localValues: new Map(), localLive: new Map(), localRecords: new Map() })
    vm.runInContext(section('function invalidateLocalAt', 'function syncLocalCompute'), ctx)
    const run = code => vm.runInContext(code, ctx)
    run('localRecords.set("p", {id:"p", enabled:true, descriptor:{context:{bars:50, per_tf:{"1m":120}}}})')
    run('localValues.set("p/sum|history", new Map(page.bars.map(b => [b.time, b.time])))')
    run('localValues.set("q/sum|history", new Map(page.bars.map(b => [b.time, b.time])))')
    run('invalidateLocalAt([60*20])')
    const kept = JSON.parse(run('JSON.stringify([...localValues.get("p/sum|history").keys()])'))
    assert.equal(kept.length, 19, 'outputs before the bucket kept')
    assert.ok(kept.every(t => t < 1200))
    assert.equal(run('localValues.get("q/sum|history").size'), 199, 'unregistered plugin loses only the invalid bucket itself')
})

const flush = () => new Promise(resolve => setImmediate(resolve))
test('validity P1 cursors: all-invalid pages advance by truthful coverage in loadMoreBefore/loadMoreAfter/pollDelayedUpdates without false exhaustion', async () => {
    const { run, requests } = harness()
    run('setBars([{time:600},{time:660}]); page.confirmedTime = 660')
    // older: all-invalid page [480,540] with more before -> next request before=480 -> valid row 420
    let pending = run('loadMoreBefore()')
    respond(requests[0], [], { has_more_before: true, invalid_times: [540, 480], coverage_from: 480, coverage_to: 599 })
    await flush()
    assert.match(String(requests[1].url), /before=480/, 'cursor advanced by coverage_from, valid look-ahead not skipped')
    respond(requests[1], [{ time: 420 }], { has_more_before: false, coverage_to: 479 })
    await pending
    assert.deepEqual(times(run), [420, 600, 660])
    assert.equal(run('page.reachedStart'), true)
    // non-advancing coverage is an error, not a loop or exhaustion
    run('page.reachedStart = false')
    pending = run('loadMoreBefore()')
    respond(requests[2], [], { has_more_before: true, coverage_from: 420 })
    await pending
    assert.equal(run('page.reachedStart'), false, 'no false exhaustion')
    assert.equal(requests.length, 3, 'no infinite request loop')
    // newer: loadMoreAfter steps over [720,780] then gets 840
    pending = run('loadMoreAfter()')
    respond(requests[3], [], { has_more_after: true, invalid_times: [720, 780], coverage_from: 661, coverage_to: 780 })
    await flush()
    assert.match(String(requests[4].url), /after=780/)
    respond(requests[4], [{ time: 840 }], { has_more_after: false })
    await pending
    assert.ok(times(run).includes(840))
    // delayed poll: all-invalid tail with has_more_after -> continues; persistent cursor stays at the bar edge
    pending = run('pollDelayedUpdates()')
    assert.match(String(requests[5].url), /after=840/)
    respond(requests[5], [], { has_more_after: true, invalid_times: [900], coverage_to: 900 })
    await flush()
    assert.match(String(requests[6].url), /after=900/)
    respond(requests[6], [], { has_more_after: false })
    await pending
    assert.equal(run('page.newestTime'), 840, 'cursor not persisted past invalid buckets (later repair still fetched)')
})

test('validity: buffered invalidation during catch-up applies even at or below the closed cursor', async () => {
    const { run, requests } = harness()
    run('setBars([{time:60},{time:120}]); page.confirmedTime = 120')
    const pending = run('catchUpHistory(wsGeneration, new Map([[120, {rev:7, action:"invalidate", invalid_time:120}]]))')
    respond(requests[0], [{ time: 120, v: 'page-predates-invalidation' }])
    assert.equal(await pending, true)
    assert.deepEqual(times(run), [60])
    assert.deepEqual(poison(run), [120])
})

test('validity: reload resets poison state (scope-bound)', () => {
    const { run } = harness()
    run('setBars([{time:60},{time:120}])')
    run('applyRealtimePayload({rev:2, action:"invalidate", invalid_time:120})')
    run('resetValidity()')
    assert.deepEqual(poison(run), [])
})
