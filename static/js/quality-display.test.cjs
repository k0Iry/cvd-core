const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const context = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, 'quality-display.mjs'), 'utf8').replaceAll('export function', 'function'), context);
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
