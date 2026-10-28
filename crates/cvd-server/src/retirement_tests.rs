use super::*;
use std::future::IntoFuture;

#[tokio::test]
async fn live_http_contract_rejects_retired_selection_and_preserves_active_socket() {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
    let state = Arc::new(tests::state().await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state)).into_future());
    let client = market_http_client().unwrap();
    for query in [
        "exchange=bybit",
        "exchange=BYBIT",
        "exchange=unknown",
        "exchange=coinbase&market=coinm",
        "symbol=SOL",
        "tf=bad",
        "market=bad",
    ] {
        let response = client.get(format!("http://{addr}/history?{query}")).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST, "{query}");
        let request = format!("ws://{addr}/ws?{query}").into_client_request().unwrap();
        match tokio_tungstenite::connect_async(request).await {
            Err(Error::Http(response)) => assert_eq!(response.status(), 400, "{query}"),
            _ => panic!("invalid WS selection was not rejected: {query}"),
        }
    }
    let response = client.get(format!("http://{addr}/meta")).send().await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-cache");
    assert_eq!(response.json::<serde_json::Value>().await.unwrap()["exchanges"].as_array().unwrap().len(), 2);
    let (mut ws, response) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws?exchange=coinbase&market=usdc&symbol=ETH&tf=1h&series=contracts-drb-v1"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), 101);
    ws.close(None).await.unwrap();
    let h: serde_json::Value = client.get(format!("http://{addr}/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["sources"].as_array().unwrap().len(), 14);
    assert_eq!(h["retired_pending_bars"], 0);
    // Static assets are owned by the frontend tree; check them when present.
    if std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../static/index.html").is_file() {
        let html = client.get(format!("http://{addr}/")).send().await.unwrap().text().await.unwrap();
        assert!(!html.to_lowercase().contains("bybit"));
        assert!(html.contains("/js/app.js?v="));
    } else {
        eprintln!("static/index.html not present in this tree; frontend assertions skipped");
    }
    // Only the test HTTP accept loop is aborted; it runs no ingestion workers.
    server.abort();
    let _ = server.await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL CVD_RETIREMENT_TEST_URL; uses a temporary bars table"]
async fn postgres_retirement_preserves_sources_and_excludes_legacy_rows() {
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit isolated test URL required");
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test", "use the dedicated disposable test database");
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert!(unix_socket, "only the disposable Unix socket may be used");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    // A connection-local table shadows public.bars. No production schema/data is changed.
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    for exchange in ["binance", "coinbase", "bybit", "unknown"] {
        let magnitude = match exchange {
            "binance" => 1.0,
            "coinbase" => 2.0,
            _ => 10000.0,
        };
        for market in ["spot", "usdm", "usdc"] {
            for tf in ["1m", "5m", "15m", "30m", "1h"] {
                for time in [59_i64, 119, 179] {
                    sqlx::query("INSERT INTO bars VALUES ($1,$2,'BTC',$3,$4,$5,$5,$5,$5,$6,$7,1,1,$5,$5,1)")
                        .bind(exchange)
                        .bind(market)
                        .bind(tf)
                        .bind(time)
                        .bind(if exchange == "coinbase" && market == "usdm" {
                            1_000_000_000.0
                        } else {
                            magnitude * 100.0
                        })
                        .bind(if exchange == "coinbase" && market == "usdm" { 1_000_000_000.0 } else { magnitude })
                        .bind(if exchange == "coinbase" && market == "usdm" {
                            1_000_000_000.0
                        } else {
                            magnitude * time as f64
                        })
                        .execute(&db)
                        .await
                        .unwrap();
                }
            }
        }
    }
    sqlx::query("INSERT INTO bars SELECT exchange, 'coinm', symbol, tf, time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades FROM bars WHERE exchange='binance' AND market='spot'").execute(&db).await.unwrap();
    // An unsupported pair on an active exchange must not enter contract aggregates.
    sqlx::query("INSERT INTO bars SELECT exchange, 'coinm', symbol, tf, time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades FROM bars WHERE exchange='coinbase' AND market='spot'").execute(&db).await.unwrap();
    // Additional legacy-only minute beyond the active boundary must not seed Binance.
    sqlx::query("UPDATE bars SET time = 239 WHERE exchange = 'bybit' AND time = 179 AND tf='1m'")
        .execute(&db)
        .await
        .unwrap();
    let before: String = sqlx::query_scalar("SELECT json_agg(b ORDER BY exchange,market,tf,time)::text FROM bars b")
        .fetch_one(&db)
        .await
        .unwrap();
    for category in [MarketCategory::Spot, MarketCategory::Contract] {
        let (multiple, venues) = if category == MarketCategory::Spot { (3.0, 2.0) } else { (4.0, 3.0) };
        for tf in ["1m", "5m", "15m", "30m", "1h"] {
            let mut bars = vec![tests::bar(119, 119.0)];
            apply_aggregate_cvd_to_bars(&db, &mut bars, "BTC", category, tf).await.unwrap();
            apply_aggregate_flow_to_bars(&db, &mut bars, "BTC", category, tf).await.unwrap();
            assert_eq!(bars[0].aggregate_cvd_usdt, multiple * 119.0, "legacy rows entered CVD");
            assert_eq!(bars[0].aggregate_delta_usdt, multiple);
            assert_eq!(bars[0].aggregate_price_open, multiple * 100.0 / venues);
            assert_eq!(bars[0].aggregate_price_high, 200.0);
            assert_eq!(bars[0].aggregate_price_low, 100.0);
            assert_eq!(bars[0].aggregate_buy_volume_base, venues);
            assert_eq!(bars[0].aggregate_sell_volume_usdt, multiple * 100.0);
            // No new rows at 120: prior seed values must also exclude retired sources.
            let mut seed_only = vec![tests::bar(120, 0.0)];
            apply_aggregate_cvd_to_bars(&db, &mut seed_only, "BTC", category, tf).await.unwrap();
            assert_eq!(seed_only[0].aggregate_cvd_usdt, multiple * 119.0);
        }
    }
    let mut state = tests::state().await;
    state.db = db.clone();
    seed_cvd_closed_from_db(&state).await.unwrap();
    for market in [Market::Spot, Market::Usdm] {
        let sh = pick_shard(&state, Exchange::Binance, market, Symbol::Btc);
        assert_eq!(sh.engine.lock().await.cvd_closed_usdt, 179.0);
        assert_eq!(sh.engine.lock().await.recovery_from_ms, Some(180000));
    }
    let new_source = pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    assert_eq!(new_source.engine.lock().await.cvd_closed_usdt, 358.0);
    assert_eq!(new_source.engine.lock().await.recovery_from_ms, Some(180000));
    assert!(!state.shards.contains_key(&(Exchange::Coinbase, Market::Usdm, Symbol::Btc)));
    for tf in [Tf::M1, Tf::M5, Tf::M15, Tf::M30, Tf::H1] {
        for (before_time, after_time, expected_times) in
            [(None, None, vec![119, 179]), (Some(179), None, vec![59, 119]), (None, Some(59), vec![119, 179])]
        {
            let page = load_history_page(
                &db,
                "coinbase",
                "BTC",
                "usdc",
                tf.as_str(),
                2,
                before_time,
                after_time,
                None,
                true,
                &indicators::IndicatorManager::empty(),
                &[],
            )
            .await
            .unwrap();
            assert_eq!(page.series, CONTRACT_SERIES);
            assert_eq!(page.bars.iter().map(|b| b.time).collect::<Vec<_>>(), expected_times);
            for bar in &page.bars {
                assert_eq!(bar.aggregate_cvd_usdt, 4.0 * bar.time as f64);
                assert_eq!(bar.aggregate_delta_usdt, 4.0);
                assert_eq!(bar.aggregate_buy_volume_base, 3.0);
                assert_eq!(bar.aggregate_price_close, 400.0 / 3.0);
            }
        }
        let empty = load_history_page(
            &db,
            "coinbase",
            "ETH",
            "usdc",
            tf.as_str(),
            2,
            None,
            None,
            None,
            true,
            &indicators::IndicatorManager::empty(),
            &[],
        )
        .await
        .unwrap();
        assert!(empty.bars.is_empty());
        assert_eq!(empty.series, CONTRACT_SERIES);

        let at = 119;
        for (exchange, market, cvd) in [
            (Exchange::Binance, Market::Usdm, 119.0),
            (Exchange::Binance, Market::Coinm, 119.0),
            (Exchange::Coinbase, Market::Usdc, 238.0),
        ] {
            let mut bar = tests::bar(at, cvd);
            bar.delta_usdt = if market == Market::Usdc { 2.0 } else { 1.0 };
            bar.price_open = if market == Market::Usdc { 200.0 } else { 100.0 };
            bar.price_close = bar.price_open;
            bar.price_high = bar.price_open;
            bar.price_low = bar.price_open;
            bar.buy_volume_base = 1.0;
            bar.sell_volume_base = 0.0;
            bar.buy_volume_usdt = bar.price_open;
            bar.sell_volume_usdt = 0.0;
            let shard = pick_shard(&state, exchange, market, Symbol::Btc);
            shard.for_tf(tf).0.write().await.push_back(bar);
        }
        let mut current = tests::bar(at, 238.0);
        current.delta_usdt = 2.0;
        current.price_open = 200.0;
        current.price_close = 200.0;
        current.price_high = 200.0;
        current.price_low = 200.0;
        current.buy_volume_base = 1.0;
        current.sell_volume_base = 0.0;
        current.buy_volume_usdt = 200.0;
        current.sell_volume_usdt = 0.0;
        apply_live_aggregates(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc, tf, &mut current).await;
        assert_eq!(current.aggregate_cvd_usdt, 476.0);
        assert_eq!(current.aggregate_delta_usdt, 4.0);
        assert_eq!(current.aggregate_price_close, 400.0 / 3.0);
    }
    for (before_time, after_time, max_time) in
        [(None, None, None), (Some(180), None, None), (None, Some(59), None), (None, None, Some(120))]
    {
        let page = load_history_page(
            &db,
            "binance",
            "BTC",
            "spot",
            "1m",
            2,
            before_time,
            after_time,
            max_time,
            max_time.is_none(),
            &indicators::IndicatorManager::empty(),
            &[],
        )
        .await
        .unwrap();
        assert!(!page.bars.is_empty());
        for bar in page.bars {
            assert_eq!(bar.aggregate_cvd_usdt, 3.0 * bar.time as f64);
        }
    }
    hydrate_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).await.unwrap();
    assert_eq!(
        pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc)
            .hist_1m
            .read()
            .await
            .back()
            .unwrap()
            .aggregate_cvd_usdt,
        537.0
    );
    let after: String = sqlx::query_scalar("SELECT json_agg(b ORDER BY exchange,market,tf,time)::text FROM bars b")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(before, after, "retirement must not rewrite any persisted history");
    for exchange in ["binance", "coinbase", "bybit"] {
        sqlx::query("INSERT INTO bars SELECT $1,'spot','ETH','1m',g*60+59,100,101,99,100,1,g+1,1,0,100,0,1 FROM generate_series(0,59) g")
            .bind(exchange).execute(&db).await.unwrap();
    }
    for market in ["usdm", "usdc"] {
        sqlx::query("INSERT INTO bars SELECT 'coinbase',$1,'ETH','1m',g*60+59,100,101,99,100,1,g+1,1,0,100,0,1 FROM generate_series(0,59) g")
            .bind(market).execute(&db).await.unwrap();
    }
    let old_hourly_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM bars WHERE exchange='coinbase' AND market='usdm' AND symbol='ETH' AND tf='1h'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(old_hourly_before, 0);
    sqlx::raw_sql(include_str!("../scripts/backfill_1h_from_1m.sql")).execute(&db).await.unwrap();
    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT exchange || ':' || market FROM bars WHERE symbol='ETH' AND tf='1h' ORDER BY exchange,market",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(sources, vec!["binance:spot", "coinbase:spot", "coinbase:usdc"]);
    let legacy_hourly_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM bars WHERE exchange='coinbase' AND market='usdm' AND symbol='ETH' AND tf='1h'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(legacy_hourly_after, old_hourly_before);
    let retired_minutes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bars WHERE exchange='bybit' AND symbol='ETH'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(retired_minutes, 60);
    let mut api_state = tests::state().await;
    api_state.db = db.clone();
    let api_state = Arc::new(api_state);
    for (market, series, expected) in [("usdc", Some(CONTRACT_SERIES), CONTRACT_SERIES), ("spot", None, SPOT_SERIES)] {
        let params = Params {
            exchange: Some("coinbase".into()),
            market: Some(market.into()),
            series: series.map(str::to_owned),
            symbol: Some("BTC".into()),
            tf: Some("1m".into()),
            key: None,
            limit: Some(2),
            before: None,
            after: None,
            indicators: None,
        };
        let response = history(State(api_state.clone()), Query(params)).await.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["series"], expected);
        assert_eq!(payload["bars"].as_array().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn retired_sources_are_absent_from_shards_and_metadata() {
    let state = tests::state().await;
    assert_eq!(state.shards.len(), 14);
    assert!(state.shards.keys().all(|(exchange, _, _)| exchange.as_str() != "bybit"));
    let response = meta(State(Arc::new(tests::state().await))).await.into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["exchanges"].as_array().unwrap().len(), 2);
    assert!(!String::from_utf8(bytes.to_vec()).unwrap().contains("bybit"));
}

#[tokio::test]
async fn retired_history_is_rejected_before_database_access() {
    let mut state = tests::state().await;
    state.db = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://localhost/cvd_retirement_test")
        .unwrap();
    let state = Arc::new(state);
    for name in ["bybit", "BYBIT", "unknown"] {
        let params = serde_json::from_value(serde_json::json!({"exchange": name})).unwrap();
        let response = history(State(state.clone()), Query(params)).await.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST, "{name}");
    }
}
