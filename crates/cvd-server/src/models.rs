use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct BinanceCombinedMsg {
    pub stream: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct AggTrade {
    #[serde(rename = "s")]
    pub symbol: Option<String>,
    #[serde(rename = "a")]
    pub id: u64,
    #[serde(rename = "e")]
    #[serde(default)]
    pub event_type: String,
    #[serde(rename = "p")]
    pub price: String,
    #[serde(rename = "q")]
    pub quantity: String,
    #[serde(rename = "T")]
    pub trade_time: i64,
    #[serde(rename = "m")]
    pub is_buyer_maker: bool,
}

// Coinbase "match" message schema
#[derive(Debug, Deserialize)]
pub struct CoinbaseMatch {
    pub trade_id: u64,
    #[serde(rename = "type")]
    pub msg_type: String, // "match"
    pub time: String, // ISO8601
    pub product_id: String,
    pub size: String,
    pub price: String,
    pub side: String, // maker side: "buy" or "sell"
}

#[derive(Debug, Deserialize)]
pub struct CoinbaseHeartbeat {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub product_id: String,
    pub last_trade_id: u64,
}

// Coinbase subscribe request
#[derive(Debug, Serialize)]
pub struct CoinbaseSubscribe<'a> {
    #[serde(rename = "type")]
    pub msg_type: &'a str, // "subscribe"
    pub product_ids: Vec<&'a str>,
    pub channels: Vec<&'a str>, // spot: ["matches", "heartbeat"]
}

#[derive(Debug, Clone, Serialize)]
pub struct Bar {
    pub time: i64, // unix seconds (lightweight-charts)
    #[serde(rename = "po")]
    pub price_open: f64,
    #[serde(rename = "ph")]
    pub price_high: f64,
    #[serde(rename = "pl")]
    pub price_low: f64,
    pub price_close: f64,
    #[serde(rename = "apo")]
    pub aggregate_price_open: f64,
    #[serde(rename = "aph")]
    pub aggregate_price_high: f64,
    #[serde(rename = "apl")]
    pub aggregate_price_low: f64,
    #[serde(rename = "apc")]
    pub aggregate_price_close: f64,
    pub delta_usdt: f64,
    #[serde(rename = "ad")]
    pub aggregate_delta_usdt: f64,
    pub cvd_usdt: f64,
    #[serde(rename = "ac")]
    pub aggregate_cvd_usdt: f64,
    #[serde(rename = "bvb")]
    pub buy_volume_base: f64,
    #[serde(rename = "abvb")]
    pub aggregate_buy_volume_base: f64,
    #[serde(rename = "svb")]
    pub sell_volume_base: f64,
    #[serde(rename = "asvb")]
    pub aggregate_sell_volume_base: f64,
    pub buy_volume_usdt: f64,
    #[serde(rename = "agb")]
    pub aggregate_buy_volume_usdt: f64,
    pub sell_volume_usdt: f64,
    #[serde(rename = "ags")]
    pub aggregate_sell_volume_usdt: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<crate::quality::BarQuality>,
    pub trades: u64,
}

impl Bar {
    pub fn is_renderable(&self) -> bool {
        self.time > 0
            && self.price_open.is_finite()
            && self.price_high.is_finite()
            && self.price_low.is_finite()
            && self.price_close.is_finite()
            && self.aggregate_price_open.is_finite()
            && self.aggregate_price_high.is_finite()
            && self.aggregate_price_low.is_finite()
            && self.aggregate_price_close.is_finite()
            && self.delta_usdt.is_finite()
            && self.aggregate_delta_usdt.is_finite()
            && self.cvd_usdt.is_finite()
            && self.aggregate_cvd_usdt.is_finite()
            && self.buy_volume_base.is_finite()
            && self.aggregate_buy_volume_base.is_finite()
            && self.sell_volume_base.is_finite()
            && self.aggregate_sell_volume_base.is_finite()
            && self.buy_volume_usdt.is_finite()
            && self.aggregate_buy_volume_usdt.is_finite()
            && self.sell_volume_usdt.is_finite()
            && self.aggregate_sell_volume_usdt.is_finite()
    }
}

