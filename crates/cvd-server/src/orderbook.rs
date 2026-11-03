//! Ephemeral in-memory live order books (Binance USDⓈ-M USDT perpetual BTC/ETH).
//!
//! One sequential Tokio task per subscribed symbol owns the book; nothing is
//! written to Redis, PostgreSQL or the outbox. Browsers receive coalesced,
//! validated, bounded snapshots through a `watch` channel (slow clients never
//! queue or stall ingestion). Lifecycle is demand-driven: the last
//! subscriber's guard drop stops the task and frees the book.
//!
//! Synchronization follows Binance's USDⓈ-M "local order book" rules
//! (verified against live data, see audit protocol notes): buffer diff events
//! while fetching a REST snapshot (limit 1000), drop events with
//! `u < lastUpdateId`, the first applied event must bridge
//! `U <= lastUpdateId <= u`, then every event's `pu` must equal the previous
//! applied event's `u`. Quantities are absolute; zero deletes. Any violation
//! resyncs with a new generation. Coverage is the snapshot's price range;
//! levels outside it are never reported (unknown, not zero).

use anyhow::{anyhow, bail, Result};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

use crate::types::{Market, Symbol};

/// Fixed-point price unit (1e-8) so book keys are exact decimal identities.
const PRICE_SCALE: i64 = 100_000_000;
/// Hard per-side bound inside coverage; exceeding it resyncs (never trims).
pub const MAX_LEVELS_PER_SIDE: usize = 20_000;
/// Maximum buffered diff events while the REST snapshot is in flight.
pub const MAX_BUFFERED_EVENTS: usize = 2_000;
/// Levels per side published to browsers.
pub const PUBLISH_LEVELS: usize = 500;
/// REST snapshot depth requested.
pub const SNAPSHOT_LIMIT: usize = 1000;
/// No accepted event for this long while live => stale (levels withdrawn).
pub const STALE_AFTER: Duration = Duration::from_secs(5);
/// Minimum interval between published live frames (coalescing).
pub const PUBLISH_INTERVAL: Duration = Duration::from_millis(200);
/// Below this many covered levels on either side the book resyncs.
pub const MIN_COVERED_LEVELS: usize = 50;
/// Fixed deadline for connection establishment.
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Fixed deadline from stream connected until the book is live (snapshot +
/// bridge). Incoming diffs never extend it.
pub const SYNC_DEADLINE: Duration = Duration::from_secs(10);
/// Largest accepted single WS message / text event.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// Total bytes of diff events buffered while the snapshot is in flight.
pub const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;
/// Largest accepted REST snapshot body.
pub const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
/// Minimum spacing between snapshot requests per symbol (survives demand churn).
pub const SNAPSHOT_MIN_GAP: Duration = Duration::from_secs(2);
/// A generation must stay live this long before retry history resets.
pub const HEALTHY_RESET: Duration = Duration::from_secs(60);

pub fn parse_price(s: &str) -> Option<i64> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_trim = frac.trim_end_matches('0');
    if frac_trim.len() > 8 {
        return None;
    }
    let i: i64 = int.parse().ok()?;
    let mut f: i64 = if frac_trim.is_empty() { 0 } else { frac_trim.parse().ok()? };
    for _ in frac_trim.len()..8 {
        f *= 10;
    }
    let v = i.checked_mul(PRICE_SCALE)?.checked_add(f)?;
    (v > 0).then_some(v)
}

fn parse_qty(s: &str) -> Option<f64> {
    let q: f64 = s.parse().ok()?;
    (q.is_finite() && q >= 0.0).then_some(q)
}

fn price_f64(p: i64) -> f64 {
    p as f64 / PRICE_SCALE as f64
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiffEvent {
    #[serde(rename = "e")]
    pub event: String,
    #[serde(rename = "E")]
    pub event_time: i64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "U")]
    pub first: u64,
    #[serde(rename = "u")]
    pub last: u64,
    #[serde(rename = "pu")]
    pub prev: u64,
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestSnapshot {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    SourceMismatch,
    BufferOverflow,
    /// First event after the snapshot does not bridge `U <= L <= u`.
    BridgeGap,
    /// `pu` != previous applied `u`.
    SequenceGap,
    BadLevel,
    Crossed,
    LevelOverflow,
    CoverageExhausted,
    BadSnapshot,
}

