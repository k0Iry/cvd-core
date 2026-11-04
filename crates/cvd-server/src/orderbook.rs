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

use crate::types::{Exchange, Market, Symbol};

/// Fixed-point price unit (1e-8) so book keys are exact decimal identities.
const PRICE_SCALE: i64 = 100_000_000;
/// Hard per-side bound inside coverage; exceeding it resyncs (never trims).
pub const MAX_LEVELS_PER_SIDE: usize = 20_000;
/// Maximum buffered diff events while the REST snapshot is in flight.
pub const MAX_BUFFERED_EVENTS: usize = 2_000;
/// Futures levels per side published to browsers (full 1000 snapshot depth).
pub const PUBLISH_LEVELS: usize = SNAPSHOT_LIMIT;
/// REST snapshot depth requested.
pub const SNAPSHOT_LIMIT: usize = 1000;
/// Binance spot REST snapshot maximum per side.
pub const SPOT_SNAPSHOT_LIMIT: usize = 5000;

/// Binance sequencing family. Spot and futures bridge/chain rules differ and
/// are never mixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Venue {
    Futures,
    Spot,
    /// Coinbase Exchange `level2_batch`: full WS snapshot then absolute
    /// `l2update` sizes; no upstream sequence ids exist.
    Coinbase,
    /// Coinbase DRB (linear USDC perpetuals): JSON-RPC `book.<native>.100ms`,
    /// full WS snapshot then `prev_change_id`-chained new/change/delete.
    Drb,
}

impl Venue {
    /// Honest REST snapshot depth: futures 1000/side, spot 5000/side.
    pub const fn snapshot_limit(self) -> usize {
        match self {
            Self::Futures => SNAPSHOT_LIMIT,
            Self::Spot => SPOT_SNAPSHOT_LIMIT,
            Self::Coinbase | Self::Drb => COINBASE_WINDOW_LEVELS,
        }
    }
    /// Levels per side transmitted to browsers: futures 500, spot the full
    /// 5000-level snapshot depth (M1 deep spot slice).
    pub const fn publish_levels(self) -> usize {
        match self {
            Self::Futures => SNAPSHOT_LIMIT,
            Self::Spot => SPOT_SNAPSHOT_LIMIT,
            Self::Coinbase | Self::Drb => COINBASE_WINDOW_LEVELS,
        }
    }
}

/// Coinbase: nearest levels per side kept as the anchored maintained window,
/// selected from the complete validated WS snapshot.
pub const COINBASE_WINDOW_LEVELS: usize = 5000;
/// Coinbase: largest accepted WS message (recorded BTC-USD snapshot ~1.07MB).
pub const COINBASE_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// Coinbase: hard per-side level count accepted in one snapshot message.
pub const COINBASE_MAX_SNAPSHOT_LEVELS: usize = 100_000;
/// Coinbase: hard change count accepted in one l2update.
pub const COINBASE_MAX_CHANGES: usize = 20_000;

/// Order-book source key: exact registered Binance instrument
/// (market + base symbol). Every registered market is a distinct producer even
/// when native symbols coincide (spot BTCUSDT vs USDⓈ-M BTCUSDT). The
/// synchronization family (`Venue`) is derived, never part of identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BookKey {
    pub exchange: Exchange,
    pub market: Market,
    pub symbol: Symbol,
}

/// Binance markets with a live depth book (Coinbase books are deferred and
/// unsupported).
pub const BOOK_MARKETS: [Market; 5] = [Market::Spot, Market::SpotUsdc, Market::Usdm, Market::UsdmUsdc, Market::Coinm];

