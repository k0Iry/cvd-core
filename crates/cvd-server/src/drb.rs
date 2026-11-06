//! Public, anonymous Coinbase Deribit-powered linear USDC perpetual feed.
use super::*;
use crate::ingestion::Trade;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use tokio::time::Instant;

pub(super) const BASE: &str = "https://drb.coinbase.com/api/v2/public";
const PAGE: u64 = 1000;
const MAX_PAGES: usize = 100;
const MAX_TRADES: usize = 100_000;
const DEADLINE: Duration = Duration::from_secs(30);
const ATTEMPTS: usize = 20;
/// A primary chunk stops paging this long before the attempt deadline and hands
/// back its validated prefix, so slow pages still commit progress (deadline unchanged).
const CHUNK_MARGIN: Duration = Duration::from_secs(5);
/// Deribit's official public archive of the same matching-engine tape. The
/// primary gateway only retains about 24 hours of sequences (observed, not a
/// documented guarantee), so older anchors are bridged from here in bounded,
/// sequence-exact chunks behind a per-chunk identity overlap gate.
pub(super) const ARCHIVE: &str = "https://history.deribit.com/api/v2/public";
/// Anchors older than this are bridged from the archive; the primary window is ~24h.
pub(super) const ARCHIVE_AFTER_MS: i64 = 20 * 3_600_000;
const ARCHIVE_SPAN: u64 = 900; // sequences requested per page; count=PAGE leaves edge padding
const ARCHIVE_PAGES: usize = 20; // bounded chunk (<=18k trades) per worker attempt
const ARCHIVE_PACE: Duration = Duration::from_millis(250);
const GATE_BACK: u64 = 2_000;
const GATE_LEN: u64 = 50;
pub(super) fn archive_base(base: &str) -> String {
    // Loopback tests serve the archive under `<base>/archive`.
    if base == BASE {
        ARCHIVE.into()
    } else {
        format!("{base}/archive")
    }
}

pub(super) fn product(symbol: Symbol) -> &'static str {
    match symbol {
        Symbol::Btc => "BTC_USDC-PERPETUAL",
        Symbol::Eth => "ETH_USDC-PERPETUAL",
        _ => unreachable!(),
    }
}
fn sym(product: &str) -> Option<Symbol> {
    match product {
        "BTC_USDC-PERPETUAL" => Some(Symbol::Btc),
        "ETH_USDC-PERPETUAL" => Some(Symbol::Eth),
        _ => None,
    }
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v.get(name).and_then(Value::as_str).ok_or_else(|| anyhow!("missing/invalid {name}"))
}
fn positive(v: &Value, name: &str) -> Result<f64> {
    let n = v.get(name).and_then(Value::as_f64).ok_or_else(|| anyhow!("invalid {name}"))?;
    anyhow::ensure!(n.is_finite() && n > 0.0, "invalid {name}");
    Ok(n)
}
pub(super) fn validate_instrument(v: &Value, name: &str, base: &str) -> Result<f64> {
    anyhow::ensure!(
        field(v, "instrument_name")? == name && field(v, "base_currency")? == base,
        "unexpected instrument identity"
    );
    for key in ["quote_currency", "counter_currency", "settlement_currency"] {
        anyhow::ensure!(field(v, key)? == "USDC", "unexpected {key}");
    }
    anyhow::ensure!(
        field(v, "kind")? == "future"
            && field(v, "state")? == "open"
            && field(v, "future_type")? == "linear"
            && field(v, "instrument_type")? == "linear"
            && v["is_active"] == true,
        "instrument is not active linear future"
    );
    positive(v, "contract_size")
}
pub(super) fn parse_trade(v: &Value, product: &str, contract_size: f64) -> Result<Trade> {
    anyhow::ensure!(field(v, "instrument_name")? == product, "trade instrument mismatch");
    let seq = v
        .get("trade_seq")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("invalid integer trade_seq"))?;
    // Bounded diagnostic: product/sequence only, never the payload.
    parse_fields(v, seq, contract_size).map_err(|e| anyhow!("{e} ({product} trade_seq {seq})"))
}
fn parse_fields(v: &Value, seq: u64, contract_size: f64) -> Result<Trade> {
    let ms = v
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("invalid millisecond timestamp"))?;
    let price = positive(v, "price")?;
    let base = positive(v, "amount")?;
    anyhow::ensure!(contract_size.is_finite() && contract_size > 0.0, "invalid contract_size");
    let tolerance = (base * 1e-8).max(1e-10);
    // `contracts` is documented optional and absent on some historical rows;
    // the required `amount` (base units for linear futures) is authoritative.
    // Absent: no extra quantity rule. Present (including null/non-numeric):
    // strict positive + consistency.
    if v.get("contracts").is_some() {
        let expected = positive(v, "contracts")? * contract_size;
        anyhow::ensure!(
            expected.is_finite() && (base - expected).abs() <= tolerance,
            "amount does not match contracts * contract_size"
        );
    }
    let quote = base * price;
    anyhow::ensure!(quote.is_finite() && quote > 0.0, "invalid quote");
    let side = field(v, "direction")?;
    anyhow::ensure!(side == "buy" || side == "sell", "invalid taker direction");
    let _opaque = field(v, "trade_id")?; // Never convert exchange metadata IDs via floating point.
    Ok(Trade { id: seq.to_string(), timestamp_ms: ms, price, base, quote, is_buy: side == "buy" })
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    query: &[(&str, String)],
) -> std::result::Result<Value, HistoryError> {
    let response = client
        .get(format!("{base}/{method}"))
        .query(query)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .await
        .map_err(retry)?;
    let status = response.status();
    if !status.is_success() {
        let error = format!("DRB HTTP {status}");
        return Err(
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status.is_server_error()
            {
                retry(error)
            } else {
                fatal(error)
            },
        );
    }
    let response: Value = response.json().await.map_err(|e| if e.is_decode() { fatal(e) } else { retry(e) })?;
    if response["jsonrpc"] != "2.0" {
        return Err(fatal("invalid DRB RPC version"));
    }
    if let Some(error) = response.get("error") {
        let code = error["code"].as_i64();
        let detail = format!("DRB RPC error: {error}");
        return Err(
            if code.is_some_and(|n| {
                // Deribit-powered gateways use positive vendor codes as well as JSON-RPC codes.
                // https://docs.deribit.com/articles/errors
                (-32099..=-32000).contains(&n)
                    || n == -32603
                    || matches!(n, 10028 | 10040 | 10041 | 10047 | 11051 | 11056 | 13025 | 13028)
            }) {
                retry(detail)
            } else {
                fatal(detail)
            },
        );
    }
    response.get("result").cloned().ok_or_else(|| fatal("missing DRB RPC result"))
}
pub(super) async fn metadata(
    client: &reqwest::Client,
    base: &str,
    symbol: Symbol,
) -> std::result::Result<f64, HistoryError> {
    let name = product(symbol);
    let v = get(client, base, "get_instrument", &[("instrument_name", name.into())]).await?;
    validate_instrument(
        &v,
        name,
        match symbol {
            Symbol::Btc => "BTC",
            _ => "ETH",
        },
    )
    .map_err(fatal)
}
pub(super) async fn latest(
    client: &reqwest::Client,
    base: &str,
    symbol: Symbol,
    size: f64,
) -> std::result::Result<Option<Trade>, HistoryError> {
    let v = get(
        client,
        base,
        "get_last_trades_by_instrument",
        &[("instrument_name", product(symbol).into()), ("count", "1".into()), ("sorting", "desc".into())],
    )
    .await?;
    let rows = v["trades"].as_array().ok_or_else(|| fatal("missing latest trades"))?;
    rows.first().map(|r| parse_trade(r, product(symbol), size)).transpose().map_err(fatal)
}