#[derive(Debug, Serialize)]
pub struct HistoryPayload {
    pub series: &'static str,
    pub bars: Vec<Bar>,
    pub has_more_before: bool,
    pub has_more_after: bool,
    pub oldest_time: Option<i64>,
    pub newest_time: Option<i64>,
    pub realtime_enabled: bool,
    /// Canonical bucket times inside this page's range that were dropped as
    /// non-renderable. Consumers must not treat a window spanning them as
    /// contiguous valid input (successor of the retired server-side guard).
    pub invalid_times: Vec<i64>,
    /// Inclusive scan coverage of this page (null = unbounded on that side).
    /// `invalid_times` lists every non-renderable bucket in it, including
    /// leading/trailing rows at page seams; also the cursor of an all-invalid page.
    pub coverage_from: Option<i64>,
    pub coverage_to: Option<i64>,
    /// Producer process epoch; `rev` values compare only within one epoch.
    pub epoch: u64,
    /// Live revision-bound validity marks inside the coverage, overlaid on the
    /// possibly lagging DB snapshot; the browser orders these against WS frames.
    pub validity: Vec<ValidityMark>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ValidityMark {
    pub time: i64,
    pub rev: u64,
    pub valid: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WsAction {
    Append,
    ReplaceLast,
    /// The canonical bucket at `invalid_time` became non-renderable (e.g. a
    /// same-time correction). Any earlier value for that bucket is no longer
    /// authoritative; consumers must drop it and refuse windows spanning it
    /// until a later-`rev` valid Append/ReplaceLast of the same time repairs it.
    Invalidate,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsPayload {
    /// Per-source monotonically increasing input revision (host-local). Live
    /// indicator frames report `as_of_rev` so clients can tell whether an
    /// output was computed from this bar version or an older one.
    #[serde(default)]
    pub rev: u64,
    pub series: &'static str,
    pub action: WsAction,
    /// Present for Append/ReplaceLast; absent for Invalidate (no fabricated OHLC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar: Option<Bar>,
    /// Present only for Invalidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_time: Option<i64>,
    /// Producer process epoch (see HistoryPayload::epoch).
    pub epoch: u64,
}

impl WsPayload {
    pub fn bar(rev: u64, series: &'static str, action: WsAction, bar: Bar) -> Self {
        Self { rev, series, action, bar: Some(bar), invalid_time: None, epoch: crate::producer_epoch() }
    }
    pub fn invalidate(rev: u64, series: &'static str, time: i64) -> Self {
        Self { rev, series, action: WsAction::Invalidate, bar: None, invalid_time: Some(time), epoch: crate::producer_epoch() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BarRow {
    pub time: i64,
    pub price_open: f64,
    pub price_high: f64,
    pub price_low: f64,
    pub price_close: f64,
    pub delta_usdt: f64,
    pub cvd_usdt: f64,
    pub buy_volume_base: f64,
    pub sell_volume_base: f64,
    pub buy_volume_usdt: f64,
    pub sell_volume_usdt: f64,
    pub trades: i64,
}

impl From<&Bar> for BarRow {
    fn from(bar: &Bar) -> Self {
        Self {
            time: bar.time,
            price_open: bar.price_open,
            price_high: bar.price_high,
            price_low: bar.price_low,
            price_close: bar.price_close,
            delta_usdt: bar.delta_usdt,
            cvd_usdt: bar.cvd_usdt,
            buy_volume_base: bar.buy_volume_base,
            sell_volume_base: bar.sell_volume_base,
            buy_volume_usdt: bar.buy_volume_usdt,
            sell_volume_usdt: bar.sell_volume_usdt,
            trades: bar.trades as i64,
        }
    }
}

impl From<BarRow> for Bar {
    fn from(r: BarRow) -> Self {
        Self {
            time: r.time,
            price_open: r.price_open,
            price_high: r.price_high,
            price_low: r.price_low,
            price_close: r.price_close,
            aggregate_price_open: r.price_open,
            aggregate_price_high: r.price_high,
            aggregate_price_low: r.price_low,
            aggregate_price_close: r.price_close,
            delta_usdt: r.delta_usdt,
            aggregate_delta_usdt: 0.0,
            cvd_usdt: r.cvd_usdt,
            aggregate_cvd_usdt: 0.0,
            buy_volume_base: r.buy_volume_base,
            aggregate_buy_volume_base: 0.0,
            sell_volume_base: r.sell_volume_base,
            aggregate_sell_volume_base: 0.0,
            buy_volume_usdt: r.buy_volume_usdt,
            aggregate_buy_volume_usdt: 0.0,
            sell_volume_usdt: r.sell_volume_usdt,
            aggregate_sell_volume_usdt: 0.0,
            quality: None,
            trades: r.trades as u64,
        }
    }
}
