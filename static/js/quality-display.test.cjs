const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const context = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, 'chart.js'), 'utf8').replace(/^export /gm, ''), context);
const call = (name, ...args) => context[name](...args);
test('empty quality evidence stays unknown', () => {
  assert.match(call('qualityDisplay', {time:60,quality:{observed_at_ms:100000,status_observed_at_ms:100000,bar_state:'elapsed',coverage:'unverified',expected_sources:0,present_sources:0,sources:[]}},true,100001,true).label,/unknown/i);
});
test('quality uses per-bar observation, distinguishes stale, recovery and no evidence', () => {
  const q = { observed_at_ms: 100000, status_observed_at_ms: 100000, bar_state:'elapsed', coverage:'unverified', expected_sources:2, present_sources:1,
    sources:[{exchange:'binance',market:'spot',bar_time:60,contribution:'same_bucket',recovery:'recovering',last_trade_ms:null},{exchange:'coinbase',market:'spot',bar_time:null,contribution:'missing',recovery:'needs_backfill',last_trade_ms:null}] };
  const partial = call('qualityDisplay', {time:60,quality:q}, true, 101000, true);
  assert.match(partial.label, /missing|backfill|recovering/i);
  assert.match(partial.tooltip, /not completeness/i);
  assert.match(partial.tooltip, /elapsed.*not final/i);
  assert.match(partial.tooltip, /coinbase/i);
  const stale = call('qualityDisplay', {time:60,quality:{...q, present_sources:2, sources:q.sources.map(s=>({...s,contribution:'same_bucket',recovery:'live'}))}}, true, 200000, true);
  assert.match(stale.label, /stale/i);
  assert.match(call('qualityDisplay',{time:60,quality:q},true,200000,false).tooltip,/observed/i);
  assert.match(call('qualityDisplay',{time:60},true,200000,true).label,/unknown|history/i);
  assert.match(call('qualityDisplay',{time:60,quality:q},false,101000,true).label,/unknown/i);
  assert.equal(call('qualityDisplay',{time:60,quality:q},false,101000,true,{exchange:'coinbase',market:'spot'}).label,'Source missing');
  assert.equal(call('qualityDisplay',{time:60,quality:q},false,101000,true,{exchange:'binance',market:'spot'}).label,'Source recovering');
  assert.equal(call('qualityDisplay',{time:60,quality:q},false,101000,true,{exchange:'coinbase',market:'usdm'}).label,'Quality unknown');
});

test('history overlap retains only matching price/volume tuple; other incoming fields win', () => {
  const fields = ['po','ph','pl','price_close','cvd_usdt','delta_usdt','apo','aph','apl','apc','ac','ad','bvb','abvb','svb','asvb','buy_volume_usdt','agb','sell_volume_usdt','ags','trades'];
  const live = {time:60, indicators:{source:{"x/y":1},aggregate:{"x/y":2}}, quality:{observed_at_ms:100000,status_observed_at_ms:null,bar_state:'forming',coverage:'unverified',sources:[{exchange:'binance',market:'spot',recovery:'live'}]}};
  fields.forEach((key, index) => { live[key] = index + 1; });
  const incoming = {...live, indicators:{source:{"x/y":3},aggregate:{"x/y":null}}}; delete incoming.quality;
  const merged = call('mergeHistoryQuality', live, incoming);
  assert.equal(merged.quality.retained, true);
  assert.equal(merged.quality.observed_at_ms, 100000);
  assert.equal(merged.quality.status_observed_at_ms, null);
  assert.deepEqual(JSON.parse(JSON.stringify(merged.quality)), {...live.quality,retained:true});
  assert.notStrictEqual(merged.quality, live.quality);
  assert.equal(live.quality.retained, undefined);
  assert.deepEqual(merged.indicators.source["x/y"], 3);
  assert.equal(merged.indicators.aggregate["x/y"], null);
  for (const key of fields) {
    assert.equal(call('mergeHistoryQuality', live, {...incoming,[key]:42}).quality, undefined, `revision of ${key}`);
    const missing = {...incoming}; delete missing[key];
    assert.equal(call('mergeHistoryQuality', live, missing).quality, undefined, `missing ${key}`);
    const missingOld = {...live}; delete missingOld[key];
    assert.equal(call('mergeHistoryQuality', missingOld, incoming).quality, undefined, `old missing ${key}`);
  }
  for (const invalid of [NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, null, '1']) {
    assert.equal(call('mergeHistoryQuality', live, {...incoming, bvb:invalid}).quality, undefined);
  }
  assert.equal(call('mergeHistoryQuality', live, {...incoming, trades:1.5}).quality, undefined);
  assert.equal(call('mergeHistoryQuality', live, {...incoming, time:120}).quality, undefined);
  assert.equal(call('mergeHistoryQuality', live, {...incoming, quality:null}).quality, null);
  const newer = {observed_at_ms:200000};
  assert.strictEqual(call('mergeHistoryQuality', live, {...incoming, quality:newer}).quality, newer);
});