#[derive(Debug)]
pub(super) enum HistoryError {
    Retry(String),
    Fatal(String),
}
impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(s) | Self::Fatal(s) => write!(f, "{s}"),
        }
    }
}
fn retry(e: impl std::fmt::Display) -> HistoryError {
    HistoryError::Retry(e.to_string())
}
fn fatal(e: impl std::fmt::Display) -> HistoryError {
    HistoryError::Fatal(e.to_string())
}

// Sequence range is fixed to the first queued target throughout the entire episode.
// On a closed-minute restart a predecessor before the boundary proves the start;
// neither an empty time window nor an unbounded latest snapshot proves coverage.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn history(
    client: &reqwest::Client,
    base: &str,
    symbol: Symbol,
    size: f64,
    cursor: Option<u64>,
    from_ms: Option<i64>,
    target: u64,
    target_ms: i64,
    fresh: (u64, i64),
) -> std::result::Result<Vec<Trade>, HistoryError> {
    history_until(client, base, symbol, size, cursor, from_ms, target, target_ms, fresh, None).await
}

/// `history` whose primary paging returns the validated contiguous prefix once
/// `soft_stop` passes (at least one page is always validated first).
#[allow(clippy::too_many_arguments)]
pub(super) async fn history_until(
    client: &reqwest::Client,
    base: &str,
    symbol: Symbol,
    size: f64,
    cursor: Option<u64>,
    from_ms: Option<i64>,
    target: u64,
    target_ms: i64,
    fresh: (u64, i64),
    soft_stop: Option<Instant>,
) -> std::result::Result<Vec<Trade>, HistoryError> {
    let name = product(symbol);
    // Primary coverage/identity is judged against the freshest observed sequence
    // (`fresh`: retained WS high-water), not the aging fixed target.
    let (fresh_seq, fresh_ms) = (fresh.0.max(target), fresh.1.max(target_ms));
    if from_ms.is_some_and(|from| from >= 0 && fresh_ms >= from && fresh_ms - from > ARCHIVE_AFTER_MS) {
        let archive = archive_base(base);
        archive_identity(client, base, &archive, symbol, size, fresh_seq).await?;
        return archive_chunk(client, &archive, symbol, size, cursor, from_ms, target, fresh_ms).await;
    }
    let predecessor = if let Some(cursor) = cursor {
        cursor
    } else if let Some(from) = from_ms {
        if from < 0 || target_ms < from || target_ms - from > 86_400_000 {
            return Err(fatal("closed-minute recovery exceeds 24-hour window"));
        }
        let start = from.saturating_sub(86_400_000).max(0);
        let v = get(
            client,
            base,
            "get_last_trades_by_instrument_and_time",
            &[
                ("instrument_name", name.into()),
                ("start_timestamp", start.to_string()),
                ("end_timestamp", from.saturating_sub(1).to_string()),
                ("count", "1".into()),
                ("sorting", "desc".into()),
            ],
        )
        .await?;
        let rows = v["trades"].as_array().ok_or_else(|| fatal("invalid predecessor response"))?;
        let prior = rows.first().ok_or_else(|| fatal("no trustworthy predecessor before closed-minute boundary"))?;
        let trade = parse_trade(prior, name, size).map_err(fatal)?;
        if trade.timestamp_ms >= from {
            return Err(fatal("predecessor after recovery boundary"));
        }
        trade.id.parse::<u64>().map_err(fatal)?
    } else {
        return Err(fatal("missing recovery anchor"));
    };
    if predecessor >= target {
        return Err(fatal("recovery predecessor is not before fixed target"));
    }
    let mut next = predecessor.checked_add(1).ok_or_else(|| fatal("sequence overflow"))?;
    let mut output = Vec::new();
    let mut previous_ms = if cursor.is_some() { from_ms } else { None };
    for _ in 0..MAX_PAGES {
        if !output.is_empty() && soft_stop.is_some_and(|t| Instant::now() >= t) {
            return Ok(output); // bounded validated prefix before the attempt deadline
        }
        let params = [
            ("instrument_name", name.into()),
            ("start_seq", next.to_string()),
            ("end_seq", target.to_string()),
            ("count", PAGE.to_string()),
            ("sorting", "asc".into()),
        ];
        let page = get(client, base, "get_last_trades_by_instrument", &params);
        // An in-flight later page must not outlive the soft stop: dropping the
        // HTTP request is cancellation-safe (nothing is ingested here), and the
        // already validated prefix is returned instead of being lost with the
        // whole future at the outer deadline. The first page stays unbounded
        // here (outer deadline/client timeout bound it) since it has no prefix.
        let v = match soft_stop.filter(|_| !output.is_empty()) {
            Some(stop) => tokio::select! {
                v = page => v?,
                _ = tokio::time::sleep_until(stop) => return Ok(output),
            },
            None => page.await?,
        };
        let rows = v["trades"].as_array().ok_or_else(|| fatal("missing sequence trades"))?;
        let more = v["has_more"].as_bool().ok_or_else(|| fatal("missing has_more"))?;
        if rows.len() > PAGE as usize {
            return Err(fatal("page exceeds requested size"));
        }
        if rows.is_empty() {
            return Err(retry("REST sequence range not yet visible"));
        }
        for row in rows {
            let trade = parse_trade(row, name, size).map_err(fatal)?;
            let seq = trade.id.parse::<u64>().map_err(fatal)?;
            if seq < next || seq > target {
                return Err(fatal("page outside requested sequence range or non-progressing"));
            }
            if seq != next {
                return Err(retry(format!("unproven sequence {next} before {seq}")));
            }
            if previous_ms.is_some_and(|ms| trade.timestamp_ms < ms) {
                return Err(fatal("sequence timestamps reversed"));
            }
            previous_ms = Some(trade.timestamp_ms);
            output.push(trade);
            next = seq.checked_add(1).ok_or_else(|| fatal("sequence overflow"))?;
        }
        if output.len() >= MAX_TRADES && next <= target {
            // Bounded validated chunk; the worker ingests it and resumes from its cursor.
            return Ok(output);
        }
        if next > target {
            if more {
                return Err(fatal("has_more after requested end_seq"));
            }
            return Ok(output);
        }
        if !more {
            return Err(retry("REST has_more=false before fixed target; history lagging"));
        }
    }
    Ok(output) // bounded contiguous prefix; the worker resumes from its new cursor
}

