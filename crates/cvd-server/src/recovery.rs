use std::{
    cmp::Ordering,
    collections::{HashSet, VecDeque},
    future::Future,
    sync::Arc,
};

#[cfg(test)]
use anyhow::Context;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc,
    time::{Duration, Instant},
};

use crate::{
    ingestion::{ingest_trade, Trade},
    models::AggTrade,
    types::{Exchange, Market, Symbol},
    AppState,
};

#[path = "recovery_spot.rs"]
pub(crate) mod spot;
pub use spot::{enqueue_spot, SpotInput};

const MAX_REPLAY_PAGES: usize = 100;
pub const RECOVERY_CHANNEL_CAPACITY: usize = 8_192;
const RECOVERY_ATTEMPTS: usize = 20;
const RECOVERY_DEADLINE: Duration = Duration::from_secs(30);
const RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryDiagnostics {
    pub state: String,
    pub attempts: usize,
    pub buffered_trades: usize,
    pub durable_cursor: Option<String>,
    pub durable_from_ms: Option<i64>,
    pub rest_earliest_ms: Option<i64>,
    pub rest_latest_ms: Option<i64>,
    pub rest_count: usize,
    pub detail: Option<String>,
}

impl Default for RecoveryDiagnostics {
    fn default() -> Self {
        Self {
            state: "starting".into(),
            attempts: 0,
            buffered_trades: 0,
            durable_cursor: None,
            durable_from_ms: None,
            rest_earliest_ms: None,
            rest_latest_ms: None,
            rest_count: 0,
            detail: None,
        }
    }
}

impl RecoveryDiagnostics {
    pub fn is_recovering(&self) -> bool {
        self.state == "recovering"
    }

    pub fn needs_backfill(&self) -> bool {
        self.state == "needs_backfill"
    }
}

#[derive(Clone, Debug)]
pub struct RecoveryAnchor {
    pub cursor: Option<String>,
    pub from_ms: i64,
    pub through_ms: i64,
}

#[derive(Debug)]
pub enum RecoveryDecision {
    Retry(String),
    Ready(Vec<Trade>),
    NeedsBackfill(String),
}

pub struct RecoveryCoordinator {
    anchor: RecoveryAnchor,
    buffer: VecDeque<Trade>,
    buffer_capacity: usize,
    spot_target: Option<u64>,
}

impl RecoveryCoordinator {
    pub fn new(anchor: RecoveryAnchor, buffer_capacity: usize) -> Self {
        assert!(buffer_capacity > 0, "recovery buffer must not be empty");
        Self { anchor, buffer: VecDeque::with_capacity(buffer_capacity), buffer_capacity, spot_target: None }
    }

