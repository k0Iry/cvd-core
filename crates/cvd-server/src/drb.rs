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
    let ms = v
        .get("timestamp")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("invalid millisecond timestamp"))?;
    let price = positive(v, "price")?;
    let base = positive(v, "amount")?;
    let contracts = positive(v, "contracts")?;
    let expected = contracts * contract_size;
    anyhow::ensure!(
        expected.is_finite() && (base - expected).abs() <= (base * 1e-8).max(1e-10),
        "amount does not match contracts * contract_size"
    );
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
pub(super) async fn history(
    client: &reqwest::Client,
    base: &str,
    symbol: Symbol,
    size: f64,
    cursor: Option<u64>,
    from_ms: Option<i64>,
    target: u64,
    target_ms: i64,
) -> std::result::Result<Vec<Trade>, HistoryError> {
    let name = product(symbol);
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
        let v = get(
            client,
            base,
            "get_last_trades_by_instrument",
            &[
                ("instrument_name", name.into()),
                ("start_seq", next.to_string()),
                ("end_seq", target.to_string()),
                ("count", PAGE.to_string()),
                ("sorting", "asc".into()),
            ],
        )
        .await?;
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
            if output.len() > MAX_TRADES {
                return Err(fatal("replay trade budget exhausted"));
            }
            next = seq.checked_add(1).ok_or_else(|| fatal("sequence overflow"))?;
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
    Err(fatal("replay page budget exhausted"))
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

pub(crate) async fn worker(
    state: Arc<AppState>,
    symbol: Symbol,
    mut rx: mpsc::Receiver<Trade>,
    base: String,
    size: f64,
) {
    let market = Market::Usdc;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, market, symbol).clone();
    let mut pending = BTreeMap::<u64, Trade>::new();
    let mut seen = BTreeMap::<u64, Trade>::new();
    let mut budget: Option<(Instant, usize)> = None;
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
    while let Some(trade) = rx.recv().await {
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
            crate::recovery::mark_buffer_exhausted(&state, Exchange::Coinbase, market, symbol).await;
            break;
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
            diag.detail = Some(format!("DRB sequence gap; fixed target {target}"));
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
                let request = history(
                    &state.http,
                    &base,
                    symbol,
                    size,
                    cursor,
                    if startup { initial_from } else { cursor_ms },
                    target,
                    first_ms,
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
                                if pending.len()>crate::recovery::RECOVERY_CHANNEL_CAPACITY {crate::recovery::mark_buffer_exhausted(&state,Exchange::Coinbase,market,symbol).await;return;}
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
                        pending.retain(|&n, _| n > target);
                        startup = false;
                        completed = true;
                        break;
                    }
                    Some(Err(HistoryError::Fatal(e))) => {
                        crate::recovery::reject_source(&state, Exchange::Coinbase, market, symbol, e).await;
                        return;
                    }
                    Some(Err(HistoryError::Retry(e))) => {
                        diag.detail = Some(e);
                        if !crate::recovery::store_recovery(&shard, diag.clone()).await {
                            return;
                        }
                    }
                    None => break,
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
                            if pending.len()>crate::recovery::RECOVERY_CHANNEL_CAPACITY {crate::recovery::mark_buffer_exhausted(&state,Exchange::Coinbase,market,symbol).await;return;}
                        } else {return},
                        _=tokio::time::sleep_until(pause)=>break,
                    }
                }
            }
            if !completed {
                crate::recovery::reject_source(
                    &state,
                    Exchange::Coinbase,
                    market,
                    symbol,
                    format!(
                        "DRB recovery budget exhausted after {attempts} attempts; target {target}; {}",
                        diag.detail.unwrap_or_default()
                    ),
                )
                .await;
                return;
            }
            // Keep the same budget across chained gaps; a new incoming trade cannot reset it.
            if pending.is_empty() {
                budget = None;
                crate::recovery::mark_live(&state, Exchange::Coinbase, market, symbol).await;
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
