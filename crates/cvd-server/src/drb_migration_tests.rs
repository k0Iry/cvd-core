use super::*;

#[test]
fn new_derivatives_have_an_independent_active_storage_key() {
    let keys = active_market_keys();
    assert!(keys.contains(&"coinbase:usdc".to_string()));
    assert!(!keys.contains(&"coinbase:usdm".to_string()));
    assert!(keys.contains(&"binance:usdm".to_string()));
    assert!(keys.contains(&"coinbase:spot".to_string()));
}

#[test]
fn retired_intx_requests_are_not_silently_remapped_to_new_data() {
    let mut p = Params {
        exchange: Some("coinbase".into()),
        market: Some("usdm".into()),
        series: Some(CONTRACT_SERIES.into()),
        symbol: Some("BTC".into()),
        tf: Some("1m".into()),
        key: None,
        limit: None,
        before: None,
        after: None,
        indicators: None,
    };
    assert!(request_selection(&p).is_err(), "legacy namespace must not mean the new product");
    p.market = Some("usdc".into());
    let (exchange, market, _, _) = request_selection(&p).unwrap();
    assert_eq!(exchange, Exchange::Coinbase);
    assert_eq!(market.as_str(), "usdc");
    p.exchange = Some("binance".into());
    assert!(request_selection(&p).is_err(), "USDC gateway is not a new Binance stream");
}

#[test]
fn old_and_new_pending_payloads_keep_distinct_market_keys() {
    let bar = tests::bar(119, 1.0);
    let old = persistence::PendingBar::new(Exchange::Coinbase, Market::Usdm, Symbol::Btc, Tf::M1, &bar);
    let new = persistence::PendingBar::new(Exchange::Coinbase, Market::from_str("usdc"), Symbol::Btc, Tf::M1, &bar);
    let old = serde_json::to_value(old).unwrap();
    let new = serde_json::to_value(new).unwrap();
    assert_eq!(old["market"], "usdm");
    assert_eq!(new["market"], "usdc");
    assert_eq!(old["bar"], new["bar"], "namespace migration must not rewrite raw bar values");
}