const IDENTITY_FIELDS: [&str; 7] = ["trade_seq", "trade_id", "timestamp", "price", "amount", "direction", "instrument_name"];

/// Same trade on one tape: every required identity field present and equal;
/// the documented-optional `contracts` may be absent on either side but must
/// be equal when both carry it. Each row is validated separately by parse_trade.
fn same_row(a: &Value, b: &Value) -> bool {
    IDENTITY_FIELDS.iter().all(|k| a.get(*k).is_some() && a.get(*k) == b.get(*k))
        && match (a.get("contracts"), b.get("contracts")) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        }
}

async fn seq_rows(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    size: f64,
    start: u64,
    end: u64,
) -> std::result::Result<BTreeMap<u64, Value>, HistoryError> {
    let v = get(
        client,
        base,
        "get_last_trades_by_instrument",
        &[
            ("instrument_name", name.into()),
            ("start_seq", start.to_string()),
            ("end_seq", end.to_string()),
            ("count", PAGE.to_string()),
            ("sorting", "asc".into()),
        ],
    )
    .await?;
    let rows = v["trades"].as_array().ok_or_else(|| fatal("missing sequence trades"))?;
    if rows.len() > PAGE as usize {
        return Err(fatal("page exceeds requested size"));
    }
    let mut out = BTreeMap::<u64, Value>::new();
    for row in rows {
        let seq = row.get("trade_seq").and_then(Value::as_u64).ok_or_else(|| fatal("invalid integer trade_seq"))?;
        // The archive orders by timestamp and pads edges: keep only in-range rows,
        // and require any repeated row to be field-identical.
        if seq < start || seq > end {
            continue;
        }
        // Validate every in-range row, so an invalid duplicate cannot hide behind
        // a good earlier one.
        parse_trade(row, name, size).map_err(fatal)?;
        if let Some(old) = out.get(&seq) {
            if !same_row(old, row) {
                return Err(fatal(format!("conflicting duplicate archive sequence {seq}")));
            }
        } else {
            out.insert(seq, row.clone());
        }
    }
    Ok(out)
}

