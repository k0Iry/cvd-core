//! Live order-book plugin contract (`OP_BOOK`), browser WASM only.
//!
//! The trusted server owns the exchange book; the browser host hands a
//! bounded, validated, read-only [`BookSnapshot`] of ONE source to a book
//! plugin, which returns declarative [`PriceLevels`] rows for the generic
//! price-level panel. Only `live` books are ever handed to plugins: syncing,
//! stale or failed books are withdrawn by the host, never computed.
//!
//! Book plugins (an [`crate::Indicator`] with `type Domain = Book`) export
//! the same `cvd-wasm` v1 functions as bar plugins; the descriptor declares the `order_book`
//! input and one `price_levels` output. Bar plugins are untouched.

use crate::model::{codes, PluginError};
use serde::{Deserialize, Serialize};

/// Maximum levels per side in a snapshot handed to a plugin (Binance spot
/// REST depth is 5000/side).
pub const MAX_BOOK_LEVELS: usize = 5000;
/// Maximum rows a plugin may return.
pub const MAX_PRICE_LEVEL_ROWS: usize = 400;

/// Single exchange/market/symbol identity; never an aggregate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BookSource {
    pub exchange: String,
    pub market: String,
    pub symbol: String,
}

/// Price range inside which the book is complete (from the exchange snapshot
/// depth). Levels outside it are unknown, not zero.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BookCoverage {
    pub bid_floor: f64,
    pub ask_ceiling: f64,
}

/// Body of `OP_BOOK`. `bids` best (highest) first, `asks` best (lowest) first;
/// each level is `[price, quantity_in_base_units]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookSnapshot {
    pub source: BookSource,
    /// Base and quote asset of the instrument (e.g. `BTC`, `USDT`).
    pub base: String,
    pub quote: String,
    /// Server sync generation (changes on every resync/reconnect).
    pub generation: u64,
    /// Opaque book revision, scoped to (`source`, `generation`): the upstream
    /// update id when the venue provides one, otherwise a server-internal
    /// per-generation revision. Compare only within one source+generation;
    /// never do arithmetic across sources or generations.
    pub seq: u64,
    /// Exchange event time (unix ms) of the last applied event.
    pub exchange_time_ms: i64,
    pub bids: Vec<[f64; 2]>,
    pub asks: Vec<[f64; 2]>,
    pub coverage: BookCoverage,
}

fn bad(m: &str) -> PluginError {
    PluginError::invalid_input(m.to_string())
}

impl BookSnapshot {
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().map(|l| l[0])
    }
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().map(|l| l[0])
    }

    /// Canonical admission shared by SDK and browser host.
    pub fn check(&self) -> Result<(), PluginError> {
        let s = &self.source;
        for t in [&s.exchange, &s.market, &s.symbol, &self.base, &self.quote] {
            if t.is_empty() || t.len() > 16 || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(bad("invalid book identity"));
            }
        }
        if self.bids.is_empty() || self.asks.is_empty() {
            return Err(bad("book side is empty"));
        }
        if self.bids.len() > MAX_BOOK_LEVELS || self.asks.len() > MAX_BOOK_LEVELS {
            return Err(bad("too many book levels"));
        }
        let ok = |l: &[f64; 2]| l[0].is_finite() && l[0] > 0.0 && l[1].is_finite() && l[1] > 0.0;
        if !self.bids.iter().all(ok) || !self.asks.iter().all(ok) {
            return Err(bad("book level must have positive finite price and quantity"));
        }
        if self.bids.windows(2).any(|w| w[0][0] <= w[1][0]) || self.asks.windows(2).any(|w| w[0][0] >= w[1][0]) {
            return Err(bad("book levels are not strictly ordered"));
        }
        if self.bids[0][0] >= self.asks[0][0] {
            return Err(bad("book is crossed"));
        }
        let c = self.coverage;
        if !(c.bid_floor.is_finite() && c.ask_ceiling.is_finite() && c.bid_floor > 0.0 && c.bid_floor < c.ask_ceiling)
            || self.bids.last().is_some_and(|l| l[0] < c.bid_floor)
            || self.asks.last().is_some_and(|l| l[0] > c.ask_ceiling)
        {
            return Err(bad("book levels outside declared coverage"));
        }
        if self.exchange_time_ms <= 0 {
            return Err(bad("invalid book time"));
        }
        Ok(())
    }
}

/// One output row: resting quantity at (or grouped into) `price`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct PriceLevelRow {
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
}

/// `OP_BOOK` result. Rows strictly descending by price; `unit` labels the
/// quantity columns (e.g. `BTC` or `USDT`); `best_bid`/`best_ask` must echo
/// the input book so the panel can draw the spread line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PriceLevels {
    pub unit: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub rows: Vec<PriceLevelRow>,
}

impl PriceLevels {
    pub fn validate(&self, input: &BookSnapshot) -> Result<(), PluginError> {
        let out = |m: &str| PluginError::new(codes::INVALID_OUTPUT, m.to_string());
        if self.unit.is_empty() || self.unit.len() > 16 || !self.unit.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(out("invalid unit label"));
        }
        if Some(self.best_bid) != input.best_bid() || Some(self.best_ask) != input.best_ask() {
            return Err(out("best bid/ask differ from input book"));
        }
        if self.rows.is_empty() || self.rows.len() > MAX_PRICE_LEVEL_ROWS {
            return Err(out("rows must be 1..=400"));
        }
        let ok = |r: &PriceLevelRow| {
            r.price.is_finite()
                && r.price > 0.0
                && r.bid.is_finite()
                && r.bid >= 0.0
                && r.ask.is_finite()
                && r.ask >= 0.0
        };
        if !self.rows.iter().all(ok) || self.rows.windows(2).any(|w| w[0].price <= w[1].price) {
            return Err(out("rows must be finite, non-negative and strictly descending"));
        }
        Ok(())
    }
}