#[test]
fn contract_series_fence_is_exact_and_spot_is_compatible() {
    let mut p = Params {
        exchange: Some("coinbase".into()),
        market: Some("usdm".into()),
        series: None,
        symbol: Some("BTC".into()),
        tf: Some("1m".into()),
        key: None,
        limit: None,
        before: None,
        after: None,
        indicators: None,
    };
    assert_eq!(selection_response(&p).unwrap_err().0, axum::http::StatusCode::GONE);
    p.market = Some("USDM".into());
    assert_eq!(selection_response(&p).unwrap_err().0, axum::http::StatusCode::GONE);
    p.market = Some("usdc".into());
    assert_eq!(selection_response(&p).unwrap_err().0, axum::http::StatusCode::CONFLICT);
    p.series = Some("contracts-old-v1".into());
    assert_eq!(selection_response(&p).unwrap_err().0, axum::http::StatusCode::CONFLICT);
    p.series = Some(CONTRACT_SERIES.into());
    assert_eq!(selection_response(&p).unwrap().1, Market::Usdc);
    p.exchange = Some("binance".into());
    p.market = Some("usdm".into());
    assert_eq!(selection_response(&p).unwrap().1, Market::Usdm);
    p.market = Some("spot".into());
    p.series = None;
    assert_eq!(selection_response(&p).unwrap().1, Market::Spot);
    p.series = Some(SPOT_SERIES.into());
    assert_eq!(selection_response(&p).unwrap().1, Market::Spot);
    for wrong in [CONTRACT_SERIES, "", "unrecognized"] {
        p.series = Some(wrong.into());
        assert_eq!(selection_response(&p).unwrap_err().0, axum::http::StatusCode::CONFLICT);
    }
    p.series = None;
    p.market = Some("nonsense".into());
    assert_eq!(selection_response(&p).unwrap_err().0, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_and_authorized_ws_fence_before_database_and_advertise_source() {
    use axum::response::IntoResponse;
    use std::future::IntoFuture;
    use tokio_tungstenite::tungstenite::Error;
    let mut state = tests::state().await;
    state.db = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(40))
        .connect_lazy("postgres://localhost/cvd_retirement_test")
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(state);
    let server = tokio::spawn(axum::serve(listener, app_router(state.clone())).into_future());
    let http = market_http_client().unwrap();
    for (market, series, expected) in [
        ("usdm", "", 410),
        ("usdm", "&series=contracts-drb-v1", 410),
        ("usdc", "", 409),
        ("usdc", "&series=spot-v1", 409),
        ("spot", "&series=contracts-drb-v1", 409),
        ("spot", "&series=unknown", 409),
        ("spot", "&series=", 409),
        ("usdc", "&series=contracts-old-v1", 409),
    ] {
        let query = format!("exchange=coinbase&market={market}&symbol=BTC&tf=1m{series}");
        assert_eq!(
            http.get(format!("http://{addr}/history?{query}")).send().await.unwrap().status().as_u16(),
            expected
        );
        match tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{query}")).await {
            Err(Error::Http(response)) => assert_eq!(response.status().as_u16(), expected),
            other => panic!("expected WS {expected}: {other:?}"),
        }
    }
    let (mut socket, response) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws?exchange=coinbase&market=usdc&symbol=BTC&tf=1h&series=contracts-drb-v1"
    ))
    .await
    .unwrap();
    assert_eq!(response.status().as_u16(), 101);
    let shard = pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    tokio::time::timeout(Duration::from_secs(1), async {
        while shard.tx_1h.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    shard
        .tx_1h
        .send(WsPayload::bar(0, CONTRACT_SERIES, WsAction::Append, tests::bar(119, 1.0)))
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), socket.next()).await.unwrap().unwrap().unwrap();
    let payload: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(payload["series"], CONTRACT_SERIES);
    socket.close(None).await.unwrap();
    let response = meta().await.into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(meta["contract_series"], CONTRACT_SERIES);
    assert_eq!(meta["spot_series"], SPOT_SERIES);
    assert!(meta["coinbase_contract_source"].as_str().unwrap().contains("independent"));
    let coinbase = meta["exchanges"].as_array().unwrap().iter().find(|v| v["exchange"] == "coinbase").unwrap();
    assert_eq!(coinbase["markets"], serde_json::json!(["spot", "usdc"]));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn all_live_timeframes_emit_matching_source_series() {
    let state = tests::state().await;
    for market in [Market::Spot, Market::Usdc] {
        for tf in [Tf::M1, Tf::M5, Tf::M15, Tf::M30, Tf::H1] {
            let shard = pick_shard(&state, Exchange::Coinbase, market, Symbol::Btc);
            let mut rx = shard.for_tf(tf).1.subscribe();
            let bar = tests::bar(119, 1.0);
            broadcast_live_payload(&state, Exchange::Coinbase, market, Symbol::Btc, tf, bar).await;
            let payload = rx.try_recv().unwrap();
            assert_eq!(payload.series, series_for_market(market));
            assert_eq!(serde_json::to_value(&payload).unwrap()["series"], series_for_market(market));
        }
    }
}

#[tokio::test]
async fn retired_outbox_moves_only_exact_coinbase_usdm_preserving_original_bytes_and_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pending.sqlite");
    let outbox = persistence::Outbox::open(&path).await.unwrap();
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display())).await.unwrap();
    let mut original = Vec::new();
    for (exchange, market) in
        [("coinbase", "usdm"), ("binance", "usdm"), ("coinbase", "spot"), ("coinbase", "usdc"), ("bybit", "spot")]
    {
        let payload = format!("{{ \"market\":\"{market}\",\"exchange\":\"{exchange}\",\"raw\":  1.2300 }}");
        let id: i64 = sqlx::query_scalar("INSERT INTO pending_bars(payload) VALUES (?) RETURNING id")
            .bind(&payload)
            .fetch_one(&pool)
            .await
            .unwrap();
        original.push((id, payload));
    }
    pool.close().await;
    drop(outbox);
    for _ in 0..2 {
        let reopened = persistence::Outbox::open(&path).await.unwrap();
        assert_eq!(reopened.pending_count().await.unwrap(), 3);
        assert_eq!(reopened.retired_count().await.unwrap(), 2);
        drop(reopened);
    }
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display())).await.unwrap();
    let retired: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,payload FROM retired_pending_bars ORDER BY id").fetch_all(&pool).await.unwrap();
    let pending: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,payload FROM pending_bars ORDER BY id").fetch_all(&pool).await.unwrap();
    assert_eq!(retired, vec![original[0].clone(), original[4].clone()]);
    assert_eq!(pending, original[1..4]);
    pool.close().await;
}
