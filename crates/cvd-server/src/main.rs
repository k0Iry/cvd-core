mod aggregator;
mod binance;
mod coinbase;
mod coinbase_perp;
mod config;
mod indicators;
mod ingestion;
mod live_indicators;
mod models;
mod persistence;
mod quality;
mod recovery;
#[cfg(test)]
mod retirement_tests;
mod types;
#[cfg(test)]
mod usdc_tests;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Query, State, WebSocketUpgrade},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::time::{sleep, Duration};
use tokio::try_join;
use tokio_tungstenite::connect_async;
use tower_http::services::ServeDir;

use models::{Bar, WsAction, WsPayload};
use types::{Exchange, Market, Symbol, Tf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarketCategory {
    Spot,
    Contract,
}

impl MarketCategory {
    fn from_market(market: &str) -> Self {
        if matches!(market, "spot" | "spot_usdc") {
            Self::Spot
        } else {
            Self::Contract
        }
    }

    fn matches_market(self, market: Market) -> bool {
        match self {
            Self::Spot => market.is_spot(),
            Self::Contract => !market.is_spot(),
        }
    }

    fn market_names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        for source in types::INSTRUMENTS {
            if self.matches_market(source.market) && !names.contains(&source.market.as_str()) {
                names.push(source.market.as_str());
            }
        }
        names
    }
}

const CONTRACT_SERIES: &str = "contracts-drb-v1";
const SPOT_SERIES: &str = "spot-v1";

fn series_for_market(market: Market) -> &'static str {
    if market.is_spot() {
        SPOT_SERIES
    } else {
        CONTRACT_SERIES
    }
}

#[derive(sqlx::FromRow)]
struct AggregateSeedRow {
    exchange: String,
    market: String,
    cvd_usdt: f64,
}

#[derive(sqlx::FromRow)]
struct CvdSeedRow {
    exchange: String,
    market: String,
    symbol: String,
    time: i64,
    cvd_usdt: f64,
}

#[derive(sqlx::FromRow)]
struct AggregateCvdRow {
    exchange: String,
    market: String,
    time: i64,
    cvd_usdt: f64,
}

#[derive(sqlx::FromRow)]
struct AggregateFlowRow {
    time: i64,
    price_open: f64,
    price_high: f64,
    price_low: f64,
    price_close: f64,
    delta_usdt: f64,
    buy_volume_base: f64,
    sell_volume_base: f64,
    buy_volume_usdt: f64,
    sell_volume_usdt: f64,
}

// ==============================
// Constants
// ==============================
const HISTORY_CAPACITY: usize = 1600;
const H1_HISTORY_CAPACITY: usize = 1400;
const BROADCAST_CHANNEL_CAPACITY: usize = 256;
const DB_CONNECTIONS: u32 = 4;
const DELAY_SECONDS: i64 = 60 * 60;
// ==============================
// State / Shard
// ==============================

#[derive(Clone)]
pub struct Shard {
    pub engine: Arc<Mutex<ingestion::SymbolState>>,
    pub stream_error: Arc<RwLock<Option<String>>>,
    pub recovery: Arc<RwLock<recovery::RecoveryDiagnostics>>,
    pub hist_1m: Arc<RwLock<VecDeque<Bar>>>,
    pub hist_5m: Arc<RwLock<VecDeque<Bar>>>,
    pub hist_15m: Arc<RwLock<VecDeque<Bar>>>,
    pub hist_30m: Arc<RwLock<VecDeque<Bar>>>,
    pub hist_1h: Arc<RwLock<VecDeque<Bar>>>,
    pub live_bar_1m: Arc<RwLock<Option<Bar>>>,
    pub live_bar_5m: Arc<RwLock<Option<Bar>>>,
    pub live_bar_15m: Arc<RwLock<Option<Bar>>>,
    pub live_bar_30m: Arc<RwLock<Option<Bar>>>,
    pub live_bar_1h: Arc<RwLock<Option<Bar>>>,
    pub tx_1m: broadcast::Sender<WsPayload>,
    pub tx_5m: broadcast::Sender<WsPayload>,
    pub tx_15m: broadcast::Sender<WsPayload>,
    pub tx_30m: broadcast::Sender<WsPayload>,
    pub tx_1h: broadcast::Sender<WsPayload>,
    pub cvd_closed_usdt: Arc<RwLock<f64>>,
    /// Monotonic input revision for every published bar version of this source.
    pub rev: Arc<std::sync::atomic::AtomicU64>,
    /// Per-tf publication lock holding the `rev` of the newest base frame
    /// published for that tf (0 = none yet). Held across cache mutation, rev
    /// assignment, hub notify and broadcast, and by snapshot readers, so a
    /// snapshot's content and revision are coherent and tf-specific.
    pub published: Arc<[tokio::sync::Mutex<u64>; 5]>,
    /// Times of canonical bars dropped as non-renderable (per tf), recorded
    /// before filtering so indicator windows covering them fail closed.
    pub invalid_times: Arc<std::sync::Mutex<HashMap<Tf, std::collections::BTreeSet<i64>>>>,
}

fn mk_shard() -> Shard {
    let (tx_1m, _) = broadcast::channel::<WsPayload>(BROADCAST_CHANNEL_CAPACITY);
    let (tx_5m, _) = broadcast::channel::<WsPayload>(BROADCAST_CHANNEL_CAPACITY);
    let (tx_15m, _) = broadcast::channel::<WsPayload>(BROADCAST_CHANNEL_CAPACITY);
    let (tx_30m, _) = broadcast::channel::<WsPayload>(BROADCAST_CHANNEL_CAPACITY);
    let (tx_1h, _) = broadcast::channel::<WsPayload>(BROADCAST_CHANNEL_CAPACITY);
    Shard {
        engine: Arc::new(Mutex::new(ingestion::SymbolState::new())),
        stream_error: Arc::new(RwLock::new(Some("starting".into()))),
        recovery: Arc::new(RwLock::new(recovery::RecoveryDiagnostics::default())),
        hist_1m: Arc::new(RwLock::new(VecDeque::with_capacity(HISTORY_CAPACITY))),
        hist_5m: Arc::new(RwLock::new(VecDeque::with_capacity(HISTORY_CAPACITY))),
        hist_15m: Arc::new(RwLock::new(VecDeque::with_capacity(HISTORY_CAPACITY))),
        hist_30m: Arc::new(RwLock::new(VecDeque::with_capacity(HISTORY_CAPACITY))),
        hist_1h: Arc::new(RwLock::new(VecDeque::with_capacity(HISTORY_CAPACITY))),
        live_bar_1m: Arc::new(RwLock::new(None)),
        live_bar_5m: Arc::new(RwLock::new(None)),
        live_bar_15m: Arc::new(RwLock::new(None)),
        live_bar_30m: Arc::new(RwLock::new(None)),
        live_bar_1h: Arc::new(RwLock::new(None)),
        tx_1m,
        tx_5m,
        tx_15m,
        tx_30m,
        tx_1h,
        cvd_closed_usdt: Arc::new(RwLock::new(0.0)),
        rev: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        published: Arc::new(std::array::from_fn(|_| tokio::sync::Mutex::new(0))),
        invalid_times: Arc::new(std::sync::Mutex::new(HashMap::new())),
    }
}

impl Shard {
    fn for_tf(&self, tf: Tf) -> (&RwLock<VecDeque<Bar>>, &broadcast::Sender<WsPayload>) {
        match tf {
            Tf::M1 => (&self.hist_1m, &self.tx_1m),
            Tf::M5 => (&self.hist_5m, &self.tx_5m),
            Tf::M15 => (&self.hist_15m, &self.tx_15m),
            Tf::M30 => (&self.hist_30m, &self.tx_30m),
            Tf::H1 => (&self.hist_1h, &self.tx_1h),
        }
    }

    pub fn mark_invalid(&self, tf: Tf, time: i64) {
        self.invalid_times.lock().unwrap().entry(tf).or_default().insert(time);
    }

    /// A validated (renderable) version of the same bucket replaces the invalid one.
    pub fn clear_invalid(&self, tf: Tf, time: i64) {
        if let Some(s) = self.invalid_times.lock().unwrap().get_mut(&tf) {
            s.remove(&time);
        }
    }

    fn prune_invalid_before(&self, tf: Tf, time: i64) {
        if let Some(s) = self.invalid_times.lock().unwrap().get_mut(&tf) {
            *s = s.split_off(&time);
        }
    }