test('retained tooltip identifies old live evidence and ages on original clock', () => {
  const q = {observed_at_ms:100000,status_observed_at_ms:100000,bar_state:'forming',coverage:'unverified',sources:[{exchange:'binance',market:'spot',bar_time:60,contribution:'same_bucket',recovery:'live'}],retained:true};
  for (const aggregate of [true,false]) {
    const selected = call('qualityDisplay', {time:60,quality:q}, aggregate, 101000, true, {exchange:'binance',market:'spot'});
    assert.match(selected.tooltip, /original real-time observation/i);
    assert.match(selected.tooltip, /not a new historical completeness audit/i);
    assert.match(selected.tooltip, /unverified/i);
    assert.match(call('qualityDisplay', {time:60,quality:q}, aggregate, 200000, true, {exchange:'binance',market:'spot'}).label, /stale/i);
    assert.match(call('qualityDisplay', {time:60,quality:q}, aggregate, 200000, false, {exchange:'binance',market:'spot'}).label, /stale/i);
  }
});

// ---- selected-source status (C): /health, independent of bars/quality/hover
const SCOPE = { exchange: 'coinbase', market: 'usdc', symbol: 'BTC' };
const row = (state, extra = {}) => ({ exchange: 'coinbase', market: 'usdc', symbol: 'BTC', error: null, last_trade_ms: 1_000_000,
  recovery: { state, attempts: 3, buffered_trades: 12, durable_cursor: '24738167', durable_from_ms: 900_000, detail: 'DRB archive bridge', ...extra.recovery }, ...extra.top });
const status = (h, scope = SCOPE, now = 1_010_000, f = null) => call('sourceStatusDisplay', h, scope, now, f);
test('source_needs_backfill_is_stopped_not_recovering', () => {
  const d = status({ sources: [row('needs_backfill')] });
  assert.equal(d.label, 'Source stopped · needs backfill');
  assert.doesNotMatch(d.label, /recovering/i);
  assert.match(d.tooltip, /Not recovering/);
});
test('source_recovering_progress_and_checkpoint_wording', () => {
  const a = status({ sources: [row('recovering', { top: { last_trade_ms: Date.UTC(2026, 9, 29, 14, 5) } })] });
  const b = status({ sources: [row('recovering', { top: { last_trade_ms: Date.UTC(2026, 9, 29, 14, 7) } })] });
  assert.equal(a.label, 'Source recovering · 14:05Z');
  assert.equal(b.label, 'Source recovering · 14:07Z');
  assert.match(a.tooltip, /in-memory progress, not a restart checkpoint/);
  assert.match(a.tooltip, /cursor 24738167 \(in memory\)/);
  assert.doesNotMatch(a.tooltip, /durable cursor/i);
});
test('source_health_failure_or_unknown_never_live', () => {
  for (const d of [status(null), status({}), status({ sources: [] }), status(null, SCOPE, 1, 'HTTP 503'),
    status({ sources: [row('weird')] }), status({ sources: [{ ...row('live'), recovery: null }] })]) {
    assert.equal(d.label, 'Source status unknown'); assert.equal(d.state, 'attention');
  }
  assert.equal(status({ sources: [row('live', { top: { error: 'x' } })] }).label, 'Source error');
  assert.equal(status({ sources: [row('live')] }, SCOPE, 1_000_000 + 90_000).label, 'Source live · no recent trade');
  assert.equal(status({ sources: [row('live', { top: { last_trade_ms: null } })] }).label, 'Source live · no recent trade');
});
test('source_scope_is_exact_exchange_market_symbol', () => {
  const h = { sources: [row('live'), { ...row('needs_backfill'), symbol: 'ETH' }, { ...row('recovering'), market: 'spot_usdc' }] };
  assert.equal(status(h).label, 'Source live');
  assert.equal(status(h, { ...SCOPE, symbol: 'ETH' }).label, 'Source stopped · needs backfill');
  assert.match(status(h, { ...SCOPE, market: 'spot_usdc' }).label, /^Source recovering/);
  assert.equal(status(h, { ...SCOPE, exchange: 'binance' }).label, 'Source status unknown');
});
test('source_recovery_to_live', () => {
  assert.match(status({ sources: [row('recovering')] }).label, /^Source recovering/);
  const live = status({ sources: [row('live')] });
  assert.equal(live.label, 'Source live'); assert.equal(live.state, 'quiet');
});