#[derive(Debug)]
enum Phase {
    /// Buffered events with their monotonic receipt instants and total bytes.
    Buffering(Vec<(DiffEvent, Instant)>, usize),
    /// Snapshot loaded; waiting for the bridging event.
    AwaitBridge {
        snapshot_id: u64,
    },
    Live {
        last_u: u64,
    },
}

/// Pure, sequential reducer for one book generation.
#[derive(Debug)]
pub struct BookSync {
    native_symbol: String,
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
    bid_floor: i64,
    ask_ceiling: i64,
    phase: Phase,
    pub last_event_ms: i64,
    /// Monotonic receipt instant of the last APPLIED event (survives buffering).
    pub last_applied_rx: Option<Instant>,
}

fn apply_side(side: &mut BTreeMap<i64, f64>, levels: &[[String; 2]], lo: i64, hi: i64) -> Result<(), SyncError> {
    for [p, q] in levels {
        let p = parse_price(p).ok_or(SyncError::BadLevel)?;
        let q = parse_qty(q).ok_or(SyncError::BadLevel)?;
        if p < lo || p > hi {
            continue; // outside coverage: unknown territory, never reported
        }
        if q == 0.0 {
            side.remove(&p); // removing an absent level is normal
        } else {
            side.insert(p, q);
        }
    }
    Ok(())
}

