//! Observations of aggregate participation, never completeness attestations.
use crate::{get_market_symbol_pairs, models::Bar, AppState, Exchange, Market, MarketCategory, Symbol};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Contribution {
    SameBucket,
    Carried,
    Missing,
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Unknown,
    Starting,
    Live,
    Recovering,
    NeedsBackfill,
    Unavailable,
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BarState {
    Forming,
    Elapsed,
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Unverified,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceQuality {
    pub exchange: String,
    pub market: String,
    pub bar_time: Option<i64>,
    pub contribution: Contribution,
    pub recovery: SourceState,
    pub last_trade_ms: Option<i64>,
}
#[derive(Debug, Clone, Serialize)]
pub struct BarQuality {
    pub observed_at_ms: i64,
    pub status_observed_at_ms: Option<i64>,
    pub bar_state: BarState,
    pub coverage: Coverage,
    pub expected_sources: usize,
    pub present_sources: usize,
    pub sources: Vec<SourceQuality>,
}

impl BarQuality {
    pub(crate) fn observe(
        symbol: Symbol,
        category: MarketCategory,
        bar_time: i64,
        observations: Vec<(Exchange, Market, Option<i64>)>,
        observed_at_ms: i64,
    ) -> Self {
        let mut entries = BTreeMap::new();
        for exchange in Exchange::ALL {
            for (market, s) in get_market_symbol_pairs(exchange) {
                if s == symbol && category.matches_market(market) {
                    entries.insert((exchange.as_str().to_string(), market.as_str().to_string()), None);
                }
            }
        }
        for (exchange, market, time) in observations {
            entries.insert((exchange.as_str().to_string(), market.as_str().to_string()), time);
        }
        let sources: Vec<_> = entries
            .into_iter()
            .map(|((exchange, market), time)| SourceQuality {
                exchange,
                market,
                bar_time: time,
                contribution: match time {
                    Some(t) if t == bar_time => Contribution::SameBucket,
                    Some(_) => Contribution::Carried,
                    None => Contribution::Missing,
                },
                recovery: SourceState::Unknown,
                last_trade_ms: None,
            })
            .collect();
        Self {
            observed_at_ms,
            status_observed_at_ms: None,
            bar_state: if observed_at_ms.div_euclid(1000) <= bar_time { BarState::Forming } else { BarState::Elapsed },
            coverage: Coverage::Unverified,
            expected_sources: sources.len(),
            present_sources: sources.iter().filter(|s| s.contribution == Contribution::SameBucket).count(),
            sources,
        }
    }
}

/// Best-effort delivery-time telemetry is deliberately separate from bar inputs.
/// Never call this to add present-day health to a historical row with no evidence.
pub async fn enrich_delivery(state: &AppState, symbol: Symbol, bar: &mut Bar) {
    let Some(quality) = bar.quality.as_mut() else {
        return;
    };
    for source in &mut quality.sources {
        let Some(exchange) = Exchange::from_str(&source.exchange) else {
            continue;
        };
        let market = Market::from_str(&source.market);
        if market.as_str() != source.market {
            continue;
        }
        let Some(shard) = state.shards.get(&(exchange, market, symbol)) else {
            continue;
        };
        source.recovery = shard
            .recovery
            .try_read()
            .ok()
            .map(|r| match r.state.as_str() {
                "live" => SourceState::Live,
                "starting" => SourceState::Starting,
                "recovering" => SourceState::Recovering,
                "needs_backfill" => SourceState::NeedsBackfill,
                _ => SourceState::Unknown,
            })
            .unwrap_or(SourceState::Unknown);
        match shard.stream_error.try_read() {
            Ok(error)
                if error.is_some()
                    && source.recovery != SourceState::NeedsBackfill
                    && source.recovery != SourceState::Recovering =>
            {
                source.recovery = SourceState::Unavailable
            }
            Err(_) => source.recovery = SourceState::Unknown,
            _ => {}
        }
        source.last_trade_ms = shard.engine.try_lock().ok().and_then(|e| e.last_trade_ms);
    }
    quality.status_observed_at_ms = Some(chrono::Utc::now().timestamp_millis());
}