impl BookKey {
    /// Binance key convenience.
    pub const fn binance(market: Market, symbol: Symbol) -> Self {
        Self { exchange: Exchange::Binance, market, symbol }
    }
    pub const fn venue(self) -> Venue {
        match (self.exchange, self.market) {
            (Exchange::Coinbase, Market::Usdc) => Venue::Drb,
            (Exchange::Coinbase, _) => Venue::Coinbase,
            (_, Market::Spot | Market::SpotUsdc) => Venue::Spot,
            _ => Venue::Futures,
        }
    }
    /// Driver/state label: `exchange/market/SYMBOL`.
    pub fn label(self) -> String {
        format!("{}/{}/{}", self.exchange.as_str(), self.market.as_str(), self.symbol.as_str())
    }
    /// COIN-M contract face value in quote USD (Binance dapi exchangeInfo
    /// `contractSize`: BTCUSD_PERP 100, ETHUSD_PERP 10; cross-checked against
    /// recorded exchangeInfo evidence in tests). `None` = raw qty is base.
    pub const fn contract_size(self) -> Option<f64> {
        if !matches!(self.exchange, Exchange::Binance) {
            return None;
        }
        match (self.market, self.symbol) {
            (Market::Coinm, Symbol::Btc) => Some(100.0),
            (Market::Coinm, Symbol::Eth) => Some(10.0),
            _ => None,
        }
    }
}

/// Raw level quantity -> base asset. Inverse contracts convert at THAT
/// level's price: base = contracts * contractSize / price.
pub fn base_qty(key: BookKey, price: f64, raw: f64) -> f64 {
    match key.contract_size() {
        Some(cs) => raw * cs / price,
        None => raw,
    }
}
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
    /// Futures-only previous final id; Binance spot diffs carry no `pu`.
    #[serde(rename = "pu", default)]
    pub prev: Option<u64>,
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

/// Coinbase Exchange `level2_batch` snapshot message (`type == "snapshot"`).
#[derive(Debug, Clone, Deserialize)]
pub struct CoinbaseSnapshot {
    pub product_id: String,
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

/// Coinbase Exchange `l2update`: `[side, price, absolute size]`.
#[derive(Debug, Clone, Deserialize)]
pub struct CoinbaseUpdate {
    pub product_id: String,
    pub changes: Vec<[String; 3]>,
    pub time: String,
}

/// Coinbase DRB book level: `[action, price, amount]`; amount is BASE coin
/// for these linear USDC perpetuals (never multiplied by `contract_size`).
pub type DrbLevel = (String, serde_json::Number, serde_json::Number);

/// Coinbase DRB `book.<native>.100ms` data (snapshot or change).
#[derive(Debug, Clone, Deserialize)]
pub struct DrbBook {
    #[serde(rename = "type")]
    pub kind: String,
    pub instrument_name: String,
    pub timestamp: i64,
    pub change_id: u64,
    #[serde(default)]
    pub prev_change_id: Option<u64>,
    pub bids: Vec<DrbLevel>,
    pub asks: Vec<DrbLevel>,
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
    venue: Venue,
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
    #[cfg(test)]
    pub fn new(native_symbol: &str) -> Self {
        Self::with_venue(native_symbol, Venue::Futures)
    }