/// Same-tape gate: a retained primary window must be complete and field-identical
/// on the archive, and the archive instrument must carry the same units.
pub(super) async fn archive_identity(
    client: &reqwest::Client,
    primary: &str,
    archive: &str,
    symbol: Symbol,
    size: f64,
    target: u64,
) -> std::result::Result<(), HistoryError> {
    // The real archive instrument omits `state`/`instrument_type`; require the
    // remaining identity/linear/active fields and the exact contract size.
    let name = product(symbol);
    let v = get(client, archive, "get_instrument", &[("instrument_name", name.into())]).await?;
    let base = if matches!(symbol, Symbol::Btc) { "BTC" } else { "ETH" };
    let ok = field(&v, "instrument_name").ok() == Some(name)
        && field(&v, "base_currency").ok() == Some(base)
        && ["quote_currency", "counter_currency", "settlement_currency"].iter().all(|k| field(&v, k).ok() == Some("USDC"))
        && field(&v, "kind").ok() == Some("future")
        && field(&v, "future_type").ok() == Some("linear")
        && v["is_active"] == true;
    if !ok {
        return Err(fatal("archive instrument is not the same active linear USDC future"));
    }
    if positive(&v, "contract_size").map_err(fatal)? != size {
        return Err(fatal("archive contract size differs from primary"));
    }
    let lo = target.saturating_sub(GATE_BACK).max(1);
    let hi = lo + GATE_LEN - 1;
    let name = product(symbol);
    let a = seq_rows(client, primary, name, size, lo, hi).await?;
    let b = seq_rows(client, archive, name, size, lo, hi).await?;
    if a.len() as u64 != GATE_LEN || b.len() as u64 != GATE_LEN {
        return Err(retry(format!("archive identity window {lo}-{hi} incomplete ({}/{})", a.len(), b.len())));
    }
    for (seq, row) in &a {
        if !same_row(row, &b[seq]) {
            return Err(fatal(format!("archive identity mismatch at sequence {seq}")));
        }
    }
    Ok(())
}