    pub fn invalid_times_for(&self, tf: Tf) -> Vec<i64> {
        self.invalid_times.lock().unwrap().get(&tf).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    pub fn published_for(&self, tf: Tf) -> &tokio::sync::Mutex<u64> {
        &self.published[match tf {
            Tf::M1 => 0,
            Tf::M5 => 1,
            Tf::M15 => 2,
            Tf::M30 => 3,
            Tf::H1 => 4,
        }]
    }

    fn live_bar_for_tf(&self, tf: Tf) -> &RwLock<Option<Bar>> {
        match tf {
            Tf::M1 => &self.live_bar_1m,
            Tf::M5 => &self.live_bar_5m,
            Tf::M15 => &self.live_bar_15m,
            Tf::M30 => &self.live_bar_30m,
            Tf::H1 => &self.live_bar_1h,
        }
    }
}

pub struct AppState {
    pub shards: Arc<HashMap<(Exchange, Market, Symbol), Shard>>,
    pub db: PgPool,
    pub outbox: Arc<persistence::Outbox>,
    pub http: reqwest::Client,
    pub ready: AtomicBool,
    pub access_key: Option<String>,
    pub indicators: Arc<indicators::IndicatorManager>,
    pub hubs: Arc<live_indicators::LiveHubs>,
    pub static_dir: std::path::PathBuf,
}

fn pick_shard(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol) -> &Shard {
    state.shards.get(&(exchange, market, symbol)).expect("shard missing")
}

fn has_realtime_access(state: &AppState, key: Option<&str>) -> bool {
    match state.access_key.as_deref() {
        None => true,
        Some(expected) => key.is_some_and(|candidate| candidate == expected),
    }
}

// Helper: Get valid (market, symbol) pairs for a given exchange
fn get_market_symbol_pairs(exchange: Exchange) -> Vec<(Market, Symbol)> {
    types::INSTRUMENTS.iter().filter(|i| i.exchange == exchange).map(|i| (i.market, i.symbol)).collect()
}

// ==============================
// HTTP params
// ==============================

fn active_market_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for exchange in Exchange::ALL {
        for (market, _) in get_market_symbol_pairs(exchange) {
            let key = format!("{}:{}", exchange.as_str(), market.as_str());
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

#[derive(Deserialize)]
struct Params {
    exchange: Option<String>,
    market: Option<String>,
    series: Option<String>,
    symbol: Option<String>,
    tf: Option<String>,
    key: Option<String>,
    limit: Option<usize>,
    before: Option<i64>,
    after: Option<i64>,
    /// Comma-separated installed indicator ids; absent = all configured, empty = none.
    indicators: Option<String>,
}

#[derive(Serialize)]
struct ExchangeMeta {
    exchange: String,
    markets: Vec<String>,
    symbols: Vec<String>,
}

#[derive(Serialize)]
struct MetaPayload {
    indicators: Vec<indicators::IndicatorInfo>,
    exchanges: Vec<ExchangeMeta>,
    instruments: &'static [types::Instrument],
    contract_series: &'static str,
    spot_series: &'static str,
    coinbase_contract_source: &'static str,
}

// ==============================
// DB init
// ==============================

async fn init_db(db: &PgPool) -> Result<()> {
    // Postgres schema (same logical schema as sqlite version)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS bars (
            exchange TEXT NOT NULL,
            market TEXT NOT NULL,
            symbol TEXT NOT NULL,
            tf TEXT NOT NULL,
            time BIGINT NOT NULL,
            price_open DOUBLE PRECISION NOT NULL,
            price_high DOUBLE PRECISION NOT NULL,
            price_low DOUBLE PRECISION NOT NULL,
            price_close DOUBLE PRECISION NOT NULL,
            delta_usdt DOUBLE PRECISION NOT NULL,
            cvd_usdt DOUBLE PRECISION NOT NULL,
            buy_volume_base DOUBLE PRECISION NOT NULL DEFAULT 0,
            sell_volume_base DOUBLE PRECISION NOT NULL DEFAULT 0,
            buy_volume_usdt DOUBLE PRECISION NOT NULL DEFAULT 0,
            sell_volume_usdt DOUBLE PRECISION NOT NULL DEFAULT 0,
            trades BIGINT NOT NULL,
            PRIMARY KEY (exchange, market, symbol, tf, time)
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS price_open DOUBLE PRECISION"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS price_high DOUBLE PRECISION"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS price_low DOUBLE PRECISION"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS buy_volume_base DOUBLE PRECISION"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS sell_volume_base DOUBLE PRECISION"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS buy_volume_usdt DOUBLE PRECISION NOT NULL DEFAULT 0"#)
        .execute(db)
        .await?;
    sqlx::query(r#"ALTER TABLE bars ADD COLUMN IF NOT EXISTS sell_volume_usdt DOUBLE PRECISION NOT NULL DEFAULT 0"#)
        .execute(db)
        .await?;
    sqlx::query(
        r#"
        UPDATE bars SET
            price_open = COALESCE(price_open, price_close),
            price_high = COALESCE(price_high, price_close),
            price_low = COALESCE(price_low, price_close),
            buy_volume_base = COALESCE(
                buy_volume_base,
                CASE WHEN price_close > 0 THEN buy_volume_usdt / price_close ELSE 0 END
            ),
            sell_volume_base = COALESCE(
                sell_volume_base,
                CASE WHEN price_close > 0 THEN sell_volume_usdt / price_close ELSE 0 END
            )
        WHERE price_open IS NULL
           OR price_high IS NULL
           OR price_low IS NULL
           OR buy_volume_base IS NULL
           OR sell_volume_base IS NULL
        "#,
    )
    .execute(db)
    .await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN price_open SET NOT NULL"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN price_high SET NOT NULL"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN price_low SET NOT NULL"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN buy_volume_base SET NOT NULL"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN sell_volume_base SET NOT NULL"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN buy_volume_base SET DEFAULT 0"#).execute(db).await?;
    sqlx::query(r#"ALTER TABLE bars ALTER COLUMN sell_volume_base SET DEFAULT 0"#).execute(db).await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_bars_query
        ON bars(exchange, market, symbol, tf, time DESC);
        "#,
    )
    .execute(db)
    .await?;

    Ok(())
}

async fn load_history(
    db: &PgPool,
    exchange: &str,
    market: &str,
    symbol: &str,
    tf: &str,
    limit: usize,
) -> Result<(VecDeque<Bar>, f64)> {
    let rows: Vec<models::BarRow> = sqlx::query_as(
        r#"
        SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
        FROM bars
        WHERE exchange = $1 AND market = $2 AND symbol = $3 AND tf = $4
        ORDER BY time DESC
        LIMIT $5
        "#,
    )
    .bind(exchange)
    .bind(market)
    .bind(symbol)
    .bind(tf)
    .bind(limit as i64)
    .fetch_all(db)
    .await?;

    let (v, last_cvd) = convert_rows_to_bars(rows);
    Ok((VecDeque::from(v), last_cvd))
}

fn convert_rows_to_bars(rows: Vec<models::BarRow>) -> (Vec<Bar>, f64) {
    let mut v: Vec<Bar> = rows.into_iter().map(Bar::from).collect();
    v.reverse();
    let last_cvd = v.last().map(|b| b.cvd_usdt).unwrap_or(0.0);
    (v, last_cvd)
}

async fn apply_aggregate_cvd_to_bars(
    db: &PgPool,
    bars: &mut [Bar],
    symbol: &str,
    market_category: MarketCategory,
    tf: &str,
) -> Result<()> {
    if bars.is_empty() {
        return Ok(());
    }

    let start_time = bars.first().map(|b| b.time).unwrap_or_default();
    let end_time = bars.last().map(|b| b.time).unwrap_or_default();
    let market_names: Vec<&str> = market_category.market_names().to_vec();

    let seed_rows: Vec<AggregateSeedRow> = sqlx::query_as(
        r#"
        SELECT DISTINCT ON (exchange, market)
            exchange, market, cvd_usdt
        FROM bars
        WHERE symbol = $1 AND tf = $2
          AND market = ANY($3)
          AND time < $4 AND (exchange || ':' || market) = ANY($5)
        ORDER BY exchange, market, time DESC
        "#,
    )
    .bind(symbol)
    .bind(tf)
    .bind(&market_names)
    .bind(start_time)
    .bind(active_market_keys())
    .fetch_all(db)
    .await?;

    let mut states: HashMap<(String, String), f64> =
        seed_rows.into_iter().map(|row| ((row.exchange, row.market), row.cvd_usdt)).collect();

    let rows: Vec<AggregateCvdRow> = sqlx::query_as(
        r#"
        SELECT exchange, market, time, cvd_usdt
        FROM bars
        WHERE symbol = $1 AND tf = $2
          AND market = ANY($3)
          AND time >= $4 AND time <= $5
          AND (exchange || ':' || market) = ANY($6)
        ORDER BY time ASC, exchange ASC, market ASC
        "#,
    )
    .bind(symbol)
    .bind(tf)
    .bind(&market_names)
    .bind(start_time)
    .bind(end_time)
    .bind(active_market_keys())
    .fetch_all(db)
    .await?;

    let mut next_row = 0usize;
    for bar in bars.iter_mut() {
        while next_row < rows.len() && rows[next_row].time <= bar.time {
            let row = &rows[next_row];
            states.insert((row.exchange.clone(), row.market.clone()), row.cvd_usdt);
            next_row += 1;
        }
        bar.aggregate_cvd_usdt = states.values().copied().sum::<f64>();
    }

    Ok(())
}

async fn apply_aggregate_flow_to_bars(
    db: &PgPool,
    bars: &mut [Bar],
    symbol: &str,
    market_category: MarketCategory,
    tf: &str,
) -> Result<()> {
    if bars.is_empty() {
        return Ok(());
    }

    let start_time = bars.first().map(|b| b.time).unwrap_or_default();
    let end_time = bars.last().map(|b| b.time).unwrap_or_default();
    let market_names: Vec<&str> = market_category.market_names().to_vec();

    let rows: Vec<AggregateFlowRow> = sqlx::query_as(
        r#"
        SELECT
            time,
            COALESCE(
                SUM(price_open * (buy_volume_base + sell_volume_base)) /
                    NULLIF(SUM(buy_volume_base + sell_volume_base), 0),
                AVG(price_open)
            ) AS price_open,
            MAX(price_high) AS price_high,
            MIN(price_low) AS price_low,
            COALESCE(
                SUM(price_close * (buy_volume_base + sell_volume_base)) /
                    NULLIF(SUM(buy_volume_base + sell_volume_base), 0),
                AVG(price_close)
            ) AS price_close,
            SUM(delta_usdt) AS delta_usdt,
            SUM(buy_volume_base) AS buy_volume_base,
            SUM(sell_volume_base) AS sell_volume_base,
            SUM(buy_volume_usdt) AS buy_volume_usdt,
            SUM(sell_volume_usdt) AS sell_volume_usdt
        FROM bars
        WHERE symbol = $1 AND tf = $2
          AND market = ANY($3)
          AND time >= $4 AND time <= $5
          AND (exchange || ':' || market) = ANY($6)
        GROUP BY time
        ORDER BY time ASC
        "#,
    )
    .bind(symbol)
    .bind(tf)
    .bind(&market_names)
    .bind(start_time)
    .bind(end_time)
    .bind(active_market_keys())
    .fetch_all(db)
    .await?;

    let by_time: HashMap<i64, (f64, f64, f64, f64, f64, f64, f64, f64, f64)> = rows
        .into_iter()
        .map(|row| {
            (
                row.time,
                (
                    row.price_open,
                    row.price_high,
                    row.price_low,
                    row.price_close,
                    row.delta_usdt,
                    row.buy_volume_base,
                    row.sell_volume_base,
                    row.buy_volume_usdt,
                    row.sell_volume_usdt,
                ),
            )
        })
        .collect();

    for bar in bars.iter_mut() {
        let (open, high, low, close, delta, buy_base, sell_base, buy, sell) = by_time
            .get(&bar.time)
            .copied()
            .unwrap_or((bar.price_open, bar.price_high, bar.price_low, bar.price_close, 0.0, 0.0, 0.0, 0.0, 0.0));
        bar.aggregate_price_open = open;
        bar.aggregate_price_high = high;
        bar.aggregate_price_low = low;
        bar.aggregate_price_close = close;
        bar.aggregate_delta_usdt = delta;
        bar.aggregate_buy_volume_base = buy_base;
        bar.aggregate_sell_volume_base = sell_base;
        bar.aggregate_buy_volume_usdt = buy;
        bar.aggregate_sell_volume_usdt = sell;
    }

    Ok(())
}

async fn apply_live_aggregates(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
    current: &mut Bar,
) {
    let category = MarketCategory::from_market(market.as_str());
    let mut observations = Vec::new();
    let mut cvd = 0.0;
    let mut flow = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut weighted_open = 0.0;
    let mut weighted_close = 0.0;
    let mut total_base = 0.0;
    let mut high = f64::NEG_INFINITY;
    let mut low = f64::INFINITY;
    let mut observe = |bar: &Bar| {
        cvd += bar.cvd_usdt;
        if bar.time != current.time {
            return;
        }
        flow.0 += bar.delta_usdt;
        flow.1 += bar.buy_volume_base;
        flow.2 += bar.sell_volume_base;
        flow.3 += bar.buy_volume_usdt;
        flow.4 += bar.sell_volume_usdt;
        let base = bar.buy_volume_base + bar.sell_volume_base;
        if base > 0.0 && base.is_finite() {
            weighted_open += bar.price_open * base;
            weighted_close += bar.price_close * base;
            total_base += base;
        }
        high = high.max(bar.price_high);
        low = low.min(bar.price_low);
    };
    for ((source, source_market, source_symbol), shard) in state.shards.iter() {
        if *source_symbol != symbol || !category.matches_market(*source_market) {
            continue;
        }
        if *source == exchange && *source_market == market {
            observe(current);
            observations.push((*source, *source_market, Some(current.time)));
            continue;
        }
        let (history, _) = shard.for_tf(tf);
        let history = history.read().await;
        let live = shard.live_bar_for_tf(tf).read().await;
        let closed = history.iter().rev().find(|bar| bar.time <= current.time);
        let live = live.as_ref().filter(|bar| bar.time <= current.time);
        let observed = closed.into_iter().chain(live).max_by_key(|bar| bar.time);
        observations.push((*source, *source_market, observed.map(|bar| bar.time)));
        if let Some(bar) = observed {
            observe(bar);
        }
    }
    current.quality =
        Some(quality::BarQuality::observe(symbol, category, current.time, observations, Utc::now().timestamp_millis()));
    current.aggregate_cvd_usdt = cvd;
    current.aggregate_delta_usdt = flow.0;
    current.aggregate_buy_volume_base = flow.1;
    current.aggregate_sell_volume_base = flow.2;
    current.aggregate_buy_volume_usdt = flow.3;
    current.aggregate_sell_volume_usdt = flow.4;
    current.aggregate_price_open = if total_base > 0.0 { weighted_open / total_base } else { current.price_open };
    current.aggregate_price_close = if total_base > 0.0 { weighted_close / total_base } else { current.price_close };
    current.aggregate_price_high = high;
    current.aggregate_price_low = low;
}

#[cfg(test)]
async fn aggregate_live_cvd(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
    current_bar: &Bar,
) -> f64 {
    let category = MarketCategory::from_market(market.as_str());

    let mut total = 0.0;
    for ((src_exchange, src_market, src_symbol), shard) in state.shards.iter() {
        if *src_symbol != symbol || !category.matches_market(*src_market) {
            continue;
        }

        if *src_exchange == exchange && *src_market == market {
            total += current_bar.cvd_usdt;
            continue;
        }

        let (hist, _) = shard.for_tf(tf);
        let h = hist.read().await;
        let live = shard.live_bar_for_tf(tf).read().await;
        let latest_closed = h.iter().rev().find(|bar| bar.time <= current_bar.time);
        let latest_live = live.as_ref().filter(|bar| bar.time <= current_bar.time);
        if let Some(bar) = latest_closed.into_iter().chain(latest_live).max_by_key(|bar| bar.time) {
            total += bar.cvd_usdt;
        }
    }

    total
}

#[cfg(test)]
async fn aggregate_live_flow(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
    current_bar: &Bar,
) -> (f64, f64, f64, f64, f64) {
    let category = MarketCategory::from_market(market.as_str());

    let mut delta_total = 0.0;
    let mut buy_base_total = 0.0;
    let mut sell_base_total = 0.0;
    let mut buy_total = 0.0;
    let mut sell_total = 0.0;

    for ((src_exchange, src_market, src_symbol), shard) in state.shards.iter() {
        if *src_symbol != symbol || !category.matches_market(*src_market) {
            continue;
        }

        let bar_opt = if *src_exchange == exchange && *src_market == market {
            Some(current_bar.clone())
        } else {
            let live = shard.live_bar_for_tf(tf).read().await;
            if let Some(live_bar) = live.as_ref() {
                if live_bar.time == current_bar.time {
                    Some(live_bar.clone())
                } else {
                    drop(live);
                    let (hist, _) = shard.for_tf(tf);
                    let h = hist.read().await;
                    h.iter().rev().find(|bar| bar.time == current_bar.time).cloned()
                }
            } else {
                drop(live);
                let (hist, _) = shard.for_tf(tf);
                let h = hist.read().await;
                h.iter().rev().find(|bar| bar.time == current_bar.time).cloned()
            }
        };

        if let Some(bar) = bar_opt {
            delta_total += bar.delta_usdt;
            buy_base_total += bar.buy_volume_base;
            sell_base_total += bar.sell_volume_base;
            buy_total += bar.buy_volume_usdt;
            sell_total += bar.sell_volume_usdt;
        }
    }

    (delta_total, buy_base_total, sell_base_total, buy_total, sell_total)
}

#[cfg(test)]
async fn aggregate_live_ohlc(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
    current_bar: &Bar,
) -> Option<(f64, f64, f64, f64)> {
    let category = MarketCategory::from_market(market.as_str());

    let mut bars = Vec::new();
    for ((src_exchange, src_market, src_symbol), shard) in state.shards.iter() {
        if *src_symbol != symbol || !category.matches_market(*src_market) {
            continue;
        }

        let bar_opt = if *src_exchange == exchange && *src_market == market {
            Some(current_bar.clone())
        } else {
            let live = shard.live_bar_for_tf(tf).read().await;
            if let Some(live_bar) = live.as_ref() {
                if live_bar.time == current_bar.time {
                    Some(live_bar.clone())
                } else {
                    drop(live);
                    let (hist, _) = shard.for_tf(tf);
                    let h = hist.read().await;
                    h.iter().rev().find(|bar| bar.time == current_bar.time).cloned()
                }
            } else {
                drop(live);
                let (hist, _) = shard.for_tf(tf);
                let h = hist.read().await;
                h.iter().rev().find(|bar| bar.time == current_bar.time).cloned()
            }
        };

        if let Some(bar) = bar_opt {
            bars.push(bar);
        }
    }

    if bars.is_empty() {
        return None;
    }

    let mut weighted_open = 0.0;
    let mut weighted_close = 0.0;
    let mut total_base = 0.0;
    for bar in &bars {
        let base = bar.buy_volume_base + bar.sell_volume_base;
        if base > 0.0 && base.is_finite() {
            weighted_open += bar.price_open * base;
            weighted_close += bar.price_close * base;
            total_base += base;
        }
    }

    let high = bars.iter().map(|bar| bar.price_high).fold(f64::NEG_INFINITY, f64::max);
    let low = bars.iter().map(|bar| bar.price_low).fold(f64::INFINITY, f64::min);
    let open = if total_base > 0.0 { weighted_open / total_base } else { bars.first()?.price_open };
    let close = if total_base > 0.0 { weighted_close / total_base } else { bars.last()?.price_close };

    Some((open, high, low, close))
}

async fn load_history_page(
    db: &PgPool,
    exchange: &str,
    symbol: &str,
    market: &str,
    tf: &str,
    limit: usize,
    before: Option<i64>,
    after: Option<i64>,
    max_time: Option<i64>,
    realtime_enabled: bool,
    mgr: &indicators::IndicatorManager,
    indicator_ids: &[String],
) -> Result<models::HistoryPayload> {
    let limit = limit.clamp(1, HISTORY_CAPACITY);
    // Warm-up rows come only from the requested indicators' declared context.
    let render_context = mgr.max_context(indicator_ids, tf);

    // 同时传 before + after 直接拒绝，避免歧义
    if before.is_some() && after.is_some() {
        anyhow::bail!("before and after cannot both be set");
    }

    let (rows, has_more_before, has_more_after): (Vec<models::BarRow>, bool, bool) = if let Some(before_ts) = before {
        let rows: Vec<models::BarRow> = if let Some(max_time) = max_time {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time < $5 AND time <= $6
            ORDER BY time DESC
            LIMIT $7
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(before_ts)
            .bind(max_time)
            .bind((limit + render_context + 1) as i64)
            .fetch_all(db)
            .await?
        } else {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time < $5
            ORDER BY time DESC
            LIMIT $6
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(before_ts)
            .bind((limit + render_context + 1) as i64)
            .fetch_all(db)
            .await?
        };
        let has_more_before = rows.len() > limit;
        (rows, has_more_before, after.is_some() || max_time.is_some())
    } else if let Some(after_ts) = after {
        let mut context_rows: Vec<models::BarRow> = if let Some(max_time) = max_time {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time <= $5 AND time <= $6
            ORDER BY time DESC
            LIMIT $7
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(after_ts)
            .bind(max_time)
            .bind(render_context as i64)
            .fetch_all(db)
            .await?
        } else {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time <= $5
            ORDER BY time DESC
            LIMIT $6
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(after_ts)
            .bind(render_context as i64)
            .fetch_all(db)
            .await?
        };
        context_rows.reverse();
        let mut newer_rows: Vec<models::BarRow> = if let Some(max_time) = max_time {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time > $5 AND time <= $6
            ORDER BY time ASC
            LIMIT $7
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(after_ts)
            .bind(max_time)
            .bind((limit + 1) as i64)
            .fetch_all(db)
            .await?
        } else {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time > $5
            ORDER BY time ASC
            LIMIT $6
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(after_ts)
            .bind((limit + 1) as i64)
            .fetch_all(db)
            .await?
        };
        let has_more_after = newer_rows.len() > limit;
        if has_more_after {
            newer_rows.pop();
        }
        (context_rows.into_iter().chain(newer_rows).collect(), true, has_more_after)
    } else {
        let rows: Vec<models::BarRow> = if let Some(max_time) = max_time {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
              AND time <= $5
            ORDER BY time DESC
            LIMIT $6
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind(max_time)
            .bind((limit + render_context + 1) as i64)
            .fetch_all(db)
            .await?
        } else {
            sqlx::query_as(
                r#"
            SELECT time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades
            FROM bars
            WHERE exchange = $1 AND symbol = $2 AND market = $3 AND tf = $4
            ORDER BY time DESC
            LIMIT $5
            "#,
            )
            .bind(exchange)
            .bind(symbol)
            .bind(market)
            .bind(tf)
            .bind((limit + render_context + 1) as i64)
            .fetch_all(db)
            .await?
        };
        let has_more_before = rows.len() > limit;
        (rows, has_more_before, false)
    };

    let mut v: Vec<Bar> = rows.into_iter().map(Bar::from).collect();

    // 只有 before/none 才 reverse
    if after.is_none() {
        v.reverse();
    }

    apply_aggregate_cvd_to_bars(db, &mut v, symbol, MarketCategory::from_market(market), tf).await?;
    apply_aggregate_flow_to_bars(db, &mut v, symbol, MarketCategory::from_market(market), tf).await?;
    let invalid_times = invalid_bar_times(&v);
    v.retain(Bar::is_renderable);

    let keep_start = history_keep_start(&v, limit, after);
    let scope = indicators::ScopeBase {
        exchange: exchange.to_string(),
        market: market.to_string(),
        symbol: symbol.to_string(),
        tf: tf.to_string(),
    };
    let indicator_outputs =
        indicators::compute_history_guarded(mgr, indicator_ids, &scope, 0, &v, keep_start, &invalid_times).await;
    let has_more_before = trim_history_page(&mut v, limit, after, has_more_before);

    let oldest_time = v.first().map(|b| b.time);
    let newest_time = v.last().map(|b| b.time);

    Ok(models::HistoryPayload {
        series: if MarketCategory::from_market(market) == MarketCategory::Spot { SPOT_SERIES } else { CONTRACT_SERIES },
        bars: v,
        has_more_before,
        has_more_after,
        oldest_time,
        newest_time,
        realtime_enabled,
        indicator_api_version: indicators::API_VERSION,
        indicators: indicator_outputs,
    })
}

/// Index of the first bar kept by [`trim_history_page`]; earlier bars are
/// warm-up context only.
/// Times of rows that are not renderable; collected BEFORE filtering.
fn invalid_bar_times<'a>(bars: impl IntoIterator<Item = &'a Bar>) -> Vec<i64> {
    bars.into_iter().filter(|b| !b.is_renderable()).map(|b| b.time).collect()
}

fn history_keep_start(bars: &[Bar], limit: usize, after: Option<i64>) -> usize {
    match after {
        Some(after) => bars.partition_point(|bar| bar.time <= after),
        None => bars.len().saturating_sub(limit),
    }
}

/// Resolve requested ids against installed indicators only (unknown ids are
/// ignored; no path/URL is ever accepted from clients).
fn requested_indicators(mgr: &indicators::IndicatorManager, raw: Option<&str>) -> Vec<String> {
    let installed = mgr.ids();
    match raw {
        None => installed,
        Some(list) => {
            let mut ids: Vec<String> = Vec::new();
            for id in list.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                if installed.iter().any(|i| i == id) && !ids.iter().any(|i| i == id) {
                    ids.push(id.to_string());
                }
            }
            ids
        }
    }
}

fn trim_history_page(bars: &mut Vec<Bar>, limit: usize, after: Option<i64>, has_more_before: bool) -> bool {
    if let Some(after) = after {
        bars.retain(|bar| bar.time > after);
        has_more_before
    } else {
        let has_more = bars.len() > limit;
        bars.drain(..bars.len().saturating_sub(limit));
        has_more
    }
}

async fn hydrate_shard(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol) -> Result<()> {
    let e = exchange.as_str();
    let m = market.as_str();
    let s = symbol.as_str();
    let ((h1, _last1), (h5, _last5), (h15, _last15), (h30, _last30), (h1h, _last1h)) = try_join!(
        load_history(&state.db, e, m, s, "1m", HISTORY_CAPACITY),
        load_history(&state.db, e, m, s, "5m", HISTORY_CAPACITY),
        load_history(&state.db, e, m, s, "15m", HISTORY_CAPACITY),
        load_history(&state.db, e, m, s, "30m", HISTORY_CAPACITY),
        load_history(&state.db, e, m, s, "1h", H1_HISTORY_CAPACITY)
    )?;
    let mut h1 = h1;
    let mut h5 = h5;
    let mut h15 = h15;
    let mut h30 = h30;
    let mut h1h = h1h;
    let category = MarketCategory::from_market(market.as_str());
    apply_aggregate_cvd_to_bars(&state.db, h1.make_contiguous(), s, category, "1m").await?;
    apply_aggregate_cvd_to_bars(&state.db, h5.make_contiguous(), s, category, "5m").await?;
    apply_aggregate_cvd_to_bars(&state.db, h15.make_contiguous(), s, category, "15m").await?;
    apply_aggregate_cvd_to_bars(&state.db, h30.make_contiguous(), s, category, "30m").await?;
    apply_aggregate_cvd_to_bars(&state.db, h1h.make_contiguous(), s, category, "1h").await?;
    apply_aggregate_flow_to_bars(&state.db, h1.make_contiguous(), s, category, "1m").await?;
    apply_aggregate_flow_to_bars(&state.db, h5.make_contiguous(), s, category, "5m").await?;
    apply_aggregate_flow_to_bars(&state.db, h15.make_contiguous(), s, category, "15m").await?;
    apply_aggregate_flow_to_bars(&state.db, h30.make_contiguous(), s, category, "30m").await?;
    apply_aggregate_flow_to_bars(&state.db, h1h.make_contiguous(), s, category, "1h").await?;
    for (tf, loaded) in [(Tf::M1, h1), (Tf::M5, h5), (Tf::M15, h15), (Tf::M30, h30), (Tf::H1, h1h)] {
        install_history(state, exchange, market, symbol, tf, loaded).await;
    }
    Ok(())
}

/// Merge loaded (warm-up / repaired) history into the cache under the tf
/// publication lock, then tell an already-running live hub of the scope to
/// reset and replay its bounded window from the new context. One signal per
/// installation; never a per-tick history transfer or per-viewer recompute.
pub(crate) async fn install_history(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
    mut loaded: VecDeque<Bar>,
) {
    let sh = pick_shard(state, exchange, market, symbol);
    {
        let _published = sh.published_for(tf).lock().await;
        admit_loaded_history(sh, tf, &mut loaded);
        let mut hist = sh.for_tf(tf).0.write().await;
        merge_history(&mut hist, loaded);
    }
    state.hubs.context_changed((exchange, market, symbol, tf));
}

/// Startup hydration: record invalid rows BEFORE dropping them from the cache.
fn admit_loaded_history(sh: &Shard, tf: Tf, h: &mut VecDeque<Bar>) {
    for t in invalid_bar_times(h.iter()) {
        sh.mark_invalid(tf, t);
    }
    h.retain(Bar::is_renderable);
}

fn merge_history(target: &mut VecDeque<Bar>, loaded: VecDeque<Bar>) {
    let mut by_time = BTreeMap::new();
    for bar in loaded {
        by_time.insert(bar.time, bar);
    }
    for bar in target.drain(..) {
        // Keep bars collected after process start if they overlap DB history.
        by_time.insert(bar.time, bar);
    }

    let skip = by_time.len().saturating_sub(HISTORY_CAPACITY);
    *target = by_time.into_iter().skip(skip).map(|(_, bar)| bar).collect();
}

async fn seed_cvd_closed_from_db(state: &AppState) -> Result<()> {
    let rows: Vec<CvdSeedRow> = sqlx::query_as(
        r#"
        SELECT DISTINCT ON (exchange, market, symbol)
            exchange, market, symbol, time, cvd_usdt
        FROM bars
        WHERE tf = '1m' AND (exchange || ':' || market) = ANY($1)
        ORDER BY exchange, market, symbol, time DESC
        "#,
    )
    .bind(active_market_keys())
    .fetch_all(&state.db)
    .await?;

    for row in rows {
        let Some(exchange) = Exchange::from_str(&row.exchange) else {
            continue;
        };
        let market = Market::from_str(&row.market);
        let symbol = Symbol::from_str(&row.symbol);
        if row.market != market.as_str() || row.symbol != symbol.as_str() {
            continue;
        }
        if let Some(shard) = state.shards.get(&(exchange, market, symbol)) {
            let (mut minutes, _) =
                load_history(&state.db, exchange.as_str(), market.as_str(), symbol.as_str(), "1m", 60).await?;
            let mut engine = ingestion::SymbolState::new();
            engine.cvd_closed_usdt = row.cvd_usdt;
            engine.recovery_from_ms = Some((row.time + 1) * 1000);
            engine.restore_minutes(minutes.make_contiguous(), row.time);
            *shard.cvd_closed_usdt.write().await = row.cvd_usdt;
            *shard.engine.lock().await = engine;
        }
    }

    Ok(())
}

fn spawn_market_engines(state: Arc<AppState>) {
    for exchange in Exchange::ALL {
        let mut markets = Vec::new();
        for source in types::INSTRUMENTS.iter().filter(|source| source.exchange == exchange) {
            if !markets.contains(&source.market) {
                markets.push(source.market);
            }
        }
        for market in markets {
            let state_clone = state.clone();
            match exchange {
                Exchange::Binance => {
                    tokio::spawn(binance_market_engine_loop(state_clone, market));
                }
                Exchange::Coinbase => {
                    tokio::spawn(coinbase_market_engine_loop(state_clone, market));
                }
            }
        }
    }
}

async fn hydrate_all_shards(state: Arc<AppState>) {
    for exchange in Exchange::ALL {
        for (market, symbol) in get_market_symbol_pairs(exchange) {
            if let Err(err) = hydrate_shard(&state, exchange, market, symbol).await {
                eprintln!(
                    "hydrate failed exchange={} market={} symbol={} err={err}",
                    exchange.as_str(),
                    market.as_str(),
                    symbol.as_str()
                );
            }
        }
    }
}

fn market_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(15)).user_agent("cvd/0.1").build()?)
}