impl BookSync {
    pub fn new(native_symbol: &str) -> Self {
        Self {
            native_symbol: native_symbol.to_string(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            bid_floor: 0,
            ask_ceiling: 0,
            phase: Phase::Buffering(Vec::new(), 0),
            last_event_ms: 0,
            last_applied_rx: None,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self.phase, Phase::Live { .. })
    }

    pub fn last_update_id(&self) -> Option<u64> {
        match self.phase {
            Phase::Live { last_u } => Some(last_u),
            _ => None,
        }
    }

    /// Test convenience: receipt = now, size not budgeted.
    #[cfg(test)]
    pub fn on_event(&mut self, ev: DiffEvent) -> Result<bool, SyncError> {
        self.on_event_at(ev, Instant::now(), 0)
    }

    /// `rx` is the monotonic receipt instant, `bytes` the raw input size.
    pub fn on_event_at(&mut self, ev: DiffEvent, rx: Instant, bytes: usize) -> Result<bool, SyncError> {
        if ev.event != "depthUpdate" || !ev.symbol.eq_ignore_ascii_case(&self.native_symbol) {
            return Err(SyncError::SourceMismatch);
        }
        // Malformed range (U > u) is never valid in any phase.
        if ev.first > ev.last {
            return Err(SyncError::SequenceGap);
        }
        match &mut self.phase {
            Phase::Buffering(buf, total) => {
                if buf.len() >= MAX_BUFFERED_EVENTS || *total + bytes > MAX_BUFFERED_BYTES {
                    return Err(SyncError::BufferOverflow);
                }
                *total += bytes;
                buf.push((ev, rx));
                Ok(false)
            }
            Phase::AwaitBridge { snapshot_id } => {
                let l = *snapshot_id;
                if ev.last < l {
                    return Ok(false); // predates snapshot
                }
                if ev.first > l {
                    return Err(SyncError::BridgeGap);
                }
                self.apply(&ev, rx)?;
                Ok(true)
            }
            Phase::Live { last_u } => {
                // Beyond the bridge, `pu` must chain AND `u` must strictly
                // advance (futures ids may skip, so no `U == last_u + 1`).
                if ev.prev != *last_u || ev.last <= *last_u {
                    return Err(SyncError::SequenceGap);
                }
                self.apply(&ev, rx)?;
                Ok(true)
            }
        }
    }

    /// Transactional: the whole event is applied to copies and checked as a
    /// completed update before it replaces the book.
    fn apply(&mut self, ev: &DiffEvent, rx: Instant) -> Result<(), SyncError> {
        if ev.bids.len() > MAX_LEVELS_PER_SIDE || ev.asks.len() > MAX_LEVELS_PER_SIDE {
            return Err(SyncError::BadLevel);
        }
        let mut bids = self.bids.clone();
        let mut asks = self.asks.clone();
        apply_side(&mut bids, &ev.bids, self.bid_floor, i64::MAX)?;
        apply_side(&mut asks, &ev.asks, 0, self.ask_ceiling)?;
        Self::check(&bids, &asks)?;
        self.bids = bids;
        self.asks = asks;
        self.phase = Phase::Live { last_u: ev.last };
        self.last_event_ms = ev.event_time;
        self.last_applied_rx = Some(rx);
        Ok(())
    }

    fn check(bids: &BTreeMap<i64, f64>, asks: &BTreeMap<i64, f64>) -> Result<(), SyncError> {
        if bids.len() > MAX_LEVELS_PER_SIDE || asks.len() > MAX_LEVELS_PER_SIDE {
            return Err(SyncError::LevelOverflow);
        }
        if bids.is_empty() || asks.is_empty() {
            return Err(SyncError::CoverageExhausted);
        }
        if let (Some((b, _)), Some((a, _))) = (bids.last_key_value(), asks.first_key_value()) {
            if b >= a {
                return Err(SyncError::Crossed);
            }
        }
        Ok(())
    }

    /// Load the REST snapshot and replay buffered events.
    pub fn on_snapshot(&mut self, snap: RestSnapshot) -> Result<(), SyncError> {
        if snap.bids.len() > SNAPSHOT_LIMIT || snap.asks.len() > SNAPSHOT_LIMIT {
            return Err(SyncError::BadSnapshot);
        }
        let Phase::Buffering(buf, _) = std::mem::replace(&mut self.phase, Phase::AwaitBridge { snapshot_id: 0 }) else {
            return Err(SyncError::BadSnapshot);
        };
        let mut bids = BTreeMap::new();
        let mut asks = BTreeMap::new();
        apply_side(&mut bids, &snap.bids, 0, i64::MAX).map_err(|_| SyncError::BadSnapshot)?;
        apply_side(&mut asks, &snap.asks, 0, i64::MAX).map_err(|_| SyncError::BadSnapshot)?;
        if bids.is_empty() || asks.is_empty() {
            return Err(SyncError::BadSnapshot);
        }
        Self::check(&bids, &asks).map_err(|_| SyncError::BadSnapshot)?;
        self.bid_floor = *bids.first_key_value().unwrap().0;
        self.ask_ceiling = *asks.last_key_value().unwrap().0;
        self.bids = bids;
        self.asks = asks;
        self.phase = Phase::AwaitBridge { snapshot_id: snap.last_update_id };
        // Replay keeps each event's original receipt instant (age is preserved).
        for (ev, rx) in buf {
            self.on_event_at(ev, rx, 0)?;
        }
        Ok(())
    }

    fn covered_ok(&self) -> bool {
        self.bids.len() >= MIN_COVERED_LEVELS && self.asks.len() >= MIN_COVERED_LEVELS
    }

    /// Best-first top `n` levels per side (prices as f64 for display only).
    pub fn top(&self, n: usize) -> (Vec<[f64; 2]>, Vec<[f64; 2]>) {
        let bids = self.bids.iter().rev().take(n).map(|(p, q)| [price_f64(*p), *q]).collect();
        let asks = self.asks.iter().take(n).map(|(p, q)| [price_f64(*p), *q]).collect();
        (bids, asks)
    }

    /// Maintained (snapshot) coverage: complete range kept in memory.
    pub fn coverage(&self) -> (f64, f64) {
        (price_f64(self.bid_floor), price_f64(self.ask_ceiling))
    }

    /// Coverage of the transmitted top-`n` subset: complete only down to the
    /// last transmitted bid / up to the last transmitted ask when a side is
    /// truncated, otherwise the maintained bound.
    pub fn published_coverage(&self, n: usize) -> (f64, f64) {
        let lo = if self.bids.len() > n { *self.bids.keys().rev().nth(n - 1).unwrap() } else { self.bid_floor };
        let hi = if self.asks.len() > n { *self.asks.keys().nth(n - 1).unwrap() } else { self.ask_ceiling };
        (price_f64(lo), price_f64(hi))
    }
}

// ------------------------------------------------------------------ frames

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookStatus {
    Syncing,
    Live,
    Stale,
}

impl BookStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syncing => "syncing",
            Self::Live => "live",
            Self::Stale => "stale",
        }
    }
}

