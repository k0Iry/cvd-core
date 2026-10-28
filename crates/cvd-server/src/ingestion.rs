use std::collections::{HashSet, VecDeque};

use anyhow::Result;

use crate::aggregator::{BucketAgg, SecAgg};
use crate::types::{Exchange, Market, Symbol, Tf};
use crate::AppState;

pub struct SymbolState {
    pub sec: SecAgg,
    pub a1: BucketAgg,
    pub a5: BucketAgg,
    pub a15: BucketAgg,
    pub a30: BucketAgg,
    pub a1h: BucketAgg,
    pub cvd_closed_usdt: f64,
    pub last_trade_id: Option<String>,
    // First witnessed spot heartbeat: an origin, not an ingested trade.
    // Retained across socket workers until the first actual trade is committed.
    pub(crate) spot_bootstrap_id: Option<u64>,
    pub last_trade_ms: Option<i64>,
    pub recovery_from_ms: Option<i64>,
    replay_frontier_ms: Option<i64>,
    replay_ids: HashSet<String>,
    recent_ids: HashSet<String>,
    recent_order: VecDeque<String>,
}

impl SymbolState {
    pub fn new() -> Self {
        Self {
            sec: SecAgg::new(),
            a1: BucketAgg::new(),
            a5: BucketAgg::new(),
            a15: BucketAgg::new(),
            a30: BucketAgg::new(),
            a1h: BucketAgg::new(),
            cvd_closed_usdt: 0.0,
            last_trade_id: None,
            spot_bootstrap_id: None,
            last_trade_ms: None,
            recovery_from_ms: None,
            replay_frontier_ms: None,
            replay_ids: HashSet::new(),
            recent_ids: HashSet::new(),
            recent_order: VecDeque::new(),
        }
    }

    pub(crate) fn is_duplicate(&self, trade: &Trade, ordered_ids: bool) -> bool {
        self.recovery_from_ms.is_some_and(|from| trade.timestamp_ms < from)
            || self.replay_ids.contains(&trade.id)
            || self.recent_ids.contains(&trade.id)
            || (ordered_ids
                && self.last_trade_id.as_ref().is_some_and(|last| {
                    last.parse::<u64>()
                        .ok()
                        .zip(trade.id.parse::<u64>().ok())
                        .is_some_and(|(previous, current)| current <= previous)
                }))
    }

    pub fn remember_replay(&mut self, trades: &[Trade]) {
        // Retain exact replay identities until the live stream crosses the fence.
        // Unlike a timestamp watermark, this cannot hide an unseen queued trade.
        self.replay_frontier_ms = trades.iter().map(|t| t.timestamp_ms).max();
        self.replay_ids = trades.iter().map(|t| t.id.clone()).collect();
    }

    fn remember(&mut self, trade: &Trade) {
        if self.replay_frontier_ms.is_some_and(|end| trade.timestamp_ms > end) {
            self.replay_ids.clear();
            self.replay_frontier_ms = None;
        }
        self.last_trade_id = Some(trade.id.clone());
        self.spot_bootstrap_id = None;
        self.last_trade_ms = Some(trade.timestamp_ms);
        self.recent_ids.insert(trade.id.clone());
        self.recent_order.push_back(trade.id.clone());
        if self.recent_order.len() > 4096 {
            if let Some(oldest) = self.recent_order.pop_front() {
                self.recent_ids.remove(&oldest);
            }
        }
    }