async fn startup_data_pipeline(state: Arc<AppState>) {
    loop {
        let recovered = async {
            state.outbox.drain(&state.db).await?;
            seed_cvd_closed_from_db(&state).await
        }
        .await;
        match recovered {
            Ok(()) => break,
            Err(err) => {
                eprintln!("startup recovery failed; ingestion remains stopped: {err}");
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
    tokio::spawn(state.outbox.clone().run(state.db.clone()));
    spawn_market_engines(state.clone());
    state.ready.store(true, Ordering::Release);
    hydrate_all_shards(state).await;
}

// ==============================
// Main
// ==============================

#[tokio::main]
async fn main() -> Result<()> {
    // Postgres URL from env
    // Example:
    let config = config::Config::from_env()?;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    let db = PgPoolOptions::new().max_connections(DB_CONNECTIONS).connect(&database_url).await?;
    let access_key = std::env::var("LIVE_ACCESS_KEY").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    init_db(&db).await?;
    let outbox_path = std::env::var("CVD_OUTBOX_PATH").unwrap_or_else(|_| "data/outbox.sqlite".into());
    let outbox = Arc::new(persistence::Outbox::open(std::path::Path::new(&outbox_path)).await?);

    // build shards for all exchange × market × symbol
    let mut shards: HashMap<(Exchange, Market, Symbol), Shard> = HashMap::new();
    let exchanges = Exchange::ALL;

    for &ex in &exchanges {
        for (m, s) in get_market_symbol_pairs(ex) {
            shards.insert((ex, m, s), mk_shard());
        }
    }

    let state = AppState {
        shards: Arc::new(shards),
        db,
        outbox,
        http: market_http_client()?,
        ready: AtomicBool::new(false),
        access_key,
        indicators: indicators::IndicatorManager::start(&config),
        hubs: Arc::new(live_indicators::LiveHubs::new()),
        static_dir: config.server.static_dir.clone(),
    };
    let state = Arc::new(state);

    let startup_state = state.clone();
    tokio::spawn(async move {
        startup_data_pipeline(startup_state).await;
    });

    // axum app
    let app = app_router(state);

    let addr: SocketAddr = config.server.http_addr.parse()?;
    println!("Open UI: http://{addr}/");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;

    Ok(())
}

// ==============================
// HTTP handlers
// ==============================

fn app_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/meta", get(meta))
        .route("/indicators", get(indicators_endpoint))
        .route("/health", get(health))
        .route("/history", get(history))
        .route("/ws", get(ws_upgrade))
        .fallback_service(ServeDir::new(state.static_dir.clone()))
        .layer(axum::middleware::map_response(|mut response: axum::response::Response| async move {
            // Revalidate HTML/module metadata across venue-policy upgrades.
            response
                .headers_mut()
                .insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-cache"));
            response
        }))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut sources = Vec::new();
    for ((exchange, market, symbol), shard) in state.shards.iter() {
        let error = shard.stream_error.read().await.clone();
        let recovery = shard.recovery.read().await.clone();
        let last_trade_ms = shard.engine.try_lock().ok().and_then(|engine| engine.last_trade_ms);
        sources.push(serde_json::json!({
            "exchange": exchange.as_str(), "market": market.as_str(), "symbol": symbol.as_str(),
            "error": error, "last_trade_ms": last_trade_ms, "recovery": recovery,
        }));
    }
    let pending = state.outbox.pending_count().await;
    let retired = state.outbox.retired_count().await;
    let ready = state.ready.load(Ordering::Acquire);
    let healthy = ready
        && pending.as_ref().is_ok_and(|count| *count == 0)
        && sources.iter().all(|source| {
            source["error"].is_null()
                && source["recovery"]["state"] == "live"
                && source["last_trade_ms"].as_i64().is_some_and(|time| Utc::now().timestamp_millis() - time < 90_000)
        });
    // Indicator availability is reported but never affects ingestion health.
    Json(
        serde_json::json!({"healthy": healthy, "ready": ready, "pending_bars": pending.ok(), "retired_pending_bars": retired.ok(), "sources": sources,
        "indicators": state.indicators.infos().into_iter().map(|i| serde_json::json!({"id": i.id, "status": i.status, "error": i.error})).collect::<Vec<_>>()}),
    )
}

fn request_selection(p: &Params) -> std::result::Result<(Exchange, Market, Symbol, Tf), &'static str> {
    let exchange = Exchange::from_str(p.exchange.as_deref().unwrap_or("binance"))
        .ok_or("unsupported exchange; use binance or coinbase")?;
    if exchange == Exchange::Coinbase && p.market.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("usdm")) {
        return Err("retired Coinbase INTX usdm source; use independent usdc series");
    }
    let market = Market::from_str(p.market.as_deref().unwrap_or("spot"));
    let symbol = Symbol::from_str(p.symbol.as_deref().unwrap_or("BTC"));
    let tf = Tf::from_str(p.tf.as_deref().unwrap_or("1m"));
    if !get_market_symbol_pairs(exchange).contains(&(market, symbol))
        || p.market.as_deref().is_some_and(|v| !v.eq_ignore_ascii_case(market.as_str()))
        || p.symbol.as_deref().is_some_and(|v| !v.eq_ignore_ascii_case(symbol.as_str()))
        || p.tf.as_deref().is_some_and(|v| v != tf.as_str())
    {
        return Err("unsupported market, symbol or timeframe");
    }
    Ok((exchange, market, symbol, tf))
}

