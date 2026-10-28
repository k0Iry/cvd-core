use crate::models::Bar;
use crate::types::{Symbol, Tf};

/// Point-in-time snapshot of a completed second, passed to flush_second.
pub struct SecSnapshot {
    pub sec: i64,
    pub open_price: f64,
    pub high_price: f64,
    pub low_price: f64,
    pub last_price: f64,
    pub delta_usdt: f64,
    pub buy_volume_base: f64,
    pub sell_volume_base: f64,
    pub buy_volume_usdt: f64,
    pub sell_volume_usdt: f64,
    pub trades: u64,
}

#[derive(Debug, Clone)]
pub struct BucketAgg {
    pub cur_bucket_id: Option<i64>,
    pub open_price: f64,
    pub high_price: f64,
    pub low_price: f64,
    pub last_price: f64,
    pub delta_usdt: f64,
    pub buy_volume_base: f64,
    pub sell_volume_base: f64,
    pub buy_volume_usdt: f64,
    pub sell_volume_usdt: f64,
    pub trades: u64,
}
impl BucketAgg {
    pub fn new() -> Self {
        Self {
            cur_bucket_id: None,
            open_price: f64::NAN,
            high_price: f64::NAN,
            low_price: f64::NAN,
            last_price: f64::NAN,
            delta_usdt: 0.0,
            buy_volume_base: 0.0,
            sell_volume_base: 0.0,
            buy_volume_usdt: 0.0,
            sell_volume_usdt: 0.0,
            trades: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SecAgg {
    pub cur_sec: Option<i64>,
    pub open_price: f64,
    pub high_price: f64,
    pub low_price: f64,
    pub last_price: f64,
    pub delta_usdt: f64,
    pub buy_volume_base: f64,
    pub sell_volume_base: f64,
    pub buy_volume_usdt: f64,
    pub sell_volume_usdt: f64,
    pub trades: u64,
}
impl SecAgg {
    pub fn new() -> Self {
        Self {
            cur_sec: None,
            open_price: f64::NAN,
            high_price: f64::NAN,
            low_price: f64::NAN,
            last_price: f64::NAN,
            delta_usdt: 0.0,
            buy_volume_base: 0.0,
            sell_volume_base: 0.0,
            buy_volume_usdt: 0.0,
            sell_volume_usdt: 0.0,
            trades: 0,
        }
    }

    pub fn reset_to(
        &mut self,
        sec: i64,
        price: f64,
        delta_usdt: f64,
        buy_volume_base: f64,
        sell_volume_base: f64,
        buy_volume_usdt: f64,
        sell_volume_usdt: f64,
        _symbol: Symbol,
    ) {
        self.cur_sec = Some(sec);
        self.open_price = price;
        self.high_price = price;
        self.low_price = price;
        self.last_price = price;
        self.delta_usdt = delta_usdt;
        self.buy_volume_base = buy_volume_base;
        self.sell_volume_base = sell_volume_base;
        self.buy_volume_usdt = buy_volume_usdt;
        self.sell_volume_usdt = sell_volume_usdt;
        self.trades = 1;
    }

    pub fn add(
        &mut self,
        price: f64,
        delta_usdt: f64,
        buy_volume_base: f64,
        sell_volume_base: f64,
        buy_volume_usdt: f64,
        sell_volume_usdt: f64,
        _symbol: Symbol,
    ) {
        if self.trades == 0 || self.open_price.is_nan() {
            self.open_price = price;
            self.high_price = price;
            self.low_price = price;
        } else {
            self.high_price = self.high_price.max(price);
            self.low_price = self.low_price.min(price);
        }
        self.last_price = price;
        self.delta_usdt += delta_usdt;
        self.buy_volume_base += buy_volume_base;
        self.sell_volume_base += sell_volume_base;
        self.buy_volume_usdt += buy_volume_usdt;
        self.sell_volume_usdt += sell_volume_usdt;
        self.trades += 1;
    }

    /// Capture the current accumulated state as a snapshot.
    /// Returns None if no trades have been seen yet (cur_sec is None).
    pub fn snapshot(&self) -> Option<SecSnapshot> {
        Some(SecSnapshot {
            sec: self.cur_sec?,
            open_price: self.open_price,
            high_price: self.high_price,
            low_price: self.low_price,
            last_price: self.last_price,
            delta_usdt: self.delta_usdt,
            buy_volume_base: self.buy_volume_base,
            sell_volume_base: self.sell_volume_base,
            buy_volume_usdt: self.buy_volume_usdt,
            sell_volume_usdt: self.sell_volume_usdt,
            trades: self.trades,
        })
    }
}

pub fn push_bucket(
    agg: &mut BucketAgg,
    tf: Tf,
    ts_ms: i64,
    open_price: f64,
    high_price: f64,
    low_price: f64,
    close_price: f64,
    delta_usdt: f64,
    buy_volume_base: f64,
    sell_volume_base: f64,
    buy_volume_usdt: f64,
    sell_volume_usdt: f64,
    trades: u64,
) -> Option<Bar> {
    let tf_ms = tf.ms();
    let bucket_id = ts_ms / tf_ms;

    match agg.cur_bucket_id {
        None => agg.cur_bucket_id = Some(bucket_id),
        Some(cur) if cur != bucket_id => {
            if agg.trades == 0 || agg.last_price.is_nan() {
                agg.cur_bucket_id = Some(bucket_id);
                agg.open_price = open_price;
                agg.high_price = high_price;
                agg.low_price = low_price;
                agg.last_price = close_price;
                agg.delta_usdt = delta_usdt;
                agg.buy_volume_base = buy_volume_base;
                agg.sell_volume_base = sell_volume_base;
                agg.buy_volume_usdt = buy_volume_usdt;
                agg.sell_volume_usdt = sell_volume_usdt;
                agg.trades = trades;
                return None;
            }

            let end_ms = (cur + 1) * tf_ms - 1;
            let end_sec = end_ms / 1000;

            let bar = Bar {
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
                cvd_usdt: 0.0,
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
            };

            agg.cur_bucket_id = Some(bucket_id);
            agg.open_price = open_price;
            agg.high_price = high_price;
            agg.low_price = low_price;
            agg.last_price = close_price;
            agg.delta_usdt = delta_usdt;
            agg.buy_volume_base = buy_volume_base;
            agg.sell_volume_base = sell_volume_base;
            agg.buy_volume_usdt = buy_volume_usdt;
            agg.sell_volume_usdt = sell_volume_usdt;
            agg.trades = trades;

            return Some(bar);
        }
        _ => {}
    }

    if agg.trades == 0 || agg.open_price.is_nan() {
        agg.open_price = open_price;
        agg.high_price = high_price;
        agg.low_price = low_price;
    } else {
        agg.high_price = agg.high_price.max(high_price);
        agg.low_price = agg.low_price.min(low_price);
    }
    agg.last_price = close_price;
    agg.delta_usdt += delta_usdt;
    agg.buy_volume_base += buy_volume_base;
    agg.sell_volume_base += sell_volume_base;
    agg.buy_volume_usdt += buy_volume_usdt;
    agg.sell_volume_usdt += sell_volume_usdt;
    agg.trades += trades;

    None
}