    pub fn with_venue(native_symbol: &str, venue: Venue) -> Self {
        Self {
            native_symbol: native_symbol.to_string(),
            venue,
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
        // Event shape must match the venue: futures carry `pu`, spot never.
        if ev.prev.is_some() != (self.venue == Venue::Futures) {
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
                // Futures: drop u < L, bridge U <= L <= u.
                // Spot: drop u <= L, bridge U <= L+1 <= u.
                let (stale, need) = match self.venue {
                    Venue::Futures => (ev.last < l, l),
                    Venue::Spot => (ev.last <= l, l.saturating_add(1)),
                    Venue::Coinbase | Venue::Drb => return Err(SyncError::SourceMismatch),
                };
                if stale {
                    return Ok(false); // predates snapshot
                }
                if ev.first > need {
                    return Err(SyncError::BridgeGap);
                }
                self.apply(&ev, rx)?;
                Ok(true)
            }
            Phase::Live { last_u } => {
                // Beyond the bridge, `pu` must chain AND `u` must strictly
                // advance (futures ids may skip, so no `U == last_u + 1`).
                match self.venue {
                    Venue::Futures => {
                        if ev.prev != Some(*last_u) || ev.last <= *last_u {
                            return Err(SyncError::SequenceGap);
                        }
                    }
                    // Spot (official rules): u <= local id => stale/duplicate,
                    // ignored; U > local id + 1 => gap; otherwise apply
                    // (overlapping ranges are valid, quantities absolute).
                    Venue::Coinbase | Venue::Drb => return Err(SyncError::SourceMismatch),
                    Venue::Spot => {
                        if ev.last <= *last_u {
                            return Ok(false);
                        }
                        if ev.first > last_u.saturating_add(1) {
                            return Err(SyncError::SequenceGap);
                        }
                    }
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
        let cap = self.venue.snapshot_limit();
        if snap.bids.len() > cap || snap.asks.len() > cap {
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

    /// Coinbase: validate the complete WS snapshot, then keep only the nearest
    /// `COINBASE_WINDOW_LEVELS` per side as an anchored price window. Stays
    /// non-live until the first timestamped l2update applies (the snapshot
    /// carries no exchange time).
    pub fn on_coinbase_snapshot(&mut self, snap: CoinbaseSnapshot) -> Result<(), SyncError> {
        if snap.product_id != self.native_symbol {
            return Err(SyncError::SourceMismatch);
        }
        if !matches!(self.phase, Phase::Buffering(..)) {
            return Err(SyncError::BadSnapshot); // second snapshot in one generation
        }
        if snap.bids.len() > COINBASE_MAX_SNAPSHOT_LEVELS || snap.asks.len() > COINBASE_MAX_SNAPSHOT_LEVELS {
            return Err(SyncError::BadSnapshot);
        }
        let mut bids = BTreeMap::new();
        let mut asks = BTreeMap::new();
        apply_side(&mut bids, &snap.bids, 0, i64::MAX).map_err(|_| SyncError::BadSnapshot)?;
        apply_side(&mut asks, &snap.asks, 0, i64::MAX).map_err(|_| SyncError::BadSnapshot)?;
        let (Some((b, _)), Some((a, _))) = (bids.last_key_value(), asks.first_key_value()) else {
            return Err(SyncError::BadSnapshot);
        };
        if b >= a {
            return Err(SyncError::BadSnapshot);
        }
        let n = COINBASE_WINDOW_LEVELS;
        let bids: BTreeMap<i64, f64> = bids.into_iter().rev().take(n).collect();
        let asks: BTreeMap<i64, f64> = asks.into_iter().take(n).collect();
        self.bid_floor = *bids.first_key_value().unwrap().0;
        self.ask_ceiling = *asks.last_key_value().unwrap().0;
        self.bids = bids;
        self.asks = asks;
        self.phase = Phase::AwaitBridge { snapshot_id: 0 };
        Ok(())
    }

    /// Coinbase l2update: absolute sizes, zero deletes, applied
    /// transactionally inside the anchored window. The resulting `seq` is an
    /// INTERNAL per-generation revision, never an exchange id.
    pub fn on_coinbase_update(&mut self, up: CoinbaseUpdate, rx: Instant) -> Result<bool, SyncError> {
        if up.product_id != self.native_symbol {
            return Err(SyncError::SourceMismatch);
        }
        let rev = match self.phase {
            Phase::Buffering(..) => return Err(SyncError::BridgeGap), // update before snapshot
            Phase::AwaitBridge { .. } => 1,
            Phase::Live { last_u } => last_u + 1,
        };
        if up.changes.len() > COINBASE_MAX_CHANGES {
            return Err(SyncError::BadLevel);
        }
        let t = chrono::DateTime::parse_from_rfc3339(&up.time).map_err(|_| SyncError::BadLevel)?.timestamp_millis();
        // SDK admission rejects exchange_time_ms <= 0: reject before any
        // mutation so a nonpositive time can never become server-live.
        if t <= 0 {
            return Err(SyncError::BadLevel);
        }
        let (mut bids, mut asks) = (Vec::new(), Vec::new());
        for [side, p, q] in up.changes {
            match side.as_str() {
                "buy" => bids.push([p, q]),
                "sell" => asks.push([p, q]),
                _ => return Err(SyncError::BadLevel),
            }
        }
        let ev = DiffEvent { event: String::new(), event_time: t, symbol: String::new(), first: rev, last: rev, prev: None, bids, asks };
        self.apply(&ev, rx)?;
        Ok(true)
    }

    /// Coinbase DRB: snapshot (live at its own positive timestamp, anchored
    /// nearest-5000 window) or change chained by `prev_change_id == last
    /// change_id` (ids may jump). Absolute amounts; delete removes.
    pub fn on_drb(&mut self, d: DrbBook, rx: Instant) -> Result<bool, SyncError> {
        if d.instrument_name != self.native_symbol {
            return Err(SyncError::SourceMismatch);
        }
        if d.timestamp <= 0 {
            return Err(SyncError::BadLevel);
        }
        if d.bids.len() > COINBASE_MAX_SNAPSHOT_LEVELS || d.asks.len() > COINBASE_MAX_SNAPSHOT_LEVELS {
            return Err(SyncError::BadLevel);
        }
        let conv = |v: &[DrbLevel], snap: bool| -> Result<Vec<[String; 2]>, SyncError> {
            v.iter()
                .map(|(a, p, q)| {
                    let qf = q.as_f64().filter(|x| x.is_finite() && *x >= 0.0).ok_or(SyncError::BadLevel)?;
                    let q = match (a.as_str(), snap) {
                        ("new", _) | ("change", false) if qf > 0.0 => q.to_string(),
                        ("delete", false) => "0".to_string(),
                        _ => return Err(SyncError::BadLevel),
                    };
                    Ok([p.to_string(), q])
                })
                .collect()
        };
        match d.kind.as_str() {
            "snapshot" => {
                if !matches!(self.phase, Phase::Buffering(..)) || d.prev_change_id.is_some() {
                    return Err(SyncError::BadSnapshot);
                }
                let b = conv(&d.bids, true).map_err(|_| SyncError::BadSnapshot)?;
                let a = conv(&d.asks, true).map_err(|_| SyncError::BadSnapshot)?;
                let mut bids = BTreeMap::new();
                let mut asks = BTreeMap::new();
                apply_side(&mut bids, &b, 0, i64::MAX).map_err(|_| SyncError::BadSnapshot)?;
                apply_side(&mut asks, &a, 0, i64::MAX).map_err(|_| SyncError::BadSnapshot)?;
                Self::check(&bids, &asks).map_err(|_| SyncError::BadSnapshot)?;
                let n = COINBASE_WINDOW_LEVELS;
                let bids: BTreeMap<i64, f64> = bids.into_iter().rev().take(n).collect();
                let asks: BTreeMap<i64, f64> = asks.into_iter().take(n).collect();
                self.bid_floor = *bids.first_key_value().unwrap().0;
                self.ask_ceiling = *asks.last_key_value().unwrap().0;
                self.bids = bids;
                self.asks = asks;
                self.phase = Phase::Live { last_u: d.change_id };
                self.last_event_ms = d.timestamp;
                self.last_applied_rx = Some(rx);
                Ok(true)
            }
            "change" => {
                let Phase::Live { last_u } = self.phase else {
                    return Err(SyncError::BridgeGap); // change before snapshot
                };
                if d.prev_change_id != Some(last_u) || d.change_id <= last_u {
                    return Err(SyncError::SequenceGap);
                }
                if d.bids.len() + d.asks.len() > COINBASE_MAX_CHANGES {
                    return Err(SyncError::BadLevel);
                }
                let ev = DiffEvent {
                    event: String::new(),
                    event_time: d.timestamp,
                    symbol: String::new(),
                    first: d.change_id,
                    last: d.change_id,
                    prev: None,
                    bids: conv(&d.bids, false)?,
                    asks: conv(&d.asks, false)?,
                };
                self.apply(&ev, rx)?;
                Ok(true)
            }
            _ => Err(SyncError::BadLevel),
        }
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

fn instrument(key: BookKey) -> Option<&'static crate::types::Instrument> {
    crate::types::INSTRUMENTS.iter().find(|i| {
        i.exchange == key.exchange && i.market == key.market && i.symbol == key.symbol
    })
}

/// Supported order-book sources (single exact source, no fallback): every
/// registered Binance instrument in `BOOK_MARKETS`. Coinbase is deferred.
pub fn supported(exchange: &str, market: &str, symbol: &str) -> Option<BookKey> {
    let sym = [Symbol::Btc, Symbol::Eth].into_iter().find(|x| x.as_str() == symbol)?;
    let key = match exchange {
        "binance" => BookKey::binance(BOOK_MARKETS.into_iter().find(|m| m.as_str() == market)?, sym),
        // Coinbase Exchange spot only; INTX/DRB books remain deferred.
        "coinbase" if market == "spot" => BookKey { exchange: Exchange::Coinbase, market: Market::Spot, symbol: sym },
        // Coinbase DRB linear USDC perpetuals (BTC_/ETH_USDC-PERPETUAL).
        "coinbase" if market == "usdc" => BookKey { exchange: Exchange::Coinbase, market: Market::Usdc, symbol: sym },
        _ => return None,
    };
    instrument(key).map(|_| key)
}

fn frame(
    key: BookKey,
    status: BookStatus,
    generation: u64,
    book: Option<&BookSync>,
    reason: Option<&str>,
) -> BookFrame {
    let inst = instrument(key).expect("supported symbol");
    let publish = key.venue().publish_levels();
    let (bids, asks, seq, t, cov, maintained) = match (status, book) {
        (BookStatus::Live, Some(b)) => {
            let (bids, asks) = b.top(publish);
            let conv = |v: Vec<[f64; 2]>| v.into_iter().map(|[p, q]| [p, base_qty(key, p, q)]).collect::<Vec<_>>();
            let (bids, asks) = (conv(bids), conv(asks));
            let (lo, hi) = b.published_coverage(publish);
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
        "source": {"exchange": key.exchange.as_str(), "market": key.market.as_str(), "symbol": key.symbol.as_str()},
        "native_symbol": inst.native_symbol,
        "raw_quantity": if key.contract_size().is_some() { "inverse_contracts" } else { "base" },
        "contract_size_quote": key.contract_size(),
        "base": key.symbol.as_str(),
        "quote": inst.quote_asset,
        "generation": generation,
        "seq": seq,
        // Binance: exchange final update id. Coinbase: internal per-generation
        // revision (the feed carries no upstream sequence).
        "seq_kind": if key.venue() == Venue::Coinbase { "internal_revision" } else { "exchange_update_id" },
        "exchange_time_ms": t,
        "depth_limit": key.venue().snapshot_limit(),
        "publish_limit": publish,
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
        key: BookKey,
        native_symbol: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>>;
    fn snapshot(
        &self,
        key: BookKey,
        native_symbol: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>;
}

pub struct BinanceFeed {
    pub http: reqwest::Client,
}

impl BookFeed for BinanceFeed {
    fn connect(
        &self,
        key: BookKey,
        native_symbol: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>> {
        let coinbase = matches!(key.venue(), Venue::Coinbase | Venue::Drb);
        let url = if key.venue() == Venue::Drb {
            "wss://streams.drb.coinbase.com/ws/api/v2".to_string()
        } else if coinbase {
            "wss://ws-feed.exchange.coinbase.com".to_string()
        } else {
            let host = match key.market {
                Market::Coinm => "wss://dstream.binance.com/ws",
                m if m.is_spot() => "wss://stream.binance.com:9443/ws",
                _ => "wss://fstream.binance.com/public/ws",
            };
            format!("{host}/{}@depth@100ms", native_symbol.to_ascii_lowercase())
        };
        let cap = if coinbase { COINBASE_MAX_MESSAGE_BYTES } else { MAX_EVENT_BYTES };
        let sub = if key.venue() == Venue::Drb {
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "public/subscribe", "params": {"channels": [drb_channel(native_symbol)]}})
        } else {
            serde_json::json!({"type": "subscribe", "product_ids": [native_symbol], "channels": ["level2_batch"]})
        }
        .to_string();
        Box::pin(async move {
            use futures::SinkExt;
            let mut cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
            cfg.max_message_size = Some(cap);
            cfg.max_frame_size = Some(cap);
            let (mut ws, _) = tokio_tungstenite::connect_async_with_config(&url, Some(cfg), false).await?;
            if coinbase {
                // Anonymous public subscription; no credentials.
                ws.send(tokio_tungstenite::tungstenite::Message::Text(sub.into())).await?;
            }
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
    fn snapshot(
        &self,
        key: BookKey,
        native_symbol: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        if matches!(key.venue(), Venue::Coinbase | Venue::Drb) {
            // Coinbase books bootstrap from the WS snapshot only.
            return Box::pin(async { bail!("no_rest_snapshot") });
        }
        let (base, limit) = match key.market {
            Market::Coinm => ("https://dapi.binance.com/dapi/v1/depth", SNAPSHOT_LIMIT),
            m if m.is_spot() => ("https://api.binance.com/api/v3/depth", SPOT_SNAPSHOT_LIMIT),
            _ => ("https://fapi.binance.com/fapi/v1/depth", SNAPSHOT_LIMIT),
        };
        let url = format!("{base}?symbol={}&limit={limit}", native_symbol.to_ascii_uppercase());
        Box::pin(async move { read_snapshot_body(self.http.get(url).send().await?).await })
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
    entries: Mutex<HashMap<BookKey, Entry>>,
    /// Per-symbol last snapshot start; outlives tasks so demand churn cannot
    /// bypass the snapshot budget.
    budget: Arc<Mutex<HashMap<BookKey, Instant>>>,
}

/// Subscriber demand; dropping the last one stops the symbol's task.
pub struct BookSubscription {
    hub: Arc<BookHub>,
    symbol: BookKey,
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

    pub fn active(&self) -> Vec<(BookKey, usize)> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(s, e)| (*s, e.subs)).collect()
    }

    pub fn subscribe(self: &Arc<Self>, symbol: BookKey) -> BookSubscription {
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
async fn reserve_snapshot(budget: &Mutex<HashMap<BookKey, Instant>>, symbol: BookKey) {
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
    symbol: BookKey,
    tx: watch::Sender<Arc<BookFrame>>,
    budget: Arc<Mutex<HashMap<BookKey, Instant>>>,
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
    if book.venue == Venue::Coinbase {
        return read_coinbase(&t, book, rx);
    }
    if book.venue == Venue::Drb {
        return read_drb(&t, book, rx);
    }
    if t.len() > MAX_EVENT_BYTES {
        return Err("oversized_input".into());
    }
    let ev = serde_json::from_str::<DiffEvent>(&t).map_err(|_| "bad_event".to_string())?;
    book.on_event_at(ev, rx, t.len()).map_err(|e| format!("{e:?}").to_ascii_lowercase())
}

#[derive(Deserialize)]
struct CbType<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
}

/// Coinbase message dispatch: subscription acks are not book updates;
/// `error`, unknown types and bad shapes end the generation (resubscribe).
fn read_coinbase(t: &str, book: &mut BookSync, rx: Instant) -> Result<bool, String> {
    if t.len() > COINBASE_MAX_MESSAGE_BYTES {
        return Err("oversized_input".into());
    }
    let kind = serde_json::from_str::<CbType>(t).map_err(|_| "bad_event".to_string())?.kind;
    let err = |e: SyncError| format!("{e:?}").to_ascii_lowercase();
    match kind {
        "subscriptions" => Ok(false),
        "snapshot" => {
            let s = serde_json::from_str::<CoinbaseSnapshot>(t).map_err(|_| "bad_event".to_string())?;
            book.on_coinbase_snapshot(s).map(|_| false).map_err(err)
        }
        "l2update" => {
            let u = serde_json::from_str::<CoinbaseUpdate>(t).map_err(|_| "bad_event".to_string())?;
            book.on_coinbase_update(u, rx).map_err(err)
        }
        "error" => Err("upstream_error".into()),
        _ => Err("bad_event".into()),
    }
}

/// Exact DRB raw (non-grouped) 100ms book channel for one instrument.
pub fn drb_channel(native: &str) -> String {
    format!("book.{native}.100ms")
}

#[derive(Deserialize)]
struct DrbMsg {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<DrbParams>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct DrbParams {
    channel: String,
    data: DrbBook,
}

/// DRB JSON-RPC dispatch: the subscribe ack (must list exactly our channel)
/// is control; errors, other methods/channels and bad shapes end the
/// generation (resubscribe, fresh snapshot).
fn read_drb(t: &str, book: &mut BookSync, rx: Instant) -> Result<bool, String> {
    if t.len() > COINBASE_MAX_MESSAGE_BYTES {
        return Err("oversized_input".into());
    }
    let m = serde_json::from_str::<DrbMsg>(t).map_err(|_| "bad_event".to_string())?;
    if m.error.is_some() {
        return Err("upstream_error".into());
    }
    // JSON-RPC 2.0 only; the ack must answer our fixed subscribe id 1.
    if m.jsonrpc.as_deref() != Some("2.0") {
        return Err("bad_event".into());
    }
    let chan = drb_channel(&book.native_symbol);
    match (m.method.as_deref(), m.params, m.result) {
        (None, None, Some(r)) if m.id == Some(serde_json::json!(1)) && r == serde_json::json!([chan]) => Ok(false),
        (Some("subscription"), Some(p), None) if p.channel == chan => {
            book.on_drb(p.data, rx).map_err(|e| format!("{e:?}").to_ascii_lowercase())
        }
        _ => Err("bad_event".into()),
    }
}

/// Runs one synchronized generation; returns the fixed-vocabulary reason it
/// ended and how long it was continuously live.
async fn run_generation(
    feed: &dyn BookFeed,
    symbol: BookKey,
    native: &str,
    generation: u64,
    tx: &watch::Sender<Arc<BookFrame>>,
    budget: &Mutex<HashMap<BookKey, Instant>>,
) -> (String, Duration) {
    let none = Duration::ZERO;
    let mut stream = match tokio::time::timeout(CONNECT_DEADLINE, feed.connect(symbol, native)).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => return ("connect_failed".into(), none),
        Err(_) => return ("connect_timeout".into(), none),
    };
    let mut book = BookSync::with_venue(native, symbol.venue());
    // Absolute deadline: snapshot + bridge must complete by then; incoming
    // diffs never extend it.
    let sync_deadline = tokio::time::sleep_until(Instant::now() + SYNC_DEADLINE);
    tokio::pin!(sync_deadline);
    // The per-symbol budget is reserved at the ACTUAL snapshot initiation
    // (after connect); diffs keep buffering under the bounded budgets and the
    // absolute sync deadline above.
    // Coinbase bootstraps from its own WS snapshot (no REST bridge): the
    // shared loop below keeps it syncing under the same absolute deadline
    // until the snapshot and first timestamped update have applied.
    if !matches!(symbol.venue(), Venue::Coinbase | Venue::Drb) {
        let snap = async {
            reserve_snapshot(budget, symbol).await;
            feed.snapshot(symbol, native).await
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