    pub fn push_ws(&mut self, trade: Trade) -> Result<()> {
        anyhow::ensure!(
            self.buffer.len() < self.buffer_capacity,
            "recovery websocket buffer exhausted at {} trades; source paused, historical backfill required",
            self.buffer_capacity
        );
        self.buffer.push_back(trade);
        Ok(())
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    fn set_spot_target(&mut self, id: u64) {
        self.spot_target = Some(id);
    }

    #[cfg(test)]
    pub fn observe_rest(&mut self, rest: Vec<Trade>) -> RecoveryDecision {
        self.observe_batch(rest, None)
    }

    fn observe_batch(&mut self, mut rest: Vec<Trade>, covered_from_ms: Option<i64>) -> RecoveryDecision {
        if rest.is_empty() {
            return RecoveryDecision::Retry("public history returned no trades".into());
        }
        rest.sort_by(compare_trades);
        let earliest_ms = rest.first().map(|trade| trade.timestamp_ms).unwrap_or_default();
        let latest_ms = rest.last().map(|trade| trade.timestamp_ms).unwrap_or_default();
        let start = if let Some(cursor) = self.anchor.cursor.as_deref() {
            let Some(position) = rest.iter().position(|trade| trade.id == cursor) else {
                // Cached history can still be older than an already ingested cursor.
                let older_id = cursor.parse::<u64>().ok().is_some_and(|id| {
                    rest.iter().filter_map(|t| t.id.parse::<u64>().ok()).max().is_some_and(|last| last < id)
                });
                if latest_ms <= self.anchor.from_ms || older_id {
                    return RecoveryDecision::Retry("public history is still behind the recovery cursor".into());
                }
                return RecoveryDecision::NeedsBackfill(format!(
                    "durable cursor {cursor} is absent from public history ({earliest_ms}..={latest_ms}); authoritative historical backfill required"
                ));
            };
            if cursor.parse::<u64>().is_ok() {
                position + 1
            } else {
                rest[position]
                    .timestamp_ms
                    .checked_sub(1)
                    .and_then(|before| rest.iter().position(|trade| trade.timestamp_ms > before))
                    .unwrap_or(position)
            }
        } else {
            if earliest_ms > self.anchor.from_ms && covered_from_ms.is_none_or(|from| from > self.anchor.from_ms) {
                return RecoveryDecision::NeedsBackfill(format!(
                    "public history starts at {earliest_ms}, after durable boundary {}; authoritative historical backfill required",
                    self.anchor.from_ms
                ));
            }
            rest.iter().position(|trade| trade.timestamp_ms >= self.anchor.from_ms).unwrap_or(rest.len())
        };

        let buffered_ids: HashSet<&str> = self.buffer.iter().map(|trade| trade.id.as_str()).collect();
        let overlap = rest
            .iter()
            .rev()
            .find(|trade| buffered_ids.contains(trade.id.as_str()))
            .cloned()
            .or_else(|| self.spot_target.and_then(|id| rest.iter().find(|t| t.id == id.to_string()).cloned()));
        if overlap.is_none() {
            return RecoveryDecision::Retry(format!(
                "public history through {latest_ms} has not overlapped {} buffered websocket trades",
                self.buffer.len()
            ));
        }

        let overlap = overlap.unwrap();
        let mut handoff = rest
            .into_iter()
            .skip(start)
            .filter(|trade| compare_trades(trade, &overlap) != Ordering::Greater)
            .collect::<Vec<_>>();
        handoff.extend(self.buffer.iter().cloned());
        handoff.sort_by(compare_trades);
        let mut seen = HashSet::new();
        handoff.retain(|trade| seen.insert(trade.id.clone()));
        if self.spot_target.is_some() {
            if let Err(error) = validate_spot_handoff(&self.anchor, &handoff) {
                return RecoveryDecision::Retry(format!("spot history has not closed the ID gap: {error}"));
            }
        }
        self.buffer.clear();
        RecoveryDecision::Ready(handoff)
    }
}

fn validate_spot_handoff(anchor: &RecoveryAnchor, rows: &[Trade]) -> Result<()> {
    let cursor = anchor.cursor.as_deref().map(str::parse::<u64>).transpose()?;
    let mut previous = cursor;
    for row in rows {
        if row.timestamp_ms < anchor.from_ms {
            continue;
        }
        let id = row.id.parse::<u64>()?;
        if cursor.is_some_and(|old| id <= old) {
            continue;
        }
        anyhow::ensure!(
            previous.is_none_or(|old| old.checked_add(1) == Some(id)),
            "Coinbase spot handoff has a trade ID gap"
        );
        previous = Some(id);
    }
    Ok(())
}

pub fn compare_trades(left: &Trade, right: &Trade) -> Ordering {
    left.timestamp_ms.cmp(&right.timestamp_ms).then_with(|| {
        left.id
            .parse::<u64>()
            .ok()
            .zip(right.id.parse::<u64>().ok())
            .map_or(Ordering::Equal, |(left_id, right_id)| left_id.cmp(&right_id))
    })
}

#[cfg(test)]
fn after_cursor(mut trades: Vec<Trade>, cursor: Option<&str>, from_ms: i64) -> Result<Vec<Trade>> {
    trades.sort_by(compare_trades);
    if let Some(cursor) = cursor {
        let position = trades
            .iter()
            .position(|trade| trade.id == cursor)
            .context("trade gap exceeds public history window; source paused, historical backfill required")?;
        if cursor.parse::<u64>().is_ok() {
            Ok(trades.into_iter().skip(position + 1).collect())
        } else {
            let anchor_ms = trades[position].timestamp_ms;
            Ok(trades.into_iter().filter(|trade| trade.timestamp_ms >= anchor_ms).collect())
        }
    } else {
        anyhow::ensure!(
            trades.first().is_some_and(|trade| trade.timestamp_ms <= from_ms),
            "startup gap exceeds public history window; source paused, historical backfill required"
        );
        Ok(trades.into_iter().filter(|trade| trade.timestamp_ms >= from_ms).collect())
    }
}

pub async fn binance_before(
    state: &AppState,
    market: Market,
    symbol: Symbol,
    current: &Trade,
    contract_size: Option<f64>,
) -> Result<()> {
    let shard = crate::pick_shard(state, Exchange::Binance, market, symbol);
    let (cursor, from_ms) = {
        let engine = shard.engine.lock().await;
        (engine.last_trade_id.as_ref().and_then(|id| id.parse::<u64>().ok()), engine.recovery_from_ms)
    };
    let target: u64 = current.id.parse()?;
    if cursor.is_some_and(|cursor| target <= cursor.saturating_add(1))
        || (cursor.is_none() && from_ms.is_none())
        || from_ms.is_some_and(|from| current.timestamp_ms < from)
    {
        return Ok(());
    }
    if cursor.is_none() {
        validate_binance_startup_window(from_ms.unwrap(), current.timestamp_ms)?;
    }
    let (base_url, product) = match market {
        Market::Spot => ("https://api.binance.com/api/v3/aggTrades", symbol.spot_usdm_stream()),
        Market::Usdm => ("https://fapi.binance.com/fapi/v1/aggTrades", symbol.spot_usdm_stream()),
        Market::Coinm => ("https://dapi.binance.com/dapi/v1/aggTrades", symbol.coinm_stream()),
        Market::Usdc => anyhow::bail!("Binance has no USDC DRB recovery source"),
    };
    let mut next_id = cursor.map(|id| id + 1);
    for _ in 0..MAX_REPLAY_PAGES {
        let mut request =
            state.http.get(base_url).query(&[("symbol", product.to_uppercase()), ("limit", "1000".into())]);
        request = if let Some(id) = next_id {
            request.query(&[("fromId", id)])
        } else {
            request.query(&[("startTime", from_ms.unwrap()), ("endTime", current.timestamp_ms)])
        };
        let rows: Vec<AggTrade> = request.send().await?.error_for_status()?.json().await?;
        anyhow::ensure!(!rows.is_empty(), "Binance replay not yet available; retaining cursor for retry");
        for row in rows {
            if row.id >= target {
                anyhow::ensure!(
                    row.id == target && next_id.is_none_or(|expected| expected == target),
                    "Binance replay has a trade ID gap"
                );
                return Ok(());
            }
            anyhow::ensure!(next_id.is_none_or(|expected| row.id == expected), "Binance replay has a trade ID gap");
            let price: f64 = row.price.parse()?;
            let (base, quote) = crate::binance::trade_volumes(market, price, row.quantity.parse()?, contract_size)?;
            ingest_trade(
                state,
                Exchange::Binance,
                market,
                symbol,
                Trade {
                    id: row.id.to_string(),
                    timestamp_ms: row.trade_time,
                    price,
                    base,
                    quote,
                    is_buy: !row.is_buyer_maker,
                },
            )
            .await?;
            next_id = Some(row.id + 1);
            if next_id == Some(target) {
                return Ok(());
            }
        }
    }
    anyhow::bail!("Binance replay page budget exhausted; retaining cursor for next retry")
}

fn validate_binance_startup_window(from_ms: i64, to_ms: i64) -> Result<()> {
    anyhow::ensure!(
        to_ms >= from_ms && to_ms - from_ms < 3_600_000,
        "Binance startup replay exceeds one-hour query window; source paused, historical backfill required"
    );
    Ok(())
}

#[derive(Deserialize)]
struct CoinbaseSpotTrade {
    trade_id: u64,
    time: String,
    price: String,
    size: String,
    side: String,
}

fn base_trade(id: String, timestamp_ms: i64, price: &str, size: &str, side: &str, maker_side: bool) -> Result<Trade> {
    anyhow::ensure!(side.eq_ignore_ascii_case("buy") || side.eq_ignore_ascii_case("sell"), "invalid replay trade side");
    let price: f64 = price.parse()?;
    let base: f64 = size.parse()?;
    let is_buy = if maker_side { side.eq_ignore_ascii_case("sell") } else { side.eq_ignore_ascii_case("buy") };
    Ok(Trade { id, timestamp_ms, price, base, quote: price * base, is_buy })
}

async fn fetch_recent(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    anchor: &RecoveryAnchor,
) -> Result<Vec<Trade>> {
    fetch_recent_with_spot_base(state, exchange, market, symbol, anchor, "https://api.exchange.coinbase.com").await
}

async fn fetch_recent_with_spot_base(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    anchor: &RecoveryAnchor,
    spot_base: &str,
) -> Result<Vec<Trade>> {
    let cursor = &anchor.cursor;
    let from_ms = anchor.from_ms;
    let mut trades = Vec::new();
    match (exchange, market) {
        (Exchange::Coinbase, Market::Spot) => {
            let url = format!("{spot_base}/products/{}/trades", symbol.coinbase_spot_product().unwrap());
            let mut after = None;
            for _ in 0..MAX_REPLAY_PAGES {
                let mut request = state.http.get(&url).query(&[("limit", "1000")]);
                if let Some(after) = &after {
                    request = request.query(&[("after", after)]);
                }
                let response = request.send().await?.error_for_status()?;
                let next =
                    response.headers().get("cb-after").and_then(|header| header.to_str().ok()).map(str::to_string);
                let rows: Vec<CoinbaseSpotTrade> = response.json().await?;
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    trades.push(base_trade(
                        row.trade_id.to_string(),
                        crate::parse_iso8601_to_ms(&row.time)?,
                        &row.price,
                        &row.size,
                        &row.side,
                        true,
                    )?);
                }
                if cursor.is_some()
                    && trades.iter().map(|t| t.timestamp_ms).max().is_some_and(|latest| latest < from_ms)
                {
                    break;
                }
                if trades
                    .iter()
                    .any(|trade| cursor.as_ref().map_or(trade.timestamp_ms <= from_ms, |cursor| &trade.id == cursor))
                {
                    break;
                }
                if next.is_none() || next == after {
                    break;
                }
                after = next;
            }
        }

        _ => anyhow::bail!("unsupported replay market"),
    }
    Ok(trades)
}