/// One published snapshot, pre-serialized once for all subscribers.
#[derive(Debug)]
pub struct BookFrame {
    pub status: BookStatus,
    pub generation: u64,
    pub json: String,
}

fn instrument(symbol: Symbol) -> Option<&'static crate::types::Instrument> {
    crate::types::INSTRUMENTS
        .iter()
        .find(|i| i.exchange == crate::types::Exchange::Binance && i.market == Market::Usdm && i.symbol == symbol)
}

/// Supported order-book sources (single source, no fallback).
pub fn supported(exchange: &str, market: &str, symbol: &str) -> Option<Symbol> {
    if exchange != "binance" || market != "usdm" {
        return None;
    }
    let s = match symbol {
        "BTC" => Symbol::Btc,
        "ETH" => Symbol::Eth,
        _ => return None,
    };
    instrument(s).map(|_| s)
}

fn frame(
    symbol: Symbol,
    status: BookStatus,
    generation: u64,
    book: Option<&BookSync>,
    reason: Option<&str>,
) -> BookFrame {
    let inst = instrument(symbol).expect("supported symbol");
    let (bids, asks, seq, t, cov, maintained) = match (status, book) {
        (BookStatus::Live, Some(b)) => {
            let (bids, asks) = b.top(PUBLISH_LEVELS);
            let (lo, hi) = b.published_coverage(PUBLISH_LEVELS);
            let (mlo, mhi) = b.coverage();
            (
                bids,
                asks,
                b.last_update_id().unwrap_or(0),
                b.last_event_ms,
                // `coverage` = complete range of the TRANSMITTED levels.
                Some(serde_json::json!({"bid_floor": lo, "ask_ceiling": hi})),
                // Server-side maintained range (informational; not transmitted).
                Some(serde_json::json!({"bid_floor": mlo, "ask_ceiling": mhi})),
            )
        }
        // Non-live frames withdraw all levels: nothing old is ever shown as live.
        _ => (Vec::new(), Vec::new(), 0, 0, None, None),
    };
    let json = serde_json::json!({
        "type": "book",
        "status": status.as_str(),
        "reason": reason,
        "source": {"exchange": "binance", "market": "usdm", "symbol": symbol.as_str()},
        "base": symbol.as_str(),
        "quote": inst.quote_asset,
        "generation": generation,
        "seq": seq,
        "exchange_time_ms": t,
        "depth_limit": SNAPSHOT_LIMIT,
        "bids": bids,
        "asks": asks,
        "coverage": cov,
        "maintained_coverage": maintained,
    })
    .to_string();
    BookFrame { status, generation, json }
}

// ------------------------------------------------------------------ feed

pub type TextStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// Exchange access (production: Binance; tests: scripted replay).
pub trait BookFeed: Send + Sync + 'static {
    fn connect(
        &self,
        native_symbol: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>>;
    fn snapshot(&self, native_symbol: &str) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>;
}

pub struct BinanceFeed {
    pub http: reqwest::Client,
}

impl BookFeed for BinanceFeed {
    fn connect(
        &self,
        native_symbol: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>> {
        let url = format!("wss://fstream.binance.com/public/ws/{}@depth@100ms", native_symbol.to_ascii_lowercase());
        Box::pin(async move {
            let mut cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
            cfg.max_message_size = Some(MAX_EVENT_BYTES);
            cfg.max_frame_size = Some(MAX_EVENT_BYTES);
            let (ws, _) = tokio_tungstenite::connect_async_with_config(&url, Some(cfg), false).await?;
            let s = ws.filter_map(|m| async move {
                match m {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => Some(Ok(t.to_string())),
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => Some(Err(anyhow!("closed"))),
                    Ok(_) => None,
                    Err(e) => Some(Err(anyhow!(e))),
                }
            });
            Ok(Box::pin(s) as TextStream)
        })
    }
    fn snapshot(&self, native_symbol: &str) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        let url = format!(
            "https://fapi.binance.com/fapi/v1/depth?symbol={}&limit={SNAPSHOT_LIMIT}",
            native_symbol.to_ascii_uppercase()
        );
        Box::pin(async move {
            read_snapshot_body(self.http.get(url).send().await?).await
        })
    }
}

