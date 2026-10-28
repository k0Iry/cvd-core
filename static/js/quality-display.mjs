// Presentation-only interpretation of the backend's source-observation (quality) contract.
const STALE_MS = 60_000;
const OBSERVED_INPUTS = ['po','ph','pl','price_close','cvd_usdt','delta_usdt',
    'apo','aph','apl','apc','ac','ad','bvb','abvb','svb','asvb',
    'buy_volume_usdt','agb','sell_volume_usdt','ags','trades'];
export function mergeHistoryQuality(existing, incoming) {
    if (!existing || !incoming || Object.hasOwn(incoming, 'quality') ||
        !Number.isSafeInteger(existing.time) || existing.time <= 0 || existing.time !== incoming.time) return incoming;
    const q = existing.quality;
    if (!q || typeof q !== 'object' || Array.isArray(q) ||
        !Number.isFinite(q.observed_at_ms) || q.observed_at_ms <= 0 || q.observed_at_ms > 8.64e15 ||
        q.coverage !== 'unverified' || !Array.isArray(q.sources) || !q.sources.length) return incoming;
    if (!OBSERVED_INPUTS.every(key => {
        const a = existing[key], b = incoming[key];
        return typeof a === 'number' && typeof b === 'number' &&
            Number.isFinite(a) && Number.isFinite(b) &&
            Math.abs(a) <= Number.MAX_SAFE_INTEGER && Math.abs(b) <= Number.MAX_SAFE_INTEGER &&
            (key !== 'trades' || (Number.isSafeInteger(a) && Number.isSafeInteger(b) && a >= 0 && b >= 0)) && a === b;
    })) return incoming;
    return {...incoming, quality: {...q, sources: q.sources.map(source => ({...source})), retained: true}};
}
const retainedNote = q => q.retained === true ? 'Original real-time observation retained across identical history inputs, not a new historical completeness audit; coverage remains unverified. ' : '';
const stamp = ms => Number.isFinite(ms) && ms > 0 && ms <= 8.64e15 ? new Date(ms).toISOString() : 'unknown';
export function qualityDisplay(bar, aggregate, now = Date.now(), latest = true, scope = null) {
    // Only a matching source row may describe a selected venue. Aggregate
    // participation counts never attest selected-feed completeness.
    const q = bar?.quality;
    if (!q || !Number.isFinite(q.observed_at_ms) || q.observed_at_ms <= 0 || q.observed_at_ms > 8.64e15 || !Array.isArray(q.sources) || !q.sources.length || q.coverage !== 'unverified') {
        return { label: 'Quality unknown', tooltip: 'No retained source evidence for this bar; coverage unknown.' };
    }
    const entries = q.sources.filter(s => s && typeof s === 'object');
    if (!aggregate) {
        const source = entries.find(s => s.exchange === scope?.exchange && s.market === scope?.market);
        if (!source) return { label: 'Quality unknown', tooltip: 'No matching source observation for this selection; coverage unknown.' };
        const stale = (latest || q.retained === true) && (!Number.isFinite(now) || now - q.observed_at_ms > STALE_MS ||
            !Number.isFinite(q.status_observed_at_ms) || now - q.status_observed_at_ms > STALE_MS);
        const missing = source.contribution === 'missing' || source.contribution === 'carried';
        const recovering = ['starting', 'recovering', 'needs_backfill', 'unavailable'].includes(source.recovery);
        const label = missing ? 'Source missing' : recovering ? 'Source recovering' : source.recovery !== 'live' ? 'Source unknown' : 'Source ·';
        return { label: stale ? (label === 'Source ·' ? 'Source stale' : `${label} · stale`) : label,
            tooltip: `${retainedNote(q)}${stale ? (q.retained === true ? 'Retained observation stale. ' : 'Latest snapshot stale. ') : ''}${source.exchange} ${source.market}: ${source.contribution || 'unknown'}, recovery ${source.recovery || 'unknown'}; bar ${source.bar_time ?? 'unknown'}, last trade ${stamp(source.last_trade_ms)}. Inputs observed ${stamp(q.observed_at_ms)}; status observed ${stamp(q.status_observed_at_ms)}. Aggregate delivery observation only, not proof of selected-feed completeness; ${q.bar_state === 'elapsed' ? 'elapsed is not final' : 'bar forming'}.` };
    }
    const sources = entries.map(s => `${s.exchange || '?'} ${s.market || '?'}: ${s.contribution || 'unknown'}, recovery ${s.recovery || 'unknown'} (bar ${s.bar_time ?? 'unknown'}, last trade ${stamp(s.last_trade_ms)})`);
    const stale = (latest || q.retained === true) && (!Number.isFinite(now) || now - q.observed_at_ms > STALE_MS ||
        !Number.isFinite(q.status_observed_at_ms) || now - q.status_observed_at_ms > STALE_MS);
    const missing = entries.some(s => s.contribution === 'missing' || s.contribution === 'carried');
    const recovery = entries.some(s => ['starting', 'recovering', 'needs_backfill', 'unavailable'].includes(s.recovery));
    const unknownRecovery = entries.length !== q.sources.length || entries.some(s => !['starting','live','recovering','needs_backfill','unavailable'].includes(s.recovery));
    const partial = missing || recovery || (Number.isFinite(q.expected_sources) && Number.isFinite(q.present_sources) && q.present_sources < q.expected_sources);
    const label = stale ? (missing ? 'Quality missing · stale' : recovery ? 'Quality recovering · stale' : 'Quality stale')
        : missing ? 'Quality missing' : recovery ? 'Quality recovering' : partial ? 'Quality partial' : unknownRecovery ? 'Quality unknown' : 'Quality ·';
    return { label, tooltip: `${retainedNote(q)}${stale ? (q.retained === true ? 'Retained observation stale. ' : 'Latest snapshot stale. ') : ''}${q.bar_state === 'elapsed' ? 'Elapsed, not final. ' : 'Forming. '}Coverage ${q.coverage || 'unverified'}; ${q.present_sources ?? '?'} / ${q.expected_sources ?? '?'} same-bucket participants, not completeness. Inputs observed ${stamp(q.observed_at_ms)}; recovery status observed ${stamp(q.status_observed_at_ms)}. ${sources.join('; ')}` };
}