async fn set_needs_backfill(shard: &crate::Shard, mut diagnostics: RecoveryDiagnostics, detail: String) {
    let mut current = shard.recovery.write().await;
    if current.needs_backfill() {
        return;
    }
    diagnostics.state = "needs_backfill".into();
    diagnostics.detail = Some(detail.clone());
    *current = diagnostics;
    drop(current);
    *shard.stream_error.write().await = Some(format!("needs_backfill: {detail}"));
}

pub(crate) async fn store_recovery(shard: &crate::Shard, diagnostics: RecoveryDiagnostics) -> bool {
    let mut current = shard.recovery.write().await;
    if current.needs_backfill() {
        false
    } else {
        *current = diagnostics;
        true
    }
}

pub async fn mark_buffer_exhausted(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol) {
    let shard = crate::pick_shard(state, exchange, market, symbol);
    let diagnostics = shard.recovery.read().await.clone();
    set_needs_backfill(
        shard,
        diagnostics,
        format!(
            "recovery input queue exhausted at {} trades; authoritative historical backfill required",
            RECOVERY_CHANNEL_CAPACITY
        ),
    )
    .await;
}

pub async fn finish_workers(mut workers: tokio::task::JoinSet<()>) -> Result<()> {
    let mut failed = None;
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            failed = Some(error);
        }
    }
    if let Some(error) = failed {
        return Err(error.into());
    }
    Ok(())
}