/// Production REST body reader: status check, then a chunked read with a hard
/// cap. Content-Length (absent, chunked or wrong) is never trusted.
pub(crate) async fn read_snapshot_body(mut r: reqwest::Response) -> Result<String> {
    if !r.status().is_success() {
        bail!("snapshot http {}", r.status().as_u16());
    }
    let mut body = Vec::new();
    while let Some(c) = r.chunk().await? {
        if body.len() + c.len() > MAX_SNAPSHOT_BYTES {
            bail!("oversized_input");
        }
        body.extend_from_slice(&c);
    }
    Ok(String::from_utf8(body)?)
}

// ------------------------------------------------------------------ hub

struct Entry {
    tx: watch::Sender<Arc<BookFrame>>,
    subs: usize,
    task: tokio::task::JoinHandle<()>,
}

pub struct BookHub {
    feed: Arc<dyn BookFeed>,
    entries: Mutex<HashMap<Symbol, Entry>>,
    /// Per-symbol last snapshot start; outlives tasks so demand churn cannot
    /// bypass the snapshot budget.
    budget: Arc<Mutex<HashMap<Symbol, Instant>>>,
}

/// Subscriber demand; dropping the last one stops the symbol's task.
pub struct BookSubscription {
    hub: Arc<BookHub>,
    symbol: Symbol,
    pub rx: watch::Receiver<Arc<BookFrame>>,
}

impl Drop for BookSubscription {
    fn drop(&mut self) {
        let mut m = self.hub.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = m.get_mut(&self.symbol) {
            e.subs -= 1;
            if e.subs == 0 {
                e.task.abort();
                m.remove(&self.symbol);
            }
        }
    }
}

impl BookHub {
    pub fn new(feed: Arc<dyn BookFeed>) -> Arc<Self> {
        Arc::new(Self { feed, entries: Mutex::new(HashMap::new()), budget: Arc::new(Mutex::new(HashMap::new())) })
    }

    pub fn active(&self) -> Vec<(Symbol, usize)> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(s, e)| (*s, e.subs)).collect()
    }

    pub fn subscribe(self: &Arc<Self>, symbol: Symbol) -> BookSubscription {
        let mut m = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let e = m.entry(symbol).or_insert_with(|| {
            let (tx, _) = watch::channel(Arc::new(frame(symbol, BookStatus::Syncing, 0, None, None)));
            let task = tokio::spawn(run_symbol(self.feed.clone(), symbol, tx.clone(), self.budget.clone()));
            Entry { tx, subs: 0, task }
        });
        e.subs += 1;
        let rx = e.tx.subscribe();
        BookSubscription { hub: self.clone(), symbol, rx }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((1000u64 << attempt.min(5)).min(30_000))
}

/// Waits for this symbol's snapshot budget, then reserves the slot; called
/// immediately before the REST snapshot request is issued.
async fn reserve_snapshot(budget: &Mutex<HashMap<Symbol, Instant>>, symbol: Symbol) {
    loop {
        let wait = {
            let mut m = budget.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            match m.get(&symbol) {
                Some(t) if now < *t + SNAPSHOT_MIN_GAP => Some(*t + SNAPSHOT_MIN_GAP),
                _ => {
                    m.insert(symbol, now);
                    None
                }
            }
        };
        match wait {
            Some(at) => tokio::time::sleep_until(at).await,
            None => return,
        }
    }
}