    pub fn restore_minutes(&mut self, minutes: &[crate::Bar], last_closed_time: i64) {
        let next_minute_ms = (last_closed_time + 1) * 1000;
        for (tf, bucket) in
            [(Tf::M5, &mut self.a5), (Tf::M15, &mut self.a15), (Tf::M30, &mut self.a30), (Tf::H1, &mut self.a1h)]
        {
            let start_ms = next_minute_ms / tf.ms() * tf.ms();
            for bar in minutes.iter().filter(|bar| bar.time * 1000 >= start_ms && bar.time <= last_closed_time) {
                crate::aggregator::push_bucket(
                    bucket,
                    tf,
                    bar.time * 1000,
                    bar.price_open,
                    bar.price_high,
                    bar.price_low,
                    bar.price_close,
                    bar.delta_usdt,
                    bar.buy_volume_base,
                    bar.sell_volume_base,
                    bar.buy_volume_usdt,
                    bar.sell_volume_usdt,
                    bar.trades,
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Trade {
    pub id: String,
    pub timestamp_ms: i64,
    pub price: f64,
    pub base: f64,
    pub quote: f64,
    pub is_buy: bool,
}

impl Trade {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.id.is_empty() && self.timestamp_ms > 0, "missing trade identity or timestamp");
        anyhow::ensure!(
            [self.price, self.base, self.quote].into_iter().all(|value| value.is_finite() && value > 0.0),
            "invalid trade price or volume"
        );
        Ok(())
    }
}

pub async fn ingest_trade(
    state: &AppState,
    exchange: Exchange,
    market: Market,
    symbol: Symbol,
    trade: Trade,
) -> Result<()> {
    trade.validate()?;
    let shard = crate::pick_shard(state, exchange, market, symbol);
    let mut guard = shard.engine.lock().await;
    anyhow::ensure!(!shard.recovery.read().await.needs_backfill(), "source requires historical backfill");
    let engine = &mut *guard;
    if engine.is_duplicate(&trade, true) {
        return Ok(());
    }
    anyhow::ensure!(
        engine.last_trade_ms.is_none_or(|last| trade.timestamp_ms >= last),
        "unseen out-of-order trade cannot be silently skipped: {}",
        trade.id
    );
    let second = trade.timestamp_ms / 1000;
    anyhow::ensure!(
        !engine.sec.cur_sec.is_some_and(|current| second < current),
        "late trade would rewind a closed second: {}",
        trade.id
    );
    if engine.sec.cur_sec.is_some_and(|current| second > current) {
        if let Some(snapshot) = engine.sec.snapshot() {
            crate::flush_second(
                state,
                exchange,
                market,
                symbol,
                &mut engine.a1,
                &mut engine.a5,
                &mut engine.a15,
                &mut engine.a30,
                &mut engine.a1h,
                &mut engine.cvd_closed_usdt,
                &snapshot,
            )
            .await;
            *shard.cvd_closed_usdt.write().await = engine.cvd_closed_usdt;
        }
    }
    let delta = if trade.is_buy { trade.quote } else { -trade.quote };
    let (buy_base, sell_base, buy_quote, sell_quote) =
        if trade.is_buy { (trade.base, 0.0, trade.quote, 0.0) } else { (0.0, trade.base, 0.0, trade.quote) };
    if engine.sec.cur_sec == Some(second) {
        engine.sec.add(trade.price, delta, buy_base, sell_base, buy_quote, sell_quote, symbol);
    } else {
        engine.sec.reset_to(second, trade.price, delta, buy_base, sell_base, buy_quote, sell_quote, symbol);
    }
    engine.remember(&trade);
    let recovery = shard.recovery.read().await;
    if !recovery.is_recovering() && !recovery.needs_backfill() {
        drop(recovery);
        *shard.stream_error.write().await = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(id: &str, timestamp_ms: i64) -> Trade {
        Trade { id: id.into(), timestamp_ms, price: 100.0, base: 1.0, quote: 100.0, is_buy: true }
    }

    #[tokio::test]
    async fn reconnect_keeps_partial_second_and_all_buckets() {
        let state = crate::tests::state().await;
        let ingest = |trade| ingest_trade(&state, Exchange::Binance, Market::Spot, Symbol::Btc, trade);
        ingest(trade("1", 60_001)).await.unwrap();
        ingest(trade("2", 61_001)).await.unwrap();
        ingest(trade("2", 61_001)).await.unwrap();
        ingest(trade("3", 61_500)).await.unwrap();
        let shard = crate::pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
        let engine = shard.engine.lock().await;
        assert_eq!(engine.sec.trades, 2);
        for bucket in [&engine.a1, &engine.a5, &engine.a15, &engine.a30, &engine.a1h] {
            assert_eq!(bucket.trades, 1);
            assert_eq!(bucket.delta_usdt, 100.0);
        }
    }

    #[tokio::test]
    async fn hour_boundary_enqueues_all_timeframes_with_consistent_cvd() {
        let state = crate::tests::state().await;
        for (id, timestamp) in [("1", 3_599_001), ("2", 3_600_001), ("3", 3_601_001)] {
            ingest_trade(&state, Exchange::Binance, Market::Spot, Symbol::Btc, trade(id, timestamp)).await.unwrap();
        }
        assert_eq!(state.outbox.pending_count().await.unwrap(), 5);
        let shard = crate::pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
        assert_eq!(*shard.cvd_closed_usdt.read().await, 100.0);
        for tf in [Tf::M1, Tf::M5, Tf::M15, Tf::M30, Tf::H1] {
            let history = shard.for_tf(tf).0.read().await;
            assert_eq!(history.len(), 1);
            let closed = history.back().unwrap();
            assert_eq!(closed.time, 3599);
            assert_eq!(closed.delta_usdt, 100.0);
            assert_eq!(closed.cvd_usdt, 100.0);
            let live = shard.live_bar_for_tf(tf).read().await;
            assert_eq!(live.as_ref().unwrap().cvd_usdt, 200.0);
        }
    }

    #[tokio::test]
    async fn late_trade_does_not_rewind_aggregation() {
        let state = crate::tests::state().await;
        ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, trade("first", 61_001)).await.unwrap();
        assert!(ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, trade("late", 60_001))
            .await
            .is_err());
        let engine = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc).engine.lock().await;
        assert_eq!(engine.sec.cur_sec, Some(61));
        assert_eq!(engine.sec.trades, 1);
    }

    #[test]
    fn restart_rebuilds_only_unfinished_higher_buckets() {
        let mut first = crate::tests::bar(59, 110.0);
        first.delta_usdt = 10.0;
        let mut second = crate::tests::bar(119, 113.0);
        second.delta_usdt = 3.0;
        let mut engine = SymbolState::new();
        engine.restore_minutes(&[first, second], 119);
        assert_eq!(engine.a1.trades, 0);
        assert_eq!(engine.a5.delta_usdt, 13.0);
        assert_eq!(engine.a1h.trades, 2);
        let mut boundary = SymbolState::new();
        boundary.restore_minutes(&[crate::tests::bar(3599, 120.0)], 3599);
        assert_eq!(boundary.a5.trades, 0);
        assert_eq!(boundary.a1h.trades, 0);
    }
}