pub async fn reject_source(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol, detail: String) {
    let shard = crate::pick_shard(state, exchange, market, symbol);
    let diagnostics = shard.recovery.read().await.clone();
    set_needs_backfill(shard, diagnostics, detail).await;
}

pub async fn enqueue_trade(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    sender: &mpsc::Sender<Trade>,
    trade: Trade,
) {
    if is_terminal(state, exchange, market, symbol).await {
        return;
    }
    match sender.try_send(trade) {
        Ok(()) => (),
        Err(mpsc::error::TrySendError::Full(_)) => mark_buffer_exhausted(state, exchange, market, symbol).await,
        Err(mpsc::error::TrySendError::Closed(_)) => {
            reject_source(state, exchange, market, symbol, "ingestion worker ended unexpectedly; source paused".into())
                .await
        }
    }
}

pub async fn is_terminal(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol) -> bool {
    crate::pick_shard(state, exchange, market, symbol).recovery.read().await.needs_backfill()
}

pub async fn mark_live(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol) {
    let shard = crate::pick_shard(state, exchange, market, symbol);
    let mut diagnostics = shard.recovery.read().await.clone();
    diagnostics.state = "live".into();
    diagnostics.detail = None;
    diagnostics.buffered_trades = 0;
    if store_recovery(shard, diagnostics).await {
        let current = shard.recovery.read().await;
        if !current.needs_backfill() {
            *shard.stream_error.write().await = None;
        }
    }
}

pub async fn run_recent_worker(
    state: Arc<AppState>,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    rx: mpsc::Receiver<Trade>,
) {
    let fetch_state = state.clone();
    run_recent_worker_with_fetch(state, exchange, market, symbol, rx, move |anchor| {
        let fetch_state = fetch_state.clone();
        async move { fetch_recent(&fetch_state, exchange, market, symbol, &anchor).await }
    })
    .await;
}