async fn run_symbol(
    feed: Arc<dyn BookFeed>,
    symbol: Symbol,
    tx: watch::Sender<Arc<BookFrame>>,
    budget: Arc<Mutex<HashMap<Symbol, Instant>>>,
) {
    let native = instrument(symbol).expect("supported").native_symbol;
    let mut generation = 0u64;
    let mut attempt = 0u32;
    loop {
        generation += 1;
        tx.send_replace(Arc::new(frame(symbol, BookStatus::Syncing, generation, None, None)));
        let (reason, live_for) = run_generation(&*feed, symbol, native, generation, &tx, &budget).await;
        tx.send_replace(Arc::new(frame(symbol, BookStatus::Stale, generation, None, Some(&reason))));
        // Retry history resets only after a sustained healthy generation.
        if live_for >= HEALTHY_RESET {
            attempt = 0;
        }
        tokio::time::sleep(backoff(attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

fn read_event(m: Option<Result<String>>, book: &mut BookSync) -> Result<bool, String> {
    let rx = Instant::now();
    let t = match m {
        Some(Ok(t)) => t,
        _ => return Err("disconnected".into()),
    };
    if t.len() > MAX_EVENT_BYTES {
        return Err("oversized_input".into());
    }
    let ev = serde_json::from_str::<DiffEvent>(&t).map_err(|_| "bad_event".to_string())?;
    book.on_event_at(ev, rx, t.len()).map_err(|e| format!("{e:?}").to_ascii_lowercase())
}

/// Runs one synchronized generation; returns the fixed-vocabulary reason it
/// ended and how long it was continuously live.
async fn run_generation(
    feed: &dyn BookFeed,
    symbol: Symbol,
    native: &str,
    generation: u64,
    tx: &watch::Sender<Arc<BookFrame>>,
    budget: &Mutex<HashMap<Symbol, Instant>>,
) -> (String, Duration) {
    let none = Duration::ZERO;
    let mut stream = match tokio::time::timeout(CONNECT_DEADLINE, feed.connect(native)).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => return ("connect_failed".into(), none),
        Err(_) => return ("connect_timeout".into(), none),
    };
    let mut book = BookSync::new(native);
    // Absolute deadline: snapshot + bridge must complete by then; incoming
    // diffs never extend it.
    let sync_deadline = tokio::time::sleep_until(Instant::now() + SYNC_DEADLINE);
    tokio::pin!(sync_deadline);
    // The per-symbol budget is reserved at the ACTUAL snapshot initiation
    // (after connect); diffs keep buffering under the bounded budgets and the
    // absolute sync deadline above.
    let snap = async {
        reserve_snapshot(budget, symbol).await;
        feed.snapshot(native).await
    };
    tokio::pin!(snap);
    let snapshot = loop {
        tokio::select! {
            s = &mut snap => break s,
            m = stream.next() => if let Err(r) = read_event(m, &mut book) { return (r, none) },
            _ = &mut sync_deadline => return ("snapshot_timeout".into(), none),
        }
    };
    let text = match snapshot {
        Ok(t) if t.len() > MAX_SNAPSHOT_BYTES => return ("oversized_input".into(), none),
        Ok(t) => t,
        Err(e) if e.to_string() == "oversized_input" => return ("oversized_input".into(), none),
        Err(_) => return ("snapshot_failed".into(), none),
    };
    let Ok(snapshot) = serde_json::from_str::<RestSnapshot>(&text) else {
        return ("snapshot_failed".into(), none);
    };
    if let Err(e) = book.on_snapshot(snapshot) {
        return (format!("{e:?}").to_ascii_lowercase(), none);
    }
    let mut live_since: Option<Instant> = None;
    let mut last_publish: Option<Instant> = None;
    let mut dirty = book.is_live();
    let mut tick = tokio::time::interval(PUBLISH_INTERVAL / 2);
    let live_for = |s: Option<Instant>| s.map_or(Duration::ZERO, |t| t.elapsed());
    loop {
        // Freshness is the receipt age of the last APPLIED event (buffered
        // events keep their original receipt instant).
        let fresh = book.last_applied_rx.is_some_and(|t| t.elapsed() < STALE_AFTER);
        if book.is_live() && !fresh {
            return ("stale_feed".into(), live_for(live_since));
        }
        if dirty && book.is_live() && last_publish.is_none_or(|t| t.elapsed() >= PUBLISH_INTERVAL) {
            if !book.covered_ok() {
                return ("coverage_exhausted".into(), live_for(live_since));
            }
            tx.send_replace(Arc::new(frame(symbol, BookStatus::Live, generation, Some(&book), None)));
            last_publish = Some(Instant::now());
            live_since.get_or_insert_with(Instant::now);
            dirty = false;
        }
        tokio::select! {
            m = stream.next() => match read_event(m, &mut book) {
                Ok(applied) => dirty |= applied,
                Err(r) => return (r, live_for(live_since)),
            },
            _ = &mut sync_deadline, if !book.is_live() => return ("snapshot_timeout".into(), none),
            _ = tick.tick() => {}
        }
    }
}

#[cfg(test)]
#[path = "orderbook_tests.rs"]
mod tests;