fn selection_response(
    p: &Params,
) -> std::result::Result<(Exchange, Market, Symbol, Tf), (axum::http::StatusCode, &'static str)> {
    use axum::http::StatusCode;
    let selection = request_selection(p).map_err(|error| {
        let status = if error.starts_with("retired Coinbase") { StatusCode::GONE } else { StatusCode::BAD_REQUEST };
        (status, error)
    })?;
    if selection.1.is_spot() && p.series.as_deref().is_some_and(|series| series != SPOT_SERIES) {
        return Err((StatusCode::CONFLICT, "spot series mismatch; require spot-v1 or omit series"));
    }
    if !selection.1.is_spot() && p.series.as_deref() != Some(CONTRACT_SERIES) {
        return Err((StatusCode::CONFLICT, "contract series mismatch; require contracts-drb-v1"));
    }
    Ok(selection)
}

async fn history(State(state): State<Arc<AppState>>, Query(p): Query<Params>) -> impl IntoResponse {
    let (exchange, market, symbol, tf_enum) = match selection_response(&p) {
        Ok(selection) => selection,
        Err(error) => return error.into_response(),
    };
    let (exchange, market, symbol, tf) = (exchange.as_str(), market.as_str(), symbol.as_str(), tf_enum.as_str());
    let limit_cap = if tf_enum == Tf::H1 { H1_HISTORY_CAPACITY } else { HISTORY_CAPACITY };
    let limit = p.limit.unwrap_or(800).min(limit_cap);
    let before = p.before;
    let after = p.after;
    let realtime_enabled = has_realtime_access(&state, p.key.as_deref());
    let max_time = if realtime_enabled { None } else { Some(Utc::now().timestamp() - DELAY_SECONDS) };
    match load_history_page(
        &state.db,
        exchange,
        symbol,
        market,
        tf,
        limit,
        before,
        after,
        max_time,
        realtime_enabled,
        &state.indicators,
        &requested_indicators(&state.indicators, p.indicators.as_deref()),
    )
    .await
    {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => {
            let status = if e.to_string().contains("before and after cannot both be set") {
                axum::http::StatusCode::BAD_REQUEST
            } else {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, format!("history error: {e}")).into_response()
        }
    }
}

