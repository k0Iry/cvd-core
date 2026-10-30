//! Lane-B test-only exporter: PUBLIC synthetic history serialized by the
//! actual producer types (`BarRow` -> `Bar` + serde), consumed by the browser
//! data gate. No database, network or private data. Writes only when
//! `CVD_PRODUCER_FIXTURE_OUT` is set.
use crate::models::{Bar, BarRow};

pub(crate) fn synthetic_public_bars(n: usize) -> Vec<Bar> {
    let mut cvd = 0.0f64;
    (0..n)
        .map(|i| {
            let p = 30_000.0 + 150.0 * (i as f64 / 211.0).sin() + 20.0 * (i as f64 / 13.0).cos();
            let buy = 4_000.0 + 900.0 * (i as f64 / 7.0).sin().abs();
            // Genuine-zero delta rows keep buys/sells reconciled (buy == sell).
            let sell = if i % 101 == 0 { buy } else { 4_000.0 + 700.0 * (i as f64 / 5.0).cos().abs() };
            let delta = buy - sell;
            cvd += delta;
            let time = 1_700_000_040 + 60 * i as i64 + if i >= 1500 { 300 } else { 0 }; // public gap
            Bar::from(BarRow {
                time,
                price_open: p - 3.0,
                price_high: p + 8.0,
                price_low: p - 9.0,
                price_close: p,
                delta_usdt: delta,
                cvd_usdt: cvd,
                buy_volume_base: buy / p,
                sell_volume_base: sell / p,
                buy_volume_usdt: buy,
                sell_volume_usdt: sell,
                trades: 7 + (i % 5) as i64,
            })
        })
        .collect()
}

#[test]
fn export_public_producer_fixture() {
    let bars = synthetic_public_bars(2600);
    let json = serde_json::to_string(&bars).unwrap();
    let back: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(back.len(), 2600);
    assert!(back.iter().all(|b| b["cvd_usdt"].is_f64() && b["ac"].is_f64()));
    // Positive fixture invariant: every row reconciles delta = buy - sell
    // exactly, and genuine-zero rows are present.
    let mut zeros = 0;
    for b in &bars {
        assert_eq!(b.delta_usdt.to_bits(), (b.buy_volume_usdt - b.sell_volume_usdt).to_bits());
        if b.delta_usdt == 0.0 { zeros += 1; }
    }
    assert_eq!(zeros, 26);
    if let Ok(path) = std::env::var("CVD_PRODUCER_FIXTURE_OUT") {
        std::fs::write(path, json).unwrap();
    }
}

/// V1/V2 producer->browser evidence: the ACTUAL push_publish /
/// broadcast_live_payload path on a real shard broadcast channel, serialized
/// by the same serde the WS handler uses. Exports the exact frame sequence
/// (valid, invalidate, repair) for the browser validity suite to check its
/// frame shape against. Writes only when `CVD_PRODUCER_VALIDITY_FRAMES` is set.
#[tokio::test]
async fn export_producer_validity_frames() {
    use crate::types::{Exchange, Market, Symbol, Tf};
    let state = crate::tests::state().await;
    let sh = crate::pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
    let mut rx = sh.tx_1m.subscribe();
    let bars = synthetic_public_bars(3);
    let (a, b) = (bars[0].clone(), bars[1].clone());
    crate::push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, a.clone()).await;
    crate::push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, b.clone()).await;
    let mut bad = b.clone();
    bad.price_close = f64::NAN;
    crate::push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, bad).await;
    crate::push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, b.clone()).await;
    let mut frames = Vec::new();
    while let Ok(f) = rx.try_recv() {
        frames.push(serde_json::to_value(&f).unwrap());
    }
    let actions: Vec<_> = frames.iter().map(|f| f["action"].as_str().unwrap().to_string()).collect();
    assert_eq!(actions, ["append", "append", "invalidate", "append"]);
    assert_eq!(frames[2], serde_json::json!({"rev": frames[1]["rev"].as_u64().unwrap() + 1, "series": "spot-v1", "action": "invalidate", "invalid_time": b.time, "epoch": crate::producer_epoch()}));
    assert!(frames[3]["rev"].as_u64() > frames[2]["rev"].as_u64() && frames[3]["bar"]["time"] == b.time);
    if let Ok(path) = std::env::var("CVD_PRODUCER_VALIDITY_FRAMES") {
        std::fs::write(path, serde_json::to_string_pretty(&frames).unwrap()).unwrap();
    }
}