/// One bounded, sequence-exact archive chunk ending at most at `target - 1`
/// (the fixed live target is handed off by the worker) or at the first trade
/// back inside the primary window, whichever comes first.
#[allow(clippy::too_many_arguments)]
pub(super) async fn archive_chunk(
    client: &reqwest::Client,
    archive: &str,
    symbol: Symbol,
    size: f64,
    cursor: Option<u64>,
    from_ms: Option<i64>,
    target: u64,
    target_ms: i64,
) -> std::result::Result<Vec<Trade>, HistoryError> {
    let name = product(symbol);
    let from = from_ms.ok_or_else(|| fatal("missing recovery anchor"))?;
    let predecessor = if let Some(cursor) = cursor {
        cursor
    } else {
        let v = get(
            client,
            archive,
            "get_last_trades_by_instrument_and_time",
            &[
                ("instrument_name", name.into()),
                ("start_timestamp", from.saturating_sub(3_600_000).max(0).to_string()),
                ("end_timestamp", from.saturating_sub(1).to_string()),
                ("count", "100".into()),
                ("sorting", "desc".into()),
            ],
        )
        .await?;
        let rows = v["trades"].as_array().ok_or_else(|| fatal("invalid predecessor response"))?;
        let mut best: Option<u64> = None;
        for row in rows {
            let trade = parse_trade(row, name, size).map_err(fatal)?;
            if trade.timestamp_ms < from {
                best = best.max(Some(trade.id.parse::<u64>().map_err(fatal)?));
            }
        }
        best.ok_or_else(|| fatal("no archive predecessor before closed-minute boundary"))?
    };
    if predecessor >= target {
        return Err(fatal("recovery predecessor is not before fixed target"));
    }
    let mut next = predecessor + 1;
    let mut output = Vec::new();
    let mut previous_ms = if cursor.is_some() { Some(from) } else { None };
    for page in 0..ARCHIVE_PAGES {
        if next >= target {
            break;
        }
        if page > 0 {
            tokio::time::sleep(ARCHIVE_PACE).await;
        }
        let end = (next + ARCHIVE_SPAN - 1).min(target - 1);
        let rows = seq_rows(client, archive, name, size, next, end).await?;
        let mut progressed = false;
        for (seq, row) in rows.range(next..=end) {
            if *seq != next {
                break; // interior/edge hole: re-request from the missing sequence
            }
            let trade = parse_trade(row, name, size).map_err(fatal)?;
            if previous_ms.is_some_and(|ms| trade.timestamp_ms < ms) {
                return Err(fatal("sequence timestamps reversed"));
            }
            previous_ms = Some(trade.timestamp_ms);
            output.push(trade);
            next += 1;
            progressed = true;
        }
        if !progressed {
            if !output.is_empty() {
                break; // deliver the validated prefix; the next attempt retries the hole
            }
            let upto = rows.keys().next().map_or(end, |s| s - 1);
            return Err(retry(format!("archive missing sequence interval {next}-{upto}")));
        }
        if previous_ms.is_some_and(|ms| target_ms - ms <= ARCHIVE_AFTER_MS) {
            break; // back inside the primary window: hand off to the primary gateway
        }
    }
    Ok(output)
}

/// Retry details where the authoritative tape itself skipped a sequence. These
/// are not outages: after the unchanged bounded episode the source fails closed.
fn is_hole(detail: &str) -> bool {
    detail.starts_with("unproven sequence ") || detail.starts_with("archive missing sequence interval ")
}

fn same(a: &Trade, b: &Trade) -> bool {
    a.id == b.id
        && a.timestamp_ms == b.timestamp_ms
        && a.price == b.price
        && a.base == b.base
        && a.quote == b.quote
        && a.is_buy == b.is_buy
}
pub(super) fn remember(seen: &mut BTreeMap<u64, Trade>, seq: u64, trade: &Trade) -> Result<()> {
    if let Some(previous) = seen.get(&seq) {
        anyhow::ensure!(same(previous, trade), "conflicting DRB sequence {seq}");
    } else {
        seen.insert(seq, trade.clone());
        // One complete bounded REST batch plus queued WS identities must fit:
        // a late duplicate of the first page must not escape comparison merely
        // because later pages filled the ordinary live dedupe window.
        if seen.len() > MAX_TRADES + crate::recovery::RECOVERY_CHANNEL_CAPACITY {
            seen.pop_first();
        }
    }
    Ok(())
}

/// Bound the long-gap buffer without forgetting the newest observed sequence:
/// evict the entry just below the high-water mark, so a known discarded tail
/// keeps a pending target and can never be reported live before it is ingested.
pub(super) fn drop_below_high_water(pending: &mut BTreeMap<u64, Trade>) {
    let Some(&high) = pending.keys().next_back() else { return };
    if let Some(&below) = pending.range(..high).next_back().map(|(k, _)| k) {
        pending.remove(&below);
    }
}

pub(crate) async fn worker(state: Arc<AppState>, symbol: Symbol, rx: mpsc::Receiver<Trade>, base: String, size: f64) {
    // The shard (runtime owner) carries the highest known WS execution above the
    // committed cursor across socket workers: a successor that only sees a lower
    // valid prefix must still recover through the predecessor's known frontier.
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, symbol).clone();
    let handed_over = shard.engine.lock().await.drb_known_frontier.clone();
    let mut pending = BTreeMap::<u64, Trade>::new();
    run_worker(state, symbol, rx, base, size, handed_over, &mut pending).await;
    let mut engine = shard.engine.lock().await;
    let committed = engine.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
    let seq = |t: &Trade| t.id.parse::<u64>().unwrap_or(0);
    let known = [engine.drb_known_frontier.take(), pending.pop_last().map(|(_, t)| t)]
        .into_iter()
        .flatten()
        .max_by_key(seq);
    // Without an ingested cursor, a startup trade on/after the uncommitted
    // closed-minute boundary is still owed by the recovery.
    let from = engine.recovery_from_ms;
    engine.drb_known_frontier = known.filter(|t| match committed {
        Some(c) => seq(t) > c,
        None => from.is_some_and(|f| t.timestamp_ms >= f),
    });
}