async fn indicators_endpoint(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({ "api_version": indicators::API_VERSION, "indicators": state.indicators.infos() }))
}

async fn meta(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let exchanges = Exchange::ALL
        .into_iter()
        .map(|exchange| {
            let mut markets: Vec<String> = Vec::new();
            let mut symbols: Vec<String> = Vec::new();

            for (market, symbol) in get_market_symbol_pairs(exchange) {
                let market = market.as_str().to_string();
                if !markets.contains(&market) {
                    markets.push(market);
                }

                let symbol = symbol.as_str().to_string();
                if !symbols.contains(&symbol) {
                    symbols.push(symbol);
                }
            }

            ExchangeMeta { exchange: exchange.as_str().to_string(), markets, symbols }
        })
        .collect();

    Json(MetaPayload {
        exchanges,
        instruments: types::INSTRUMENTS,
        contract_series: CONTRACT_SERIES,
        spot_series: SPOT_SERIES,
        indicators: state.indicators.infos(),
        coinbase_contract_source:
            "Coinbase USDC perpetuals via Deribit-powered gateway; independent new series, not INTX history",
    })
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> impl IntoResponse {
    if !has_realtime_access(&state, p.key.as_deref()) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let (exchange, market, symbol, tf) = match selection_response(&p) {
        Ok(selection) => selection,
        Err(error) => return error.into_response(),
    };
    if !state.shards.contains_key(&(exchange, market, symbol)) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    ws.on_upgrade(move |socket| ws_handler(socket, state, exchange, market, symbol, tf)).into_response()
}

async fn ws_handler(
    mut socket: WebSocket,
    state: Arc<AppState>,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
) {
    let sh = pick_shard(&state, exchange, market, symbol).clone();

    let mut rx = match tf {
        Tf::M1 => sh.tx_1m.subscribe(),
        Tf::M5 => sh.tx_5m.subscribe(),
        Tf::M15 => sh.tx_15m.subscribe(),
        Tf::M30 => sh.tx_30m.subscribe(),
        Tf::H1 => sh.tx_1h.subscribe(),
    };
    // Shared per-scope indicator hub (None when no indicators are configured).
    let mut indicator_sub = state.hubs.subscribe(&state, (exchange, market, symbol, tf));

    loop {
        // Biased: a base frame enqueued before an indicator frame is always
        // forwarded first (the hub's bootstrap base frame precedes the
        // initial result computed from it).
        tokio::select! {
            biased;
            msg = rx.recv() => {
                match msg {
                    Ok(mut payload) => {
                        quality::enrich_delivery(&state, symbol, &mut payload.bar).await;
                        let Ok(txt) = serde_json::to_string(&payload) else { continue; };
                        if socket.send(Message::Text(txt.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Err(_) => break,
                }
            }
            ind = async {
                match indicator_sub.as_mut() {
                    Some(sub) => sub.rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let text: Arc<str> = match ind {
                    Ok(text) => text,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Viewer missed indicator deltas: ask it to refetch.
                        let generation = indicator_sub.as_ref().map(|s| s.hub.generation()).unwrap_or(0);
                        live_indicators::IndicatorFrame::reset(generation).to_text()
                    }
                    Err(_) => { indicator_sub = None; continue; }
                };
                if socket.send(Message::Text(text.as_ref().into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    None => break,
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

// ==============================
// Engine
// ==============================

async fn binance_market_engine_loop(state: Arc<AppState>, market: Market) {
    let exchange = Exchange::Binance;
    let mut attempt: u32 = 0;

    loop {
        let res = run_binance_market_once(&state, market).await;

        match res {
            Ok(_) => attempt = 0,
            Err(e) => {
                mark_market_error(&state, exchange, market, &e.to_string()).await;
                let s = e.to_string();
                if s.contains("ws ended") {
                    attempt = 0;
                } else {
                    eprintln!("[{}:{}] engine error: {e}", exchange.as_str(), market.as_str());
                    attempt = attempt.saturating_add(1);
                }
            }
        }

        let wait = backoff_with_jitter(attempt);
        eprintln!(
            "[{}:{}] reconnecting in {:.3}s (attempt={})",
            exchange.as_str(),
            market.as_str(),
            wait.as_secs_f64(),
            attempt
        );
        sleep(wait).await;
    }
}

async fn coinbase_market_engine_loop(state: Arc<AppState>, market: Market) {
    let exchange = Exchange::Coinbase;
    let mut attempt: u32 = 0;

    loop {
        let res = run_coinbase_market_once(&state, market).await;

        match res {
            Ok(_) => attempt = 0,
            Err(e) => {
                mark_market_error(&state, exchange, market, &e.to_string()).await;
                let s = e.to_string();
                if s.contains("ws ended") {
                    attempt = 0;
                } else {
                    eprintln!("[{}:{}] engine error: {e}", exchange.as_str(), market.as_str());
                    attempt = attempt.saturating_add(1);
                }
            }
        }

        let wait = backoff_with_jitter(attempt);
        eprintln!(
            "[{}:{}] reconnecting in {:.3}s (attempt={})",
            exchange.as_str(),
            market.as_str(),
            wait.as_secs_f64(),
            attempt
        );
        sleep(wait).await;
    }
}

async fn mark_market_error(state: &AppState, exchange: Exchange, market: Market, error: &str) {
    for ((source, source_market, _), shard) in state.shards.iter() {
        if *source == exchange && *source_market == market && !shard.recovery.read().await.needs_backfill() {
            *shard.stream_error.write().await = Some(error.to_owned());
        }
    }
}

#[cfg(test)]
#[test]
fn new_usdc_stream_routes_without_panicking() {
    assert!(binance_market_stream_url(Market::SpotUsdc).unwrap().contains("btcusdc@aggTrade"));
    assert!(binance_market_stream_url(Market::UsdmUsdc).unwrap().contains("ethusdc@aggTrade"));
}

fn binance_market_stream_url(market: Market) -> Result<String> {
    let instruments = binance::market_symbols(market)?;
    let streams = instruments
        .iter()
        .map(|i| format!("{}@aggTrade", i.native_symbol.to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join("/");
    let base = match market {
        Market::Spot | Market::SpotUsdc => "wss://stream.binance.com:9443/stream",
        Market::Usdm | Market::UsdmUsdc => "wss://fstream.binance.com/market/stream",
        Market::Coinm => "wss://dstream.binance.com/stream",
        Market::Usdc => anyhow::bail!("Coinbase USDC is not a Binance gateway"),
    };
    Ok(format!("{base}?streams={streams}"))
}

async fn run_binance_market_once(state: &AppState, market: Market) -> Result<()> {
    let url = binance_market_stream_url(market)?;
    binance::validate_market_metadata(market).await?;
    println!("[binance:{}] Connecting to: {}", market.as_str(), url);

    let (ws_stream, _resp) = connect_async(&url).await?;
    let (write, read) = ws_stream.split();

    binance::run_binance_market_engine(state, market, write, read).await
}

async fn run_coinbase_market_once(state: &Arc<AppState>, market: Market) -> Result<()> {
    let url = match market {
        Market::Spot => "wss://ws-feed.exchange.coinbase.com",
        Market::Usdc => "wss://streams.drb.coinbase.com/ws/api/v2",
        Market::Usdm => return Err(anyhow::anyhow!("retired Coinbase INTX feed must not run")),
        Market::Coinm => return Err(anyhow::anyhow!("Coinbase does not support coin-m market")),
        Market::SpotUsdc | Market::UsdmUsdc => {
            return Err(anyhow::anyhow!("Binance USDC market is not a Coinbase source"))
        }
    };
    println!("[coinbase:{}] Connecting to: {}", market.as_str(), url);

    let (ws_stream, _resp) = connect_async(url).await?;
    let (write, read) = ws_stream.split();

    match market {
        Market::Spot => coinbase::run_coinbase_spot_market_engine(state.clone(), write, read).await,
        Market::Usdc => coinbase_perp::run_coinbase_perp_market_engine(state.clone(), write, read).await,
        Market::Usdm => Err(anyhow::anyhow!("retired Coinbase INTX feed must not run")),
        Market::Coinm => Err(anyhow::anyhow!("Coinbase does not support coin-m market")),
        Market::SpotUsdc | Market::UsdmUsdc => Err(anyhow::anyhow!("Binance USDC market is not a Coinbase source")),
    }
}

fn make_live_bar(agg: &aggregator::BucketAgg, tf: Tf, ts_ms: i64, current_cvd: f64) -> Bar {
    let tf_ms = tf.ms();
    let bucket_id = ts_ms / tf_ms;
    let end_sec = ((bucket_id + 1) * tf_ms - 1) / 1000;
    Bar {
        time: end_sec,
        price_open: agg.open_price,
        price_high: agg.high_price,
        price_low: agg.low_price,
        price_close: agg.last_price,
        aggregate_price_open: agg.open_price,
        aggregate_price_high: agg.high_price,
        aggregate_price_low: agg.low_price,
        aggregate_price_close: agg.last_price,
        delta_usdt: agg.delta_usdt,
        aggregate_delta_usdt: 0.0,
        cvd_usdt: current_cvd,
        aggregate_cvd_usdt: 0.0,
        buy_volume_base: agg.buy_volume_base,
        aggregate_buy_volume_base: 0.0,
        sell_volume_base: agg.sell_volume_base,
        aggregate_sell_volume_base: 0.0,
        buy_volume_usdt: agg.buy_volume_usdt,
        aggregate_buy_volume_usdt: 0.0,
        sell_volume_usdt: agg.sell_volume_usdt,
        aggregate_sell_volume_usdt: 0.0,
        quality: None,
        trades: agg.trades,
    }
}

pub async fn flush_second(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    a1: &mut aggregator::BucketAgg,
    a5: &mut aggregator::BucketAgg,
    a15: &mut aggregator::BucketAgg,
    a30: &mut aggregator::BucketAgg,
    a1h: &mut aggregator::BucketAgg,
    cvd_closed_usdt: &mut f64,
    snap: &aggregator::SecSnapshot,
) {
    let ts_ms_flush = snap.sec * 1000 + 999;
    let mut closed = Vec::new();
    let mut live = Vec::with_capacity(5);
    let mut current_cvd = *cvd_closed_usdt;
    for (tf, bucket) in [(Tf::M1, a1), (Tf::M5, a5), (Tf::M15, a15), (Tf::M30, a30), (Tf::H1, a1h)] {
        if let Some(mut bar) = aggregator::push_bucket(
            bucket,
            tf,
            ts_ms_flush,
            snap.open_price,
            snap.high_price,
            snap.low_price,
            snap.last_price,
            snap.delta_usdt,
            snap.buy_volume_base,
            snap.sell_volume_base,
            snap.buy_volume_usdt,
            snap.sell_volume_usdt,
            snap.trades,
        ) {
            if tf == Tf::M1 {
                *cvd_closed_usdt += bar.delta_usdt;
            }
            bar.cvd_usdt = *cvd_closed_usdt;
            closed.push((tf, bar));
        }
        if tf == Tf::M1 {
            current_cvd = *cvd_closed_usdt + bucket.delta_usdt;
        }
        live.push((tf, make_live_bar(bucket, tf, ts_ms_flush, current_cvd)));
    }
    let pending: Vec<_> =
        closed.iter().map(|(tf, bar)| persistence::PendingBar::new(exchange, market, symbol, *tf, bar)).collect();
    while let Err(error) = state.outbox.enqueue_batch(&pending).await {
        eprintln!("outbox enqueue failed; ingestion paused without discarding bars: {error}");
        sleep(Duration::from_secs(1)).await;
    }
    for (tf, bar) in closed {
        push_publish(state, exchange, market, symbol, tf, bar).await;
    }
    for (tf, bar) in live {
        broadcast_live_payload(state, exchange, market, symbol, tf, bar).await;
    }
}

async fn push_publish(state: &AppState, exchange: Exchange, market: Market, symbol: Symbol, tf: Tf, bar: Bar) {
    let sh = pick_shard(state, exchange, market, symbol);
    let (hist, tx) = sh.for_tf(tf);

    let mut bar = bar;
    apply_live_aggregates(state, exchange, market, symbol, tf, &mut bar).await;
    let mut published = sh.published_for(tf).lock().await;
    let mut h = hist.write().await;
    if !bar.is_renderable() {
        sh.mark_invalid(tf, bar.time);
        return;
    }
    sh.clear_invalid(tf, bar.time);
    if h.len() >= HISTORY_CAPACITY {
        h.pop_front();
        if let Some(front) = h.front() {
            sh.prune_invalid_before(tf, front.time);
        }
    }
    h.push_back(bar.clone());
    drop(h);
    let action = {
        let mut live_bar = sh.live_bar_for_tf(tf).write().await;
        let action = if live_bar.as_ref().is_some_and(|live| live.time == bar.time) {
            WsAction::ReplaceLast
        } else {
            WsAction::Append
        };
        *live_bar = None;
        action
    };
    let rev = sh.rev.fetch_add(1, Ordering::AcqRel) + 1;
    *published = rev;
    // Base frame is enqueued before the hub can see the event, so a result
    // stamped with `rev` can never precede its base frame on a connection.
    let _ = tx.send(WsPayload { rev, series: series_for_market(market), action, bar: bar.clone() });
    state.hubs.notify((exchange, market, symbol, tf), &bar, rev);
}

async fn broadcast_live_payload(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    tf: Tf,
    bar: Bar,
) {
    let sh = pick_shard(state, exchange, market, symbol);
    let (_, tx) = sh.for_tf(tf);
    let mut bar = bar;
    apply_live_aggregates(state, exchange, market, symbol, tf, &mut bar).await;
    let mut published = sh.published_for(tf).lock().await;
    if !bar.is_renderable() {
        sh.mark_invalid(tf, bar.time);
        return;
    }
    sh.clear_invalid(tf, bar.time);
    let action = {
        let mut live_bar = sh.live_bar_for_tf(tf).write().await;
        let action = if live_bar.as_ref().is_some_and(|live| live.time == bar.time) {
            WsAction::ReplaceLast
        } else {
            WsAction::Append
        };
        *live_bar = Some(bar.clone());
        action
    };
    let rev = sh.rev.fetch_add(1, Ordering::AcqRel) + 1;
    *published = rev;
    // Base frame is enqueued before the hub can see the event, so a result
    // stamped with `rev` can never precede its base frame on a connection.
    let _ = tx.send(WsPayload { rev, series: series_for_market(market), action, bar: bar.clone() });
    state.hubs.notify((exchange, market, symbol, tf), &bar, rev);
}

pub fn parse_iso8601_to_ms(s: &str) -> Result<i64> {
    let dt: DateTime<Utc> = s.parse()?;
    Ok(dt.timestamp_millis())
}

fn backoff_with_jitter(attempt: u32) -> Duration {
    let exp = (1u64 << attempt.min(6)).min(60);
    let base = exp.max(1);
    let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
    Duration::from_secs(base) + Duration::from_millis(jitter_ms)
}

#[cfg(test)]
mod drb_migration_tests;

#[cfg(test)]
mod quality_tests;

#[cfg(test)]
mod pg_plugin_tests;
#[cfg(test)]
mod plugin_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_scan_aggregates_match_reference_for_live_closed_and_future_sources() {
        let state = state().await;
        let shard = pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
        let mut other = bar(119, 42.0);
        other.buy_volume_base = 2.0;
        other.buy_volume_usdt = 200.0;
        other.delta_usdt = 200.0;
        other.price_open = 98.0;
        other.price_high = 105.0;
        other.price_low = 95.0;
        shard.hist_1m.write().await.push_back(other.clone());
        for live_time in [59, 119, 179] {
            other.time = live_time;
            other.cvd_usdt = 55.0;
            *shard.live_bar_for_tf(Tf::M1).write().await = Some(other.clone());
            let mut current = bar(119, 10.0);
            current.buy_volume_base = 1.0;
            current.delta_usdt = 100.0;
            let expected_cvd =
                aggregate_live_cvd(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &current).await;
            let expected_flow =
                aggregate_live_flow(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &current).await;
            let expected_ohlc =
                aggregate_live_ohlc(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &current)
                    .await
                    .unwrap();
            apply_live_aggregates(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mut current).await;
            assert_eq!(current.aggregate_cvd_usdt, expected_cvd);
            assert_eq!(
                (
                    current.aggregate_delta_usdt,
                    current.aggregate_buy_volume_base,
                    current.aggregate_sell_volume_base,
                    current.aggregate_buy_volume_usdt,
                    current.aggregate_sell_volume_usdt
                ),
                expected_flow
            );
            assert_eq!(
                (
                    current.aggregate_price_open,
                    current.aggregate_price_high,
                    current.aggregate_price_low,
                    current.aggregate_price_close
                ),
                expected_ohlc
            );
        }
    }

    #[test]
    fn pagination_warmup_does_not_hide_older_rows() {
        let mut bars: Vec<_> = (1..=2000).map(|time| bar(time, 1.0)).collect();
        assert!(trim_history_page(&mut bars, 1600, None, false));
        assert_eq!(bars.len(), 1600);
        assert_eq!(bars[0].time, 401);
        let mut older: Vec<_> = (1..=400).map(|time| bar(time, 1.0)).collect();
        assert!(!trim_history_page(&mut older, 1600, None, true));
    }

    #[test]
    fn pagination_filters_context_by_time_after_invalid_rows_removed() {
        let mut bars = vec![bar(9, 1.0), bar(11, 2.0), bar(12, 3.0)];
        trim_history_page(&mut bars, 2, Some(10), true);
        assert_eq!(bars.iter().map(|bar| bar.time).collect::<Vec<_>>(), [11, 12]);
    }

    pub(crate) fn bar(time: i64, cvd: f64) -> Bar {
        Bar::from(models::BarRow {
            time,
            price_open: 100.0,
            price_high: 100.0,
            price_low: 100.0,
            price_close: 100.0,
            delta_usdt: 0.0,
            cvd_usdt: cvd,
            buy_volume_base: 0.0,
            sell_volume_base: 0.0,
            buy_volume_usdt: 0.0,
            sell_volume_usdt: 0.0,
            trades: 1,
        })
    }

    pub(crate) async fn state() -> AppState {
        state_with(indicators::IndicatorManager::empty()).await
    }

    pub(crate) async fn state_with(manager: Arc<indicators::IndicatorManager>) -> AppState {
        let shards = Exchange::ALL
            .into_iter()
            .flat_map(|exchange| {
                get_market_symbol_pairs(exchange)
                    .into_iter()
                    .map(move |(market, symbol)| ((exchange, market, symbol), mk_shard()))
            })
            .collect();
        AppState {
            shards: Arc::new(shards),
            outbox: Arc::new(persistence::Outbox::memory().await),
            http: market_http_client().unwrap(),
            ready: AtomicBool::new(false),
            db: PgPoolOptions::new().connect_lazy("postgres://localhost/cvd_test").unwrap(),
            access_key: None,
            indicators: manager,
            hubs: Arc::new(live_indicators::LiveHubs::new()),
            static_dir: std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static"),
        }
    }

    #[test]
    fn live_cvd_does_not_add_higher_timeframe_delta_twice() {
        let mut minute = aggregator::BucketAgg::new();
        minute.delta_usdt = 3.0;
        let mut higher = aggregator::BucketAgg::new();
        higher.delta_usdt = 13.0;
        let current_cvd = 110.0 + minute.delta_usdt;
        for tf in [Tf::M5, Tf::M15, Tf::M30, Tf::H1] {
            let live = make_live_bar(&higher, tf, 61_999, current_cvd);
            assert_eq!(live.cvd_usdt, 113.0);
            assert_eq!(live.delta_usdt, 13.0);
        }
        assert_eq!(make_live_bar(&minute, Tf::M1, 61_999, current_cvd).cvd_usdt, 113.0);
    }

    #[tokio::test]
    async fn aggregate_cvd_includes_other_live_sources_but_not_future_bars() {
        let state = state().await;
        let other = pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
        other.hist_1m.write().await.push_back(bar(59, 100.0));
        *other.live_bar_1m.write().await = Some(bar(119, 113.0));
        let current = bar(119, 20.0);
        assert_eq!(
            aggregate_live_cvd(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &current).await,
            133.0
        );
        *other.live_bar_1m.write().await = Some(bar(179, 150.0));
        assert_eq!(
            aggregate_live_cvd(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &current).await,
            120.0
        );
    }
}