async fn run_recent_worker_with_fetch<F, Fut>(
    state: Arc<AppState>,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    mut rx: mpsc::Receiver<Trade>,
    mut fetch: F,
) where
    F: FnMut(RecoveryAnchor) -> Fut,
    Fut: Future<Output = Result<Vec<Trade>>>,
{
    let shard = crate::pick_shard(&state, exchange, market, symbol).clone();
    if shard.recovery.read().await.needs_backfill() {
        while rx.recv().await.is_some() {}
        return;
    }

    let (cursor, from_ms) = {
        let engine = shard.engine.lock().await;
        (engine.last_trade_id.clone(), engine.last_trade_ms.or(engine.recovery_from_ms))
    };
    let Some(from_ms) = from_ms else {
        let mut recovery = shard.recovery.read().await.clone();
        recovery.state = "live".into();
        recovery.detail = None;
        if !store_recovery(&shard, recovery).await {
            while rx.recv().await.is_some() {}
            return;
        }
        *shard.stream_error.write().await = None;
        while let Some(trade) = rx.recv().await {
            if let Err(error) = ingest_trade(&state, exchange, market, symbol, trade).await {
                let diagnostics = shard.recovery.read().await.clone();
                set_needs_backfill(
                    &shard,
                    diagnostics,
                    format!("live ingestion failed: {error}; authoritative historical backfill required"),
                )
                .await;
                return;
            }
        }
        return;
    };

    let mut anchor = RecoveryAnchor { cursor, from_ms, through_ms: 0 };
    let mut diagnostics = RecoveryDiagnostics {
        state: "recovering".into(),
        durable_cursor: anchor.cursor.clone(),
        durable_from_ms: Some(anchor.from_ms),
        detail: Some("waiting for validated REST/WebSocket overlap".into()),
        ..RecoveryDiagnostics::default()
    };
    if !store_recovery(&shard, diagnostics.clone()).await {
        return;
    }
    *shard.stream_error.write().await = Some("recovering: waiting for REST/WebSocket overlap".into());

    let first = loop {
        let Some(trade) = rx.recv().await else { return };
        if trade.timestamp_ms >= anchor.from_ms {
            break trade;
        }
        // A saved-minute prefix is already durable. With a live cursor, however,
        // an unseen older execution is a discontinuity, not a duplicate.
        if anchor.cursor.is_some() && !shard.engine.lock().await.is_duplicate(&trade, true) {
            set_needs_backfill(
                &shard,
                diagnostics,
                "unseen pre-anchor websocket trade; source paused rather than discarding delta".into(),
            )
            .await;
            return;
        }
    };
    anchor.through_ms = first.timestamp_ms;
    let mut coordinator = RecoveryCoordinator::new(anchor.clone(), RECOVERY_CHANNEL_CAPACITY);
    if let Err(error) = coordinator.push_ws(first) {
        set_needs_backfill(&shard, diagnostics, error.to_string()).await;
        return;
    }
    let deadline = Instant::now() + RECOVERY_DEADLINE;

    for attempt in 1..=RECOVERY_ATTEMPTS {
        diagnostics.attempts = attempt;
        diagnostics.buffered_trades = coordinator.buffered_len();
        if !store_recovery(&shard, diagnostics.clone()).await {
            return;
        }

        let request = fetch(anchor.clone());
        tokio::pin!(request);
        let result = loop {
            tokio::select! {
                trade = rx.recv() => match trade {
                    Some(trade) => {
                        if let Err(error) = coordinator.push_ws(trade) {
                            set_needs_backfill(&shard, diagnostics, error.to_string()).await;
                            return;
                        }
                    }
                    None => return,
                },
                result = &mut request => break result,
                _ = tokio::time::sleep_until(deadline) => {
                    set_needs_backfill(
                        &shard,
                        diagnostics,
                        "recovery deadline expired without a validated overlap; authoritative historical backfill required".into(),
                    ).await;
                    return;
                }
            }
        };

        let decision = match result {
            Ok(trades) => {
                diagnostics.rest_count = trades.len();
                diagnostics.rest_earliest_ms = trades.iter().map(|trade| trade.timestamp_ms).min();
                diagnostics.rest_latest_ms = trades.iter().map(|trade| trade.timestamp_ms).max();
                coordinator.observe_batch(trades, None)
            }

            Err(error) => RecoveryDecision::Retry(format!("history transport error: {error}")),
        };
        match decision {
            RecoveryDecision::Ready(trades) => {
                if exchange == Exchange::Coinbase && market == Market::Spot {
                    if let Err(error) = validate_spot_handoff(&anchor, &trades) {
                        set_needs_backfill(&shard, diagnostics, error.to_string()).await;
                        return;
                    }
                }
                if shard.recovery.read().await.needs_backfill() {
                    return;
                }
                for trade in &trades {
                    if let Err(error) = ingest_trade(&state, exchange, market, symbol, trade.clone()).await {
                        set_needs_backfill(
                            &shard,
                            diagnostics,
                            format!("recovery replay failed: {error}; authoritative historical backfill required"),
                        )
                        .await;
                        return;
                    }
                }
                {
                    let mut engine = shard.engine.lock().await;
                    engine.remember_replay(&trades);
                }
                diagnostics.state = "live".into();
                diagnostics.buffered_trades = 0;
                diagnostics.detail = None;
                if !store_recovery(&shard, diagnostics).await {
                    return;
                }
                *shard.stream_error.write().await = None;
                while let Some(trade) = rx.recv().await {
                    if let Err(error) = ingest_trade(&state, exchange, market, symbol, trade).await {
                        let diagnostics = shard.recovery.read().await.clone();
                        set_needs_backfill(
                            &shard,
                            diagnostics,
                            format!("live ingestion failed: {error}; authoritative historical backfill required"),
                        )
                        .await;
                        return;
                    }
                }
                return;
            }
            RecoveryDecision::NeedsBackfill(detail) => {
                set_needs_backfill(&shard, diagnostics, detail).await;
                return;
            }
            RecoveryDecision::Retry(detail) => {
                diagnostics.detail = Some(detail);
                diagnostics.buffered_trades = coordinator.buffered_len();
                if !store_recovery(&shard, diagnostics.clone()).await {
                    return;
                }
            }
        }

        let sleep = tokio::time::sleep(RECOVERY_RETRY_DELAY);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                trade = rx.recv() => match trade {
                    Some(trade) => {
                        if let Err(error) = coordinator.push_ws(trade) {
                            set_needs_backfill(&shard, diagnostics, error.to_string()).await;
                            return;
                        }
                    }
                    None => return,
                },
                _ = &mut sleep => break,
                _ = tokio::time::sleep_until(deadline) => {
                    set_needs_backfill(
                        &shard,
                        diagnostics,
                        "recovery deadline expired without a validated overlap; authoritative historical backfill required".into(),
                    ).await;
                    return;
                }
            }
        }
    }

    let detail = format!(
        "recovery attempt budget ({RECOVERY_ATTEMPTS}) exhausted; source paused; last observation: {}",
        diagnostics.detail.as_deref().unwrap_or("none")
    );
    set_needs_backfill(&shard, diagnostics, detail).await;
}