async fn run_worker(
    state: Arc<AppState>,
    symbol: Symbol,
    mut rx: mpsc::Receiver<Trade>,
    base: String,
    size: f64,
    mut handed_over: Option<Trade>,
    pending: &mut BTreeMap<u64, Trade>,
) {
    let market = Market::Usdc;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, market, symbol).clone();
    let mut seen = BTreeMap::<u64, Trade>::new();
    let mut budget: Option<(Instant, usize)> = None;
    // Consecutive bounded recovery episodes that ended without progress for
    // transient reasons; spaces the next episode with a capped backoff.
    let mut outage = 0_u32;
    let (initial_cursor, initial_from) = {
        let e = shard.engine.lock().await;
        (e.last_trade_id.clone(), e.recovery_from_ms)
    };
    if initial_cursor.as_deref().is_some_and(|id| id.parse::<u64>().is_err()) {
        crate::recovery::reject_source(
            &state,
            Exchange::Coinbase,
            market,
            symbol,
            "invalid durable DRB sequence cursor".into(),
        )
        .await;
        return;
    }
    let mut startup = initial_from.is_some() && initial_cursor.is_none();
    // A handed-over obligation is processed first, without waiting for new input.
    while let Some(trade) = match handed_over.take() {
        Some(trade) => Some(trade),
        None => rx.recv().await,
    } {
        if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await {
            break;
        }
        // The closed-minute prefix is durable, not part of this source's new
        // unfinished minute. Wait for a trade on/after the boundary to anchor.
        if startup && initial_from.is_some_and(|from| trade.timestamp_ms < from) {
            continue;
        }
        let seq: u64 = match trade.id.parse() {
            Ok(v) if v > 0 => v,
            _ => {
                crate::recovery::reject_source(
                    &state,
                    Exchange::Coinbase,
                    market,
                    symbol,
                    "invalid trade sequence".into(),
                )
                .await;
                break;
            }
        };
        if let Err(error) = remember(&mut seen, seq, &trade) {
            crate::recovery::reject_source(&state, Exchange::Coinbase, market, symbol, error.to_string()).await;
            break;
        }
        let cursor = shard.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
        if cursor.is_some_and(|v| seq <= v) {
            continue;
        }
        if let Some(existing) = pending.get(&seq) {
            if !same(existing, &trade) {
                crate::recovery::reject_source(
                    &state,
                    Exchange::Coinbase,
                    market,
                    symbol,
                    format!("conflicting websocket sequence {seq}"),
                )
                .await;
                break;
            }
        } else {
            pending.insert(seq, trade);
        }
        if pending.len() > crate::recovery::RECOVERY_CHANNEL_CAPACITY {
            // Archive bridge or prolonged outage: keep the fixed target and memory
            // bounded by retaining the highest buffered identity and evicting the entry
            // just below it; evicted sequences stay unproven until replay supplies them
            // contiguously (the known high-water stays required before live).
            drop_below_high_water(pending);
        }
        loop {
            let Some((&first, first_trade)) = pending.first_key_value() else {
                break;
            };
            let first_ms = first_trade.timestamp_ms;
            let cursor = shard.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
            if cursor.is_some_and(|v| first <= v) {
                pending.pop_first();
                continue;
            }
            let contiguous = cursor.is_some_and(|v| v.checked_add(1) == Some(first));
            if !startup && (contiguous || cursor.is_none()) {
                if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await {
                    return;
                }
                let t = pending.pop_first().unwrap().1;
                if let Err(error) = crate::ingestion::ingest_trade(&state, Exchange::Coinbase, market, symbol, t).await
                {
                    crate::recovery::reject_source(
                        &state,
                        Exchange::Coinbase,
                        market,
                        symbol,
                        format!("DRB live ingestion failed: {error}"),
                    )
                    .await;
                    return;
                }
                if pending.is_empty() {
                    budget = None;
                    crate::recovery::mark_live(&state, Exchange::Coinbase, market, symbol).await;
                }
                continue;
            }
            let (deadline, attempts) = budget.get_or_insert_with(|| (Instant::now() + DEADLINE, 0));
            let target = first;
            let mut diag = shard.recovery.read().await.clone();
            diag.state = "recovering".into();
            diag.durable_cursor = cursor.map(|n| n.to_string());
            diag.durable_from_ms = initial_from;
            diag.buffered_trades = pending.len();
            let anchor_ms = if startup { initial_from } else { shard.engine.lock().await.last_trade_ms };
            let (&high_seq, high_trade) = pending.last_key_value().expect("non-empty");
            let fresh = (high_seq, high_trade.timestamp_ms);
            // True while the durable anchor is older than the primary window and the
            // gap is being bridged from the archive in bounded chunks (diagnostic only).
            let long_gap = anchor_ms.is_some_and(|ms| fresh.1 - ms > ARCHIVE_AFTER_MS);
            diag.detail = Some(if long_gap {
                format!(
                    "DRB archive bridge; cursor {} at {} ms; fixed target {target} at {first_ms} ms",
                    cursor.map_or("closed-minute anchor".into(), |c| c.to_string()),
                    anchor_ms.unwrap_or_default()
                )
            } else {
                format!("DRB sequence gap; fixed target {target}")
            });
            if !crate::recovery::store_recovery(&shard, diag.clone()).await {
                return;
            }
            // Hold the recovery read guard through the stream-error write so a
            // concurrent terminal transition cannot be followed by stale status.
            let current = shard.recovery.read().await;
            if current.needs_backfill() {
                return;
            }
            *shard.stream_error.write().await = Some(format!("recovering: DRB sequence target {target}"));
            drop(current);
            let mut completed = false;
            let mut progressed = false;
            // Last attempt saw an authoritative sequence hole (not an outage).
            let mut hole = false;
            // Last bounded error of this episode only (never prior cooldown text).
            let mut last_error = String::new();
            while *attempts < ATTEMPTS && Instant::now() < *deadline {
                if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await {
                    return;
                }
                *attempts += 1;
                diag.attempts = *attempts;
                if !crate::recovery::store_recovery(&shard, diag.clone()).await {
                    return;
                }
                let cursor_ms = shard.engine.lock().await.last_trade_ms;
                // Re-derive the coverage/identity basis per attempt: the freshest
                // retained observation advances while the fixed target ages.
                let fresh = pending.last_key_value().map_or(fresh, |(&n, t)| (n, t.timestamp_ms));
                let request = history_until(
                    &state.http,
                    &base,
                    symbol,
                    size,
                    cursor,
                    if startup { initial_from } else { cursor_ms },
                    target,
                    first_ms,
                    fresh,
                    deadline.checked_sub(CHUNK_MARGIN),
                );
                tokio::pin!(request);
                let response = loop {
                    tokio::select! {
                        result=&mut request => break Some(result),
                        incoming=rx.recv()=> match incoming {
                            Some(t)=> {
                                if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await { return; }
                                let n: u64=match t.id.parse() {Ok(n)=>n,Err(_)=>{crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,"invalid buffered sequence".into()).await;return;}};
                                if let Err(error)=remember(&mut seen,n,&t) {crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,error.to_string()).await;return;}
                                if let Some(old)=pending.get(&n) { if !same(old,&t) {crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,format!("conflicting buffered sequence {n}")).await;return;} }
                                else if cursor.is_none_or(|c|n>c) {pending.insert(n,t);}
                                if pending.len()>crate::recovery::RECOVERY_CHANNEL_CAPACITY { drop_below_high_water(pending); }
                            },
                            None=>return, // no partial replay, join happens at caller
                        },
                        _=tokio::time::sleep_until(*deadline)=>break None,
                    }
                };
                if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await {
                    return;
                }
                match response {
                    Some(Ok(rows)) => {
                        // Validate the entire authoritative batch before admitting any row.
                        // A queued (or later) WS duplicate must compare with REST, even
                        // after its sequence is at/below the committed cursor.
                        for t in &rows {
                            let n: u64 = t.id.parse().expect("validated REST sequence");
                            let identity = pending.get(&n).filter(|ws| !same(ws, t));
                            let remembered = remember(&mut seen, n, t);
                            if identity.is_some() || remembered.is_err() {
                                crate::recovery::reject_source(
                                    &state,
                                    Exchange::Coinbase,
                                    market,
                                    symbol,
                                    format!("REST/WebSocket sequence {n} conflict"),
                                )
                                .await;
                                return;
                            }
                        }
                        diag.rest_count = rows.len();
                        diag.rest_earliest_ms = rows.first().map(|t| t.timestamp_ms);
                        diag.rest_latest_ms = rows.last().map(|t| t.timestamp_ms);
                        if !crate::recovery::store_recovery(&shard, diag.clone()).await {
                            return;
                        }
                        for t in rows {
                            if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await {
                                return;
                            }
                            if startup && initial_from.is_some_and(|from| t.timestamp_ms < from) {
                                continue;
                            }
                            if let Err(e) =
                                crate::ingestion::ingest_trade(&state, Exchange::Coinbase, market, symbol, t).await
                            {
                                crate::recovery::reject_source(
                                    &state,
                                    Exchange::Coinbase,
                                    market,
                                    symbol,
                                    format!("DRB replay ingest failed: {e}"),
                                )
                                .await;
                                return;
                            }
                        }
                        let committed =
                            shard.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
                        pending.retain(|&n, _| committed.is_none_or(|c| n > c));
                        startup = false;
                        completed = true;
                        // Renew the work budget only for validated, ingested progress
                        // (a bounded chunk), never because more live trades arrived.
                        progressed = committed != cursor;
                        break;
                    }
                    Some(Err(HistoryError::Fatal(e))) => {
                        crate::recovery::reject_source(&state, Exchange::Coinbase, market, symbol, e).await;
                        return;
                    }
                    Some(Err(HistoryError::Retry(e))) => {
                        hole = is_hole(&e);
                        last_error = e.chars().take(300).collect();
                        diag.detail = Some(e);
                        if !crate::recovery::store_recovery(&shard, diag.clone()).await {
                            return;
                        }
                    }
                    None => {
                        hole = false; // in-flight request outlived the episode: transient
                        last_error = "history request outlived the episode deadline".into();
                        break;
                    }
                }
                let pause = (Instant::now() + Duration::from_secs(1)).min(*deadline);
                loop {
                    tokio::select! {
                        incoming=rx.recv()=> if let Some(t)=incoming {
                            if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await { return; }
                            let n: u64=match t.id.parse(){Ok(n)=>n,Err(_)=>{crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,"invalid buffered sequence".into()).await;return;}};
                            if let Err(error)=remember(&mut seen,n,&t) {crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,error.to_string()).await;return;}
                            if let Some(old)=pending.get(&n) {if !same(old,&t){crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,format!("conflicting sequence {n}")).await;return;}}
                            else if cursor.is_none_or(|c|n>c) {pending.insert(n,t);}
                            if pending.len()>crate::recovery::RECOVERY_CHANNEL_CAPACITY { drop_below_high_water(pending); }
                        } else {return},
                        _=tokio::time::sleep_until(pause)=>break,
                    }
                }
            }
            if !completed {
                let reason = format!(
                    "budget exhausted after {attempts} attempts without committed progress; processed cursor {}; target {target}; {}",
                    cursor.map_or("none".into(), |c| c.to_string()),
                    last_error
                );
                if hole {
                    // Authoritative history skips a sequence: fail closed as before.
                    crate::recovery::reject_source(
                        &state,
                        Exchange::Coinbase,
                        market,
                        symbol,
                        format!("DRB recovery {reason}"),
                    )
                    .await;
                    return;
                }
                // Transient unavailability (transport/HTTP/RPC retryable, lagging or empty
                // history, request outliving the episode): stay recovering and never live,
                // back off (capped), then start a new bounded episode from the committed
                // cursor and the same fixed target, without waiting for a new trade.
                outage = outage.saturating_add(1);
                let wait = crate::recovery::outage_backoff(outage);
                diag.state = "recovering".into();
                diag.buffered_trades = pending.len();
                diag.detail = Some(format!(
                    "DRB history temporarily unavailable; episode {outage} {reason}; automatic retry in {}s",
                    wait.as_secs()
                ));
                if !crate::recovery::store_recovery(&shard, diag).await {
                    return;
                }
                let until = Instant::now() + wait;
                loop {
                    tokio::select! {
                        incoming=rx.recv()=> if let Some(t)=incoming {
                            if crate::recovery::is_terminal(&state, Exchange::Coinbase, market, symbol).await { return; }
                            let n: u64=match t.id.parse(){Ok(n)=>n,Err(_)=>{crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,"invalid buffered sequence".into()).await;return;}};
                            if let Err(error)=remember(&mut seen,n,&t) {crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,error.to_string()).await;return;}
                            if let Some(old)=pending.get(&n) {if !same(old,&t){crate::recovery::reject_source(&state,Exchange::Coinbase,market,symbol,format!("conflicting sequence {n}")).await;return;}}
                            else if cursor.is_none_or(|c|n>c) {pending.insert(n,t);}
                            if pending.len()>crate::recovery::RECOVERY_CHANNEL_CAPACITY { drop_below_high_water(pending); }
                        } else {return},
                        _=tokio::time::sleep_until(until)=>break,
                    }
                }
                budget = None;
                continue;
            }
            outage = 0;
            // Keep the same budget across chained gaps; a new incoming trade cannot reset it.
            if pending.is_empty() {
                budget = None;
                crate::recovery::mark_live(&state, Exchange::Coinbase, market, symbol).await;
            } else if progressed {
                // Any validated+ingested chunk (archive or primary) renews the work
                // budget; retries, errors, empty REST and new WS arrivals never do.
                budget = None;
            }
        }
    }
}

