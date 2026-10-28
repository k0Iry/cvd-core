use super::*;

#[test]
fn expected_scope_and_time_boundary_are_not_completeness() {
    use quality::{BarQuality, BarState, Coverage};
    let forming = BarQuality::observe(
        Symbol::Btc,
        MarketCategory::Contract,
        119,
        vec![(Exchange::Binance, Market::Usdm, Some(119))],
        119999,
    );
    assert_eq!(forming.expected_sources, 4);
    assert_eq!(forming.present_sources, 1);
    assert_eq!(forming.bar_state, BarState::Forming);
    assert_eq!(forming.coverage, Coverage::Unverified);
    assert!(forming.sources.iter().all(|s| s.market != "spot"));
    let elapsed = BarQuality::observe(
        Symbol::Btc,
        MarketCategory::Spot,
        119,
        vec![(Exchange::Binance, Market::Spot, Some(119)), (Exchange::Coinbase, Market::Spot, Some(119))],
        120000,
    );
    assert_eq!(elapsed.expected_sources, 3);
    assert_eq!(elapsed.present_sources, 2);
    assert!(elapsed
        .sources
        .iter()
        .any(|s| s.market == "spot_usdc" && s.contribution == quality::Contribution::Missing));
    assert_eq!(elapsed.bar_state, BarState::Elapsed);
    assert_eq!(elapsed.coverage, Coverage::Unverified);
}

#[tokio::test]
async fn late_contribution_changes_participation_not_coverage_claim() {
    let state = tests::state().await;
    let other = pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let mut current = tests::bar(119, 10.0);
    apply_live_aggregates(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mut current).await;
    assert_eq!(current.quality.as_ref().unwrap().present_sources, 1);
    other.hist_1m.write().await.push_back(tests::bar(119, 25.0));
    apply_live_aggregates(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mut current).await;
    assert_eq!(current.quality.as_ref().unwrap().present_sources, 2);
    assert_eq!(current.aggregate_cvd_usdt, 35.0);
    assert_eq!(current.quality.as_ref().unwrap().coverage, quality::Coverage::Unverified);
    let row = serde_json::to_value(models::BarRow::from(&current)).unwrap();
    assert!(row.get("quality").is_none() && row.get("iv").is_none() && row.get("aiv").is_none());
    let mut historical = Bar::from(models::BarRow::from(&current));
    quality::enrich_delivery(&state, Symbol::Btc, &mut historical).await;
    assert!(historical.quality.is_none());
}

#[tokio::test]
async fn delivery_status_is_best_effort_and_does_not_expose_errors() {
    let state = tests::state().await;
    let other = pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    other.recovery.write().await.state = "live".into();
    *other.stream_error.write().await = Some("internal-debug-detail-not-for-chart".into());
    let mut current = tests::bar(119, 10.0);
    apply_live_aggregates(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mut current).await;
    let engine_guard = other.engine.lock().await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        quality::enrich_delivery(&state, Symbol::Btc, &mut current),
    )
    .await
    .unwrap();
    let source = current.quality.as_ref().unwrap().sources.iter().find(|s| s.exchange == "coinbase").unwrap();
    assert_eq!(source.recovery, quality::SourceState::Unavailable);
    assert!(source.last_trade_ms.is_none());
    assert!(!serde_json::to_string(&current).unwrap().contains("internal-debug-detail"));
    drop(engine_guard);
    other.recovery.write().await.state = "recovering".into();
    quality::enrich_delivery(&state, Symbol::Btc, &mut current).await;
    assert_eq!(
        current.quality.as_ref().unwrap().sources.iter().find(|s| s.exchange == "coinbase").unwrap().recovery,
        quality::SourceState::Recovering
    );
    let recovery_guard = other.recovery.write().await;
    quality::enrich_delivery(&state, Symbol::Btc, &mut current).await;
    // Error is independently known, so unavailable is still a truthful outcome.
    assert_ne!(
        current.quality.as_ref().unwrap().sources.iter().find(|s| s.exchange == "coinbase").unwrap().recovery,
        quality::SourceState::Live
    );
    drop(recovery_guard);
}

#[tokio::test]
async fn actual_websocket_route_serializes_delivery_quality() {
    use futures::StreamExt;
    let state = Arc::new(tests::state().await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = app_router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/ws?exchange=binance&market=spot&symbol=BTC&tf=1m"))
            .await
            .unwrap();
    let primary = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
    let other = pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    other.recovery.write().await.state = "recovering".into();
    let mut current = tests::bar(119, 10.0);
    apply_live_aggregates(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mut current).await;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while primary.tx_1m.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    primary
        .tx_1m
        .send(models::WsPayload { rev: 0, series: SPOT_SERIES, action: models::WsAction::Append, bar: current })
        .unwrap();
    let message = tokio::time::Duration::from_secs(2);
    let response = tokio::time::timeout(message, socket.next()).await.unwrap().unwrap().unwrap();
    let payload: serde_json::Value = serde_json::from_str(response.to_text().unwrap()).unwrap();
    assert_eq!(payload["bar"]["quality"]["coverage"], "unverified");
    assert!(payload["bar"]["quality"]["status_observed_at_ms"].as_i64().unwrap() > 0);
    assert_eq!(
        payload["bar"]["quality"]["sources"].as_array().unwrap().iter().find(|s| s["exchange"] == "coinbase").unwrap()
            ["recovery"],
        "recovering"
    );
    socket.close(None).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn live_aggregate_reports_participation_without_claiming_completeness() {
    let state = tests::state().await;
    let other = pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    other.hist_1m.write().await.push_back(tests::bar(59, 20.0));
    let mut current = tests::bar(119, 10.0);
    apply_live_aggregates(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mut current).await;
    let encoded = serde_json::to_value(&current).unwrap();
    let q = &encoded["quality"];
    assert_eq!(q["coverage"], "unverified");
    assert_eq!(q["bar_state"], "elapsed");
    assert_eq!(q["expected_sources"], 3);
    assert_eq!(q["present_sources"], 1);
    let sources = q["sources"].as_array().unwrap();
    assert_eq!(sources.iter().find(|s| s["exchange"] == "coinbase").unwrap()["contribution"], "carried");
    assert_eq!(current.aggregate_cvd_usdt, 30.0);
}

#[test]
fn historical_rows_never_inherit_current_source_health() {
    let bar = tests::bar(119, 10.0);
    let rebuilt = Bar::from(models::BarRow::from(&bar));
    let encoded = serde_json::to_value(rebuilt).unwrap();
    assert!(encoded.get("quality").is_none_or(serde_json::Value::is_null));
}