#[cfg(test)]
#[path = "recovery_safety_tests.rs"]
mod safety_tests;

#[cfg(test)]
#[path = "recovery_spot_tests.rs"]
mod spot_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(id: &str, timestamp_ms: i64) -> Trade {
        Trade { id: id.into(), timestamp_ms, price: 10.0, base: 1.0, quote: 10.0, is_buy: true }
    }

    #[test]
    fn replay_requires_anchor_and_preserves_same_millisecond_trades() {
        let rows = vec![trade("3", 1001), trade("1", 1000), trade("2", 1000)];
        let replay = after_cursor(rows.clone(), Some("1"), 1000).unwrap();
        assert_eq!(replay.iter().map(|trade| trade.id.as_str()).collect::<Vec<_>>(), ["2", "3"]);
        assert!(after_cursor(rows.clone(), Some("missing"), 999).is_err());
        assert!(after_cursor(rows, None, 999).is_err());
    }

    #[test]
    fn unordered_ids_replay_the_entire_anchor_millisecond() {
        let rows = vec![trade("unseen", 1000), trade("anchor", 1000), trade("new", 1001)];
        let replay = after_cursor(rows, Some("anchor"), 1000).unwrap();
        assert_eq!(replay.iter().map(|trade| trade.id.as_str()).collect::<Vec<_>>(), ["unseen", "anchor", "new"]);
    }

    #[test]
    fn binance_startup_window_is_bounded() {
        assert!(validate_binance_startup_window(60_000, 3_659_999).is_ok());
        assert!(validate_binance_startup_window(60_000, 3_660_000).is_err());
        assert!(validate_binance_startup_window(60_000, 59_999).is_err());
    }

    #[test]
    fn restart_replay_does_not_include_persisted_minutes() {
        let replay = after_cursor(vec![trade("1", 59_999), trade("2", 60_000)], None, 60_000).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].id, "2");
    }

    #[test]
    fn rest_trade_sides_match_websocket_units() {
        let maker_sell = base_trade("1".into(), 1000, "100", "2", "SELL", true).unwrap();
        let taker_buy = base_trade("2".into(), 1000, "100", "2", "Buy", false).unwrap();
        assert!(maker_sell.is_buy && taker_buy.is_buy);
        assert_eq!(maker_sell.quote, 200.0);
    }

    #[test]
    fn coordinator_waits_for_lagging_rest_then_hands_off_without_loss_or_duplicates() {
        let anchor = RecoveryAnchor { cursor: Some("100".into()), from_ms: 1_000, through_ms: 0 };
        let mut coordinator = RecoveryCoordinator::new(anchor, 8);
        coordinator.push_ws(trade("103", 1_003)).unwrap();
        coordinator.push_ws(trade("104", 1_004)).unwrap();

        assert!(matches!(
            coordinator.observe_rest(vec![trade("100", 1_000), trade("101", 1_001)]),
            RecoveryDecision::Retry(_)
        ));
        coordinator.push_ws(trade("105", 1_005)).unwrap();
        let handoff = match coordinator.observe_rest(vec![
            trade("100", 1_000),
            trade("101", 1_001),
            trade("102", 1_002),
            trade("103", 1_003),
            trade("104", 1_004),
        ]) {
            RecoveryDecision::Ready(trades) => trades,
            other => panic!("expected ready handoff, got {other:?}"),
        };
        assert_eq!(
            handoff.iter().map(|trade| trade.id.as_str()).collect::<Vec<_>>(),
            ["101", "102", "103", "104", "105"]
        );
    }

    #[test]
    fn coordinator_marks_insufficient_history_and_bounds_its_buffer() {
        let anchor = RecoveryAnchor { cursor: Some("100".into()), from_ms: 1_000, through_ms: 0 };
        let mut coordinator = RecoveryCoordinator::new(anchor, 2);
        coordinator.push_ws(trade("200", 2_000)).unwrap();
        coordinator.push_ws(trade("201", 2_001)).unwrap();
        assert!(coordinator.push_ws(trade("202", 2_002)).is_err());
        assert!(matches!(
            coordinator.observe_rest(vec![trade("190", 1_900), trade("191", 1_901)]),
            RecoveryDecision::NeedsBackfill(_)
        ));
    }

    #[test]
    fn overlap_may_move_forward_while_the_original_target_ages_out() {
        let anchor = RecoveryAnchor { cursor: Some("10".into()), from_ms: 10, through_ms: 0 };
        let mut coordinator = RecoveryCoordinator::new(anchor, 8);
        for id in 13..=16 {
            coordinator.push_ws(trade(&id.to_string(), id)).unwrap();
        }
        let handoff = match coordinator.observe_rest(vec![
            trade("10", 10),
            trade("11", 11),
            trade("12", 12),
            trade("15", 15),
            trade("16", 16),
        ]) {
            RecoveryDecision::Ready(trades) => trades,
            other => panic!("expected moving overlap, got {other:?}"),
        };
        assert_eq!(handoff.last().unwrap().id, "16");
        assert_eq!(handoff.iter().filter(|trade| trade.id == "15").count(), 1);
    }

    #[tokio::test]
    async fn injectable_worker_buffers_during_rest_lag_and_replays_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = Arc::new(crate::tests::state().await);
        let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
        {
            let mut engine = shard.engine.lock().await;
            engine.last_trade_id = Some("100".into());
            engine.last_trade_ms = Some(1_000);
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let fetch_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel(8);
        let worker_state = state.clone();
        let worker = tokio::spawn(async move {
            run_recent_worker_with_fetch(worker_state, Exchange::Coinbase, Market::Spot, Symbol::Btc, rx, move |_| {
                let attempt = fetch_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(if attempt == 0 {
                        vec![trade("100", 1_000), trade("101", 1_001)]
                    } else {
                        vec![
                            trade("100", 1_000),
                            trade("101", 1_001),
                            trade("102", 1_002),
                            trade("103", 1_003),
                            trade("104", 1_004),
                        ]
                    })
                }
            })
            .await;
        });
        for id in 103..=105 {
            tx.send(trade(&id.to_string(), id + 900)).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if shard.recovery.read().await.state == "live" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(tx);
        worker.await.unwrap();
        let engine = shard.engine.lock().await;
        assert_eq!(engine.last_trade_id.as_deref(), Some("105"));
        assert_eq!(engine.sec.trades, 5);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn terminal_gap_isolated_and_survives_reconnect_without_refetch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let state = Arc::new(crate::tests::state().await);
        let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
        let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Eth);
        {
            let mut engine = btc.engine.lock().await;
            engine.last_trade_id = Some("missing".into());
            engine.last_trade_ms = Some(1_000);
        }
        let (btc_tx, btc_rx) = mpsc::channel(8);
        let (eth_tx, eth_rx) = mpsc::channel(8);
        let btc_worker = tokio::spawn(run_recent_worker_with_fetch(
            state.clone(),
            Exchange::Coinbase,
            Market::Spot,
            Symbol::Btc,
            btc_rx,
            |_| async { Ok(vec![trade("other", 1_050)]) },
        ));
        let eth_worker = tokio::spawn(run_recent_worker_with_fetch(
            state.clone(),
            Exchange::Coinbase,
            Market::Spot,
            Symbol::Eth,
            eth_rx,
            |_| async { panic!("fresh bootstrap must not fetch history") },
        ));
        btc_tx.send(trade("live-btc", 1_100)).await.unwrap();
        eth_tx.send(trade("live-eth", 1_100)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if btc.recovery.read().await.needs_backfill() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(btc_tx);
        drop(eth_tx);
        btc_worker.await.unwrap();
        eth_worker.await.unwrap();
        assert!(btc.recovery.read().await.needs_backfill());
        assert_eq!(eth.engine.lock().await.last_trade_id.as_deref(), Some("live-eth"));

        let calls = Arc::new(AtomicUsize::new(0));
        let reconnect_calls = calls.clone();
        let (tx, rx) = mpsc::channel(1);
        let reconnect = tokio::spawn(run_recent_worker_with_fetch(
            state,
            Exchange::Coinbase,
            Market::Spot,
            Symbol::Btc,
            rx,
            move |_| {
                reconnect_calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(Vec::new()) }
            },
        ));
        tx.send(trade("later", 1_200)).await.unwrap();
        drop(tx);
        reconnect.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn closing_transport_cancels_pending_recovery_worker() {
        let state = Arc::new(crate::tests::state().await);
        let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
        {
            let mut engine = shard.engine.lock().await;
            engine.last_trade_id = Some("100".into());
            engine.last_trade_ms = Some(1_000);
        }
        let (tx, rx) = mpsc::channel(1);
        let worker = tokio::spawn(run_recent_worker_with_fetch(
            state,
            Exchange::Coinbase,
            Market::Spot,
            Symbol::Btc,
            rx,
            |_| std::future::pending::<Result<Vec<Trade>>>(),
        ));
        tx.send(trade("101", 1_001)).await.unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_millis(100), worker)
            .await
            .expect("worker leaked after transport closed")
            .unwrap();
    }

    #[tokio::test]
    async fn terminal_state_cannot_be_cleared_by_late_success_or_market_errors() {
        let state = crate::tests::state().await;
        reject_source(
            &state,
            Exchange::Binance,
            Market::Spot,
            Symbol::Btc,
            "trade ID gap; historical backfill required".into(),
        )
        .await;
        mark_live(&state, Exchange::Binance, Market::Spot, Symbol::Btc).await;
        crate::mark_market_error(&state, Exchange::Binance, Market::Spot, "socket closed").await;
        let shard = crate::pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
        assert!(shard.recovery.read().await.needs_backfill());
        assert!(shard.stream_error.read().await.as_deref().unwrap().contains("needs_backfill"));
    }

    #[tokio::test]
    #[ignore = "opt-in public Coinbase REST/WebSocket handoff probe"]
    async fn public_coinbase_spot_handoff_probe() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::{connect_async, tungstenite::Message};

        let state = crate::tests::state().await;
        let (socket, _) = connect_async("wss://ws-feed.exchange.coinbase.com").await.unwrap();
        let (mut write, mut read) = socket.split();
        write
            .send(Message::Text(r#"{"type":"subscribe","product_ids":["BTC-USD"],"channels":["matches"]}"#.into()))
            .await
            .unwrap();
        let live = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let Message::Text(text) = read.next().await.unwrap().unwrap() else {
                    continue;
                };
                let Ok(row) = serde_json::from_str::<crate::models::CoinbaseMatch>(&text) else {
                    continue;
                };
                if row.msg_type == "match" && row.product_id == "BTC-USD" {
                    let price: f64 = row.price.parse().unwrap();
                    let base: f64 = row.size.parse().unwrap();
                    break Trade {
                        id: row.trade_id.to_string(),
                        timestamp_ms: crate::parse_iso8601_to_ms(&row.time).unwrap(),
                        price,
                        base,
                        quote: price * base,
                        is_buy: row.side == "sell",
                    };
                }
            }
        })
        .await
        .unwrap();
        let anchor = RecoveryAnchor { cursor: None, from_ms: live.timestamp_ms - 30_000, through_ms: 0 };
        let mut coordinator = RecoveryCoordinator::new(anchor.clone(), 8);
        coordinator.push_ws(live).unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let rows = fetch_recent(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, &anchor).await.unwrap();
                if matches!(coordinator.observe_rest(rows), RecoveryDecision::Ready(_)) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
        .await
        .expect("public REST never overlapped the buffered WebSocket trade");
    }
}