pub(super) fn channels() -> Vec<String> {
    [Symbol::Btc, Symbol::Eth].map(|s| format!("trades.{}.100ms", product(s))).to_vec()
}
pub(super) fn decode_notification(v: &Value, sizes: &HashMap<Symbol, f64>) -> Result<Option<(Symbol, Vec<Trade>)>> {
    if v["method"] != "subscription" {
        return Ok(None);
    }
    let channel = field(&v["params"], "channel")?;
    let symbol = channels()
        .iter()
        .find(|c| c.as_str() == channel)
        .and_then(|_| channel.strip_prefix("trades.").and_then(|s| s.strip_suffix(".100ms")))
        .and_then(sym)
        .ok_or_else(|| anyhow!("unexpected DRB subscription channel {channel}"))?;
    let rows = v["params"]["data"].as_array().ok_or_else(|| anyhow!("malformed subscribed trade array"))?;
    let size = *sizes.get(&symbol).ok_or_else(|| anyhow!("unvalidated DRB instrument {channel}"))?;
    let mut trades = rows.iter().map(|r| parse_trade(r, product(symbol), size)).collect::<Result<Vec<_>>>()?;
    trades.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(0));
    Ok(Some((symbol, trades)))
}
pub(super) fn rpc(id: u64, method: &str, params: Value) -> Message {
    Message::Text(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string().into())
}
