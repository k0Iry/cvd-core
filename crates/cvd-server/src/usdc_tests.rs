use super::*;
use crate::ingestion::{ingest_trade, Trade};

fn trade(id: &str, time: i64, quote: f64) -> Trade {
    Trade { id: id.into(), timestamp_ms: time, price: 100.0, base: quote / 100.0, quote, is_buy: true }
}

#[test]
fn descriptor_routing_is_exact_and_rejects_cross_venue_or_wrong_stream() {
    assert_eq!(types::INSTRUMENTS.len(), 14);
    for i in types::INSTRUMENTS {
        assert_eq!(types::instrument(i.exchange, i.market, i.symbol).unwrap().native_symbol, i.native_symbol);
        assert_eq!(i.category == "spot", i.market.is_spot());
        assert_eq!(
            i.market.quantity_semantics() == types::QuantitySemantics::InverseContracts,
            i.contract_kind == Some("inverse")
        );
        if i.exchange == Exchange::Binance {
            let url = binance_market_stream_url(i.market).unwrap();
            assert!(url.contains(&format!("{}@aggTrade", i.native_symbol.to_lowercase())));
            let (rest, product) = recovery::binance_replay_route(i.market, i.symbol).unwrap();
            assert_eq!(product, i.native_symbol);
            assert_eq!(
                rest,
                match i.market {
                    Market::Spot | Market::SpotUsdc => "https://api.binance.com/api/v3/aggTrades",
                    Market::Usdm | Market::UsdmUsdc => "https://fapi.binance.com/fapi/v1/aggTrades",
                    Market::Coinm => "https://dapi.binance.com/dapi/v1/aggTrades",
                    _ => unreachable!(),
                }
            );
            assert_eq!(
                Symbol::from_binance_stream_name(&format!("{}@aggTrade", i.native_symbol.to_lowercase()), i.market),
                Some(i.symbol)
            );
            assert_eq!(
                Symbol::from_binance_stream_name(&format!("{}@trade", i.native_symbol.to_lowercase()), i.market),
                None
            );
        }
    }
    assert!(binance_market_stream_url(Market::Usdc).is_err());
    assert!(recovery::binance_replay_route(Market::Usdc, Symbol::Btc).is_err());
    assert_eq!(Symbol::from_binance_stream_name("btcusdt@aggTrade", Market::SpotUsdc), None);
    assert_eq!(Symbol::from_binance_stream_name("btcusdc@aggTrade", Market::Spot), None);
    assert_eq!(Symbol::from_binance_stream_name("btcusdc@aggTrade", Market::Usdm), None);
    assert_eq!(Symbol::from_binance_stream_name("solusdc@aggTrade", Market::UsdmUsdc), None);
    assert_eq!(Symbol::from_binance_stream_name("btcusdc@aggTrade/extra", Market::SpotUsdc), None);
    assert_eq!(get_market_symbol_pairs(Exchange::Binance).len(), 10);
    assert_eq!(get_market_symbol_pairs(Exchange::Coinbase).len(), 4);
}

#[test]
fn quote_and_perpetual_metadata_must_match_native_product() {
    let good = serde_json::from_str::<binance::LinearInfo>(r#"{"symbols":[{"symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC","marginAsset":"USDC","contractType":"PERPETUAL","status":"TRADING"},{"symbol":"ETHUSDC","baseAsset":"ETH","quoteAsset":"USDC","marginAsset":"USDC","contractType":"PERPETUAL","status":"TRADING"}]}"#).unwrap();
    assert!(binance::validate_products(&good, Market::UsdmUsdc).is_ok());
    assert!(binance::validate_products(&good, Market::Usdm).is_err());
    let mut broken = good;
    broken.symbols[0].margin = Some("USDT".into());
    assert!(binance::validate_products(&broken, Market::UsdmUsdc).is_err());
    broken.symbols[0].margin = Some("USDC".into());
    broken.symbols[0].contract_type = Some("CURRENT_QUARTER".into());
    assert!(binance::validate_products(&broken, Market::UsdmUsdc).is_err());
    broken.symbols[0].contract_type = Some("PERPETUAL".into());
    broken.symbols[0].status = "BREAK".into();
    assert!(binance::validate_products(&broken, Market::UsdmUsdc).is_err());
    let spot = serde_json::from_str::<binance::LinearInfo>(r#"{"symbols":[{"symbol":"BTCUSDC","baseAsset":"BTC","quoteAsset":"USDC","status":"TRADING","isSpotTradingAllowed":true},{"symbol":"ETHUSDC","baseAsset":"ETH","quoteAsset":"USDC","status":"TRADING","isSpotTradingAllowed":true}]}"#).unwrap();
    assert!(binance::validate_products(&spot, Market::SpotUsdc).is_ok());
    assert!(binance::validate_products(&spot, Market::Spot).is_err());
}

#[tokio::test]
async fn colliding_ids_have_separate_cursors_and_outbox_payloads() {
    let state = tests::state().await;
    for market in [Market::Spot, Market::SpotUsdc, Market::Usdm, Market::UsdmUsdc] {
        ingest_trade(&state, Exchange::Binance, market, Symbol::Btc, trade("42", 60_001, 100.0)).await.unwrap();
        assert_eq!(
            pick_shard(&state, Exchange::Binance, market, Symbol::Btc).engine.lock().await.last_trade_id.as_deref(),
            Some("42")
        );
    }
    for market in [Market::Spot, Market::SpotUsdc, Market::Usdm, Market::UsdmUsdc] {
        ingest_trade(&state, Exchange::Binance, market, Symbol::Btc, trade("43", 120_001, 200.0)).await.unwrap();
        ingest_trade(&state, Exchange::Binance, market, Symbol::Btc, trade("44", 180_001, 300.0)).await.unwrap();
    }
    let usdt = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).engine.lock().await;
    assert_eq!(usdt.last_trade_id.as_deref(), Some("44"));
    drop(usdt);
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT payload FROM pending_bars ORDER BY id").fetch_all(&state.outbox.pool).await.unwrap();
    let parsed: Vec<serde_json::Value> = rows.iter().map(|r| serde_json::from_str(&r.0).unwrap()).collect();
    for market in ["spot_usdc", "usdm_usdc"] {
        assert!(parsed.iter().any(|r| r["market"] == market));
    }
    assert_eq!(parsed.iter().filter(|r| r["market"] == "spot" && r["bar"]["cvd_usdt"] == 100.0).count(), 1);
    assert_eq!(parsed.iter().filter(|r| r["market"] == "spot_usdc" && r["bar"]["cvd_usdt"] == 100.0).count(), 1);
}

#[tokio::test]
async fn http_ws_selection_and_meta_are_additive() {
    use std::future::IntoFuture;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
    let state = Arc::new(tests::state().await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state)).into_future());
    let client = market_http_client().unwrap();
    let meta: serde_json::Value = client.get(format!("http://{addr}/meta")).send().await.unwrap().json().await.unwrap();
    if let Ok(path) = std::env::var("CVD_META_EXPORT_PATH") {
        // Test-only export of the ACTUAL serialized /meta response for contract comparison.
        std::fs::write(&path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    }
    assert_eq!(meta["instruments"].as_array().unwrap().len(), 14);
    let source = meta["instruments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["market"] == "spot_usdc" && i["symbol"] == "BTC")
        .unwrap();
    assert_eq!(
        (source["native_symbol"].as_str(), source["category"].as_str(), source["quote_asset"].as_str()),
        (Some("BTCUSDC"), Some("spot"), Some("USDC"))
    );
    for (market, series) in [("spot_usdc", SPOT_SERIES), ("usdm_usdc", CONTRACT_SERIES)] {
        let query = format!("exchange=binance&market={market}&symbol=BTC&tf=1m&series={series}");
        let (mut socket, response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{query}")).await.unwrap();
        assert_eq!(response.status(), 101);
        socket.close(None).await.unwrap();
        for invalid in [
            format!("exchange=coinbase&market={market}&series={series}"),
            format!("exchange=binance&market={market}&series=wrong"),
            format!("exchange=binance&market={market}&symbol=SOL&series={series}"),
        ] {
            let req = format!("ws://{addr}/ws?{invalid}").into_client_request().unwrap();
            assert!(matches!(tokio_tungstenite::connect_async(req).await, Err(Error::Http(_))));
            assert!(!client
                .get(format!("http://{addr}/history?{invalid}"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success());
        }
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
#[ignore = "requires CVD_RETIREMENT_TEST_URL at disposable Unix-socket cvd_retirement_test"]
async fn postgres_usdc_rows_have_independent_seeds_history_and_outbox_delivery() {
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit disposable URL required");
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test");
    assert!(unix_socket, "TCP or non-disposable database forbidden");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let mut state = tests::state().await;
    state.db = db.clone();
    let mut input = Vec::new();
    for (market, cvd) in
        [(Market::Spot, 10.0), (Market::SpotUsdc, 20.0), (Market::Usdm, 30.0), (Market::UsdmUsdc, 40.0)]
    {
        input.push(persistence::PendingBar::new(Exchange::Binance, market, Symbol::Btc, Tf::M1, &tests::bar(119, cvd)));
    }
    state.outbox.enqueue_batch(&input).await.unwrap();
    assert_eq!(state.outbox.pending_count().await.unwrap(), 4);
    state.outbox.drain(&db).await.unwrap();
    assert_eq!(state.outbox.pending_count().await.unwrap(), 0);
    let rows: Vec<(String, f64)> =
        sqlx::query_as("SELECT market, cvd_usdt FROM bars ORDER BY market").fetch_all(&db).await.unwrap();
    assert_eq!(
        rows,
        vec![("spot".into(), 10.0), ("spot_usdc".into(), 20.0), ("usdm".into(), 30.0), ("usdm_usdc".into(), 40.0)]
    );
    seed_cvd_closed_from_db(&state).await.unwrap();
    for (market, cvd) in
        [(Market::Spot, 10.0), (Market::SpotUsdc, 20.0), (Market::Usdm, 30.0), (Market::UsdmUsdc, 40.0)]
    {
        assert_eq!(pick_shard(&state, Exchange::Binance, market, Symbol::Btc).engine.lock().await.cvd_closed_usdt, cvd);
        let page = load_history_page(
            &db,
            "binance",
            "BTC",
            market.as_str(),
            "1m",
            1,
            None,
            None,
            None,
            true,
        )
        .await
        .unwrap();
        assert_eq!(page.bars.len(), 1);
        assert_eq!(page.bars[0].cvd_usdt, cvd);
        assert_eq!(page.series, series_for_market(market));
        let expected = if market.is_spot() { 30.0 } else { 70.0 };
        assert_eq!(page.bars[0].aggregate_cvd_usdt, expected);
        hydrate_shard(&state, Exchange::Binance, market, Symbol::Btc).await.unwrap();
        assert_eq!(
            pick_shard(&state, Exchange::Binance, market, Symbol::Btc)
                .hist_1m
                .read()
                .await
                .back()
                .unwrap()
                .aggregate_cvd_usdt,
            expected
        );
    }
    let empty = load_history_page(
        &db,
        "binance",
        "ETH",
        "spot_usdc",
        "1m",
        10,
        None,
        None,
        None,
        true,
    )
    .await
    .unwrap();
    assert!(empty.bars.is_empty(), "new source has no invented zero history");
}

/// Opt in explicitly; no server startup, no persistent collector and no PostgreSQL connection.
/// A recent REST cursor is deliberately moved back to force real adapter replay
/// before its next WebSocket execution; the outbox is a disposable SQLite file.
#[tokio::test]
#[ignore = "opt-in bounded actual Binance USDC HTTP/WS adapter, temporary SQLite, lazy unused PostgreSQL"]
async fn public_binance_usdc_adapter_replays_and_ingests_both_markets() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = tests::state().await;
    state.outbox = Arc::new(persistence::Outbox::open(&temp.path().join("outbox.sqlite")).await.unwrap());
    for market in [Market::SpotUsdc, Market::UsdmUsdc] {
        tokio::time::timeout(Duration::from_secs(120), async {
            binance::validate_market_metadata(market).await.unwrap();
            let mut targets = Vec::new();
            for symbol in [Symbol::Btc, Symbol::Eth] {
                let (endpoint, product) = recovery::binance_replay_route(market, symbol).unwrap();
                let rows: Vec<models::AggTrade> = state.http.get(endpoint)
                    .query(&[("symbol", product), ("limit", "1")]).send().await.unwrap()
                    .error_for_status().unwrap().json().await.unwrap();
                let row = rows.first().expect("public REST no recent trades").id;
                let cursor = row.checked_sub(10).unwrap();
                let shard = pick_shard(&state, Exchange::Binance, market, symbol);
                { let mut e = shard.engine.lock().await; e.last_trade_id = Some(cursor.to_string()); }
                targets.push((symbol, shard, cursor, row));
            }
            let url = binance_market_stream_url(market).unwrap();
            let (socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            let (write, read) = futures::StreamExt::split(socket);
            let worker = binance::run_binance_market_engine(&state, market, write, read);
            tokio::pin!(worker);
            tokio::select! {
                result = &mut worker => panic!("adapter stopped before ingesting: {result:?}"),
                _ = async {
                    'wait: loop {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        for (_, shard, _, row) in &targets {
                            let e = shard.engine.lock().await;
                            let ok = e.last_trade_id.as_deref().and_then(|id| id.parse::<u64>().ok()).is_some_and(|id| id > *row)
                                && e.sec.trades > 0;
                            if !ok { continue 'wait; }
                        }
                        break;
                    }
                } => {}
            }
            for (symbol, shard, cursor, row) in &targets {
                let e = shard.engine.lock().await;
                assert!(e.last_trade_id.as_deref().unwrap().parse::<u64>().unwrap() > *row);
                assert!(e.cvd_closed_usdt.is_finite());
                assert!(!shard.recovery.read().await.needs_backfill());
                println!("{} {:?} REST cursor {} -> adapter {} (real replay/ingest)", market.as_str(), symbol, cursor, e.last_trade_id.as_deref().unwrap());
            }
        }).await.expect("public Binance adapter exceeded bounded 120s");
    }
}

/// R integration lane: SYNTHETIC bars written through the real outbox into a
/// disposable pg_temp `bars` table, then served by the PRODUCTION `app_router`
/// over real HTTP `/history` and `/ws`. Every response recorded in the export is
/// the actual serialized producer body; nothing in the corpus is hand-built.
#[tokio::test]
#[ignore = "requires CVD_RETIREMENT_TEST_URL at disposable Unix-socket cvd_retirement_test"]
async fn postgres_router_http_ws_isolate_usdt_usdc_sources_across_timeframes() {
    use futures::StreamExt;
    use std::future::IntoFuture;
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit disposable URL required");
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test");
    assert!(unix_socket, "TCP or non-disposable database forbidden");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let mut state = tests::state().await;
    state.db = db.clone();

    const MARKETS: [Market; 4] = [Market::Spot, Market::SpotUsdc, Market::Usdm, Market::UsdmUsdc];
    const SYMBOLS: [Symbol; 2] = [Symbol::Btc, Symbol::Eth];
    const TFS: [Tf; 5] = [Tf::M1, Tf::M5, Tf::M15, Tf::M30, Tf::H1];
    let t0: i64 = 1_700_000_000 - 1_700_000_000 % 3600;
    // Source code 1..=8 identifies symbol+market; it is encoded in every value band.
    let code = |s: usize, m: usize| (s * 4 + m + 1) as f64;
    let mk = |s: usize, m: usize, t: usize, k: usize| {
        let c = code(s, m);
        let p = c * 100.0 + t as f64 + k as f64 * 0.25;
        Bar::from(models::BarRow {
            time: t0 + k as i64 * TFS[t].ms() / 1000,
            price_open: p,
            price_high: p + 0.5,
            price_low: p - 0.5,
            price_close: p + 0.125,
            delta_usdt: c * 5.0, // reconciles: buy_usdt - sell_usdt
            cvd_usdt: c * 1000.0 + t as f64 * 10.0 + k as f64,
            buy_volume_base: c,
            sell_volume_base: c / 2.0,
            buy_volume_usdt: c * 10.0,
            sell_volume_usdt: c * 5.0,
            trades: c as i64,
        })
    };
    let mut input = Vec::new();
    for (s, symbol) in SYMBOLS.iter().enumerate() {
        for (m, market) in MARKETS.iter().enumerate() {
            for (t, tf) in TFS.iter().enumerate() {
                for k in 0..3 {
                    input.push(persistence::PendingBar::new(Exchange::Binance, *market, *symbol, *tf, &mk(s, m, t, k)));
                }
            }
        }
    }
    state.outbox.enqueue_batch(&input).await.unwrap();
    state.outbox.drain(&db).await.unwrap();
    assert_eq!(state.outbox.pending_count().await.unwrap(), 0);
    let snapshot = || async {
        sqlx::query_as::<_, (String, String, String, i64, f64, f64)>(
            "SELECT market, symbol, tf, time, price_close, cvd_usdt FROM bars ORDER BY 1,2,3,4",
        )
        .fetch_all(&db)
        .await
        .unwrap()
    };
    let before_rows = snapshot().await;
    assert_eq!(before_rows.len(), 2 * 4 * 5 * 3);

    seed_cvd_closed_from_db(&state).await.unwrap();
    for symbol in SYMBOLS {
        for market in MARKETS {
            hydrate_shard(&state, Exchange::Binance, market, symbol).await.unwrap();
        }
    }
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state.clone())).into_future());
    let client = market_http_client().unwrap();
    let meta: serde_json::Value = client.get(format!("http://{addr}/meta")).send().await.unwrap().json().await.unwrap();
    // Retired server indicator discovery: no route, no meta/health/history field.
    let retired = client.get(format!("http://{addr}/indicators")).send().await.unwrap();
    assert_eq!(retired.status(), reqwest::StatusCode::NOT_FOUND, "server indicator discovery must be retired");
    assert!(meta.get("indicators").is_none(), "meta must not advertise server indicators");
    let mut history = serde_json::Map::new();
    for (s, symbol) in SYMBOLS.iter().enumerate() {
        for (m, market) in MARKETS.iter().enumerate() {
            for (t, tf) in TFS.iter().enumerate() {
                let series = series_for_market(*market);
                let q = format!(
                    "exchange=binance&market={}&symbol={}&tf={}&series={series}",
                    market.as_str(),
                    symbol.as_str(),
                    tf.as_str()
                );
                let get = |extra: String| {
                    let client = client.clone();
                    let url = format!("http://{addr}/history?{q}{extra}");
                    async move {
                        let r = client.get(url).send().await.unwrap();
                        assert!(r.status().is_success());
                        r.json::<serde_json::Value>().await.unwrap()
                    }
                };
                let full = get(String::new()).await;
                let bars = full["bars"].as_array().unwrap();
                assert_eq!(full["series"], series);
                assert_eq!(bars.len(), 3, "{q}");
                for (k, bar) in bars.iter().enumerate() {
                    let want = mk(s, m, t, k);
                    assert_eq!(bar["time"].as_i64(), Some(want.time));
                    assert_eq!(bar["price_close"].as_f64(), Some(want.price_close), "{q}");
                    assert_eq!(bar["cvd_usdt"].as_f64(), Some(want.cvd_usdt), "{q}");
                    assert_eq!(bar["buy_volume_usdt"].as_f64(), Some(want.buy_volume_usdt), "{q}");
                    // Aggregate category = same symbol/tf/time across both quote sources of the category.
                    let pair = if market.is_spot() { [0, 1] } else { [2, 3] };
                    let agg: f64 = pair.iter().map(|pm| mk(s, *pm, t, k).cvd_usdt).sum();
                    assert_eq!(bar["ac"].as_f64(), Some(agg), "{q}");
                }
                assert!(full.get("indicators").is_none() && full.get("indicator_api_version").is_none(), "{q}");
                assert_eq!(full["invalid_times"], serde_json::json!([]), "{q}: clean synthetic page");
                // Pagination: newest two, then strictly older remainder.
                let p1 = get("&limit=2".into()).await;
                assert_eq!(p1["bars"].as_array().unwrap().len(), 2);
                assert_eq!(p1["has_more_before"], true);
                let oldest = p1["oldest_time"].as_i64().unwrap();
                let p2 = get(format!("&limit=2&before={oldest}")).await;
                assert_eq!(p2["bars"].as_array().unwrap().len(), 1);
                assert_eq!(p2["bars"][0]["time"].as_i64(), Some(t0));
                assert_eq!(p2["bars"][0]["cvd_usdt"].as_f64(), Some(mk(s, m, t, 0).cvd_usdt));
                history.insert(q.clone(), full);
                history.insert(format!("{q}&limit=2"), p1);
                history.insert(format!("{q}&limit=2&before={oldest}"), p2);
            }
        }
    }
    // Honest empty history for an active source with no rows (Coinbase USDC perpetual).
    let empty_q = format!("exchange=coinbase&market=usdc&symbol=BTC&tf=1m&series={CONTRACT_SERIES}");
    let empty: serde_json::Value =
        client.get(format!("http://{addr}/history?{empty_q}")).send().await.unwrap().json().await.unwrap();
    assert!(empty["bars"].as_array().unwrap().is_empty(), "no invented zero history");
    history.insert(empty_q, empty);

    // WS: subscribe every source x tf, then publish distinct trades via real ingestion.
    let mut sockets = Vec::new();
    for symbol in SYMBOLS {
        for market in MARKETS {
            for tf in TFS {
                let q = format!(
                    "exchange=binance&market={}&symbol={}&tf={}&series={}",
                    market.as_str(),
                    symbol.as_str(),
                    tf.as_str(),
                    series_for_market(market)
                );
                let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{q}")).await.unwrap();
                sockets.push((q, symbol, market, ws));
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let live_ms = (t0 + 3 * 3600) * 1000 + 1;
    let live_price = |s: usize, m: usize| code(s, m) * 100.0 + 50.0;
    let seeded = |s: Symbol, m: Market| {
        let state = state.clone();
        async move { pick_shard(&state, Exchange::Binance, m, s).engine.lock().await.cvd_closed_usdt }
    };
    let usdt_seeds = (seeded(Symbol::Btc, Market::Spot).await, seeded(Symbol::Btc, Market::Usdm).await);
    // USDC first: USDT seeds must be untouched by USDC publication.
    for order in [[1usize, 3], [0, 2]] {
        for (s, symbol) in SYMBOLS.iter().enumerate() {
            for m in order {
                let price = live_price(s, m);
                // Second trade one second later flushes the first second through the real publisher.
                for (n, at) in [live_ms, live_ms + 1000].into_iter().enumerate() {
                    let trade = Trade { id: format!("r-{s}-{m}-{n}"), timestamp_ms: at, price, base: 0.01, quote: price * 0.01, is_buy: true };
                    ingest_trade(&state, Exchange::Binance, MARKETS[m], *symbol, trade).await.unwrap();
                }
            }
        }
        if order == [1, 3] {
            assert_eq!(usdt_seeds, (seeded(Symbol::Btc, Market::Spot).await, seeded(Symbol::Btc, Market::Usdm).await));
        }
    }
    let mut ws_out = serde_json::Map::new();
    for (q, symbol, market, mut ws) in sockets {
        let s = SYMBOLS.iter().position(|x| *x == symbol).unwrap();
        let m = MARKETS.iter().position(|x| *x == market).unwrap();
        let mut frames = Vec::new();
        while let Ok(Some(Ok(msg))) = tokio::time::timeout(std::time::Duration::from_millis(400), ws.next()).await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                frames.push(serde_json::from_str::<serde_json::Value>(&text).unwrap());
            }
        }
        let base: Vec<_> = frames.iter().filter(|f| f.get("bar").is_some()).collect();
        assert!(!base.is_empty(), "no base frame for {q}");
        assert!(
            base.iter().any(|f| f["bar"]["price_close"].as_f64() == Some(live_price(s, m))),
            "{q}: live trade price not delivered"
        );
        for f in &base {
            assert_eq!(f["series"], series_for_market(market));
            let price = f["bar"]["price_close"].as_f64().unwrap();
            assert_eq!((price / 100.0).floor(), code(s, m), "{q}: foreign price {price} leaked");
            let cvd = f["bar"]["cvd_usdt"].as_f64().unwrap();
            assert_eq!((cvd / 1000.0).floor(), code(s, m), "{q}: cvd band {cvd}");
        }
        assert!(frames.iter().all(|f| f.get("bar").is_some() && f.get("type").is_none()), "{q}: only base frames on /ws");
        ws_out.insert(q, serde_json::Value::Array(frames));
        let _ = ws.close(None).await;
    }
    assert_eq!(snapshot().await, before_rows, "old/new source rows unchanged by live publication");
    if let Ok(dir) = std::env::var("CVD_R_EXPORT_DIR") {
        let corpus = serde_json::json!({
            "label": "SYNTHETIC input bars/trades; ACTUAL production app_router HTTP/WS serialization (pg_temp, cvd_retirement_test). Not production data, not Safari acceptance.",
            "t0": t0,
            "meta": meta,
            "indicators_endpoint": "retired (404)",
            "history": history,
            "ws": ws_out,
        });
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/producer-corpus.json"), serde_json::to_vec_pretty(&corpus).unwrap()).unwrap();
    }
    server.abort();
    let _ = server.await;
}

/// E4-H1/P1 joined producer lane: SYNTHETIC rows in a disposable pg_temp table
/// served by the PRODUCTION `app_router`. (H1) a live invalidation while the
/// DB/outbox still holds the older valid row must not be served as valid, and a
/// live repair while the DB still lags must be served as the repaired bar, both
/// with revision-bound marks; (P1) leading/trailing/all-invalid scan rows at
/// page seams are reported within truthful coverage. Writes the actual response
/// bodies to CVD_PRODUCER_VALIDITY_PAGES for the browser consumer suite.
#[tokio::test]
#[ignore = "requires CVD_RETIREMENT_TEST_URL at disposable Unix-socket cvd_retirement_test"]
async fn postgres_router_validity_authority_and_page_seams() {
    use std::future::IntoFuture;
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit disposable URL required");
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test");
    assert!(unix_socket, "TCP or non-disposable database forbidden");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let mut state = tests::state().await;
    state.db = db.clone();
    let t0: i64 = 1_700_000_000 - 1_700_000_000 % 3600;
    let mk = |k: i64, close: f64| {
        Bar::from(models::BarRow {
            time: t0 + 60 * k,
            price_open: 100.0 + k as f64,
            price_high: 101.0 + k as f64,
            price_low: 99.0 + k as f64,
            price_close: close,
            delta_usdt: 5.0,
            cvd_usdt: 1000.0 + k as f64,
            buy_volume_base: 1.0,
            sell_volume_base: 0.5,
            buy_volume_usdt: 10.0,
            sell_volume_usdt: 5.0,
            trades: 3,
        })
    };
    // k: 0..=2 valid; 3 invalid; 4 valid; 5,6 invalid; 7 valid (closes with NaN = non-renderable).
    let rows: Vec<Bar> = (0..8).map(|k| mk(k, if [3, 5, 6].contains(&k) { f64::NAN } else { 100.5 + k as f64 })).collect();
    let input: Vec<_> = rows.iter().filter(|b| b.is_renderable()).map(|b| persistence::PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, b)).collect();
    state.outbox.enqueue_batch(&input).await.unwrap();
    state.outbox.drain(&db).await.unwrap();
    // The outbox refuses invalid bars; non-renderable rows (legacy/corrupt
    // history) are written directly into the disposable temp table.
    for k in [3i64, 5, 6] {
        sqlx::query("INSERT INTO bars VALUES ($2,$3,$4,'1m',$1,1,1,1,'NaN'::float8,5,1000,1,0.5,10,5,3)").bind(t0 + 60 * k).bind(Exchange::Binance.as_str()).bind(Market::Spot.as_str()).bind(Symbol::Btc.as_str()).execute(&db).await.unwrap();
    }
    seed_cvd_closed_from_db(&state).await.unwrap();
    hydrate_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).await.unwrap();
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state.clone())).into_future());
    let client = market_http_client().unwrap();
    let get = |q: String| {
        let client = client.clone();
        async move {
            client.get(format!("http://{addr}/history?exchange=binance&market=spot&symbol=btc&tf=1m&series=spot-v1{q}")).send().await.unwrap().json::<serde_json::Value>().await.unwrap()
        }
    };
    let times = |v: &serde_json::Value| v["bars"].as_array().unwrap().iter().map(|b| b["time"].as_i64().unwrap()).collect::<Vec<_>>();
    let inv = |v: &serde_json::Value| v["invalid_times"].as_array().unwrap().iter().map(|t| t.as_i64().unwrap()).collect::<Vec<_>>();
    let t = |k: i64| t0 + 60 * k;
    let mut pages = serde_json::Map::new();

    // P1 leading invalid in an after-page: after=t2, limit=2 scans [t3 invalid, t4 valid] (limit counts scanned rows).
    let p = get(format!("&after={}&limit=2", t(2))).await;
    assert_eq!(times(&p), [t(4)]);
    assert_eq!(inv(&p), [t(3)], "leading invalid seam row must be reported: {p}");
    assert_eq!((p["coverage_from"].as_i64(), p["coverage_to"].as_i64()), (Some(t(2) + 1), Some(t(4))));
    pages.insert("after_leading".into(), p);
    // P1 all-invalid after-page: after=t4, limit=2 scans [t5, t6] (both invalid) + has_more.
    let p = get(format!("&after={}&limit=2", t(4))).await;
    assert!(times(&p).is_empty() && p["has_more_after"] == true, "{p}");
    assert_eq!(inv(&p), [t(5), t(6)]);
    assert_eq!(p["coverage_to"].as_i64(), Some(t(6)), "all-invalid page cursor must advance");
    pages.insert("after_all_invalid".into(), p);
    // P1 trailing invalid in a before-page: before=t7, limit=1 scans [t6, t5] ; older valid t4 not returned.
    let p = get(format!("&before={}&limit=1", t(5))).await;
    assert_eq!(times(&p), [t(4)]);
    assert_eq!(inv(&p), Vec::<i64>::new(), "t3 is older than the returned row; next page owns it: {p}");
    let p2 = get(format!("&before={}&limit=1", t(4))).await;
    assert_eq!(times(&p2), [t(2)]);
    assert_eq!(inv(&p2), [t(3)], "trailing invalid seam row must be reported by the older page: {p2}");
    pages.insert("before_trailing".into(), p2);

    // H1: live invalidation of t2 while the DB still holds the older valid row (outbox not drained).
    push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, mk(2, f64::NAN)).await;
    let db_close: f64 = sqlx::query_scalar("SELECT price_close FROM bars WHERE time = $1").bind(t(2)).fetch_one(&db).await.unwrap();
    assert!(db_close.is_finite(), "precondition: DB lags with the older valid row");
    let p = get(format!("&limit=50")).await;
    assert!(!times(&p).contains(&t(2)), "lagging DB valid row must not be served after live invalidation: {p}");
    assert!(inv(&p).contains(&t(2)));
    let m = p["validity"].as_array().unwrap().iter().find(|m| m["time"] == t(2)).cloned().expect("revision-bound mark");
    assert_eq!(m["valid"], false);
    let rev_inv = m["rev"].as_u64().unwrap();
    assert!(p["epoch"].as_u64().unwrap() > 0);
    pages.insert("live_invalid_db_lag".into(), p);
    // H1 repair while the DB still lags with the INVALID version of t3: served as the repaired bar.
    push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, mk(2, 200.25)).await;
    push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, mk(3, 300.25)).await;
    let p = get(format!("&limit=50")).await;
    let bar = |k: i64| p["bars"].as_array().unwrap().iter().find(|b| b["time"] == t(k)).cloned();
    assert_eq!(bar(2).expect("repaired t2")["price_close"], 200.25);
    assert_eq!(bar(3).expect("repaired t3 despite invalid DB row")["price_close"], 300.25);
    assert!(!inv(&p).contains(&t(3)));
    let m2 = p["validity"].as_array().unwrap().iter().find(|m| m["time"] == t(2)).cloned().unwrap();
    assert!(m2["valid"] == true && m2["rev"].as_u64().unwrap() > rev_inv);
    pages.insert("live_repair_db_lag".into(), p);
    // H1 live-only repair: t4 invalidated (closed cache evicted), then repaired
    // ONLY as the open live bar (broadcast_live_payload; not in closed hist).
    // The valid mark must be served together with the live canonical bar, never
    // stamped onto the stale DB value (close 104.5).
    push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, mk(4, f64::NAN)).await;
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, mk(4, 444.25)).await;
    let p = get(format!("&limit=50")).await;
    let b4 = p["bars"].as_array().unwrap().iter().find(|b| b["time"] == t(4)).cloned().expect("live-repaired t4 served");
    assert_eq!(b4["price_close"], 444.25, "valid mark must certify the live repair, not the stale DB row: {p}");
    let m4 = p["validity"].as_array().unwrap().iter().find(|m| m["time"] == t(4)).cloned().unwrap();
    assert_eq!(m4["valid"], true);
    pages.insert("live_only_repair".into(), p);
    // Fail closed: the live bar moves on (t4 now in neither live nor closed
    // cache). The valid mark has no canonical representation, so the DB row is
    // withdrawn as an unmarked invalid fact and no valid mark is published.
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, mk(9, 109.5)).await;
    let p = get(format!("&limit=50")).await;
    assert!(!times(&p).contains(&t(4)), "unproven valid mark must not certify stale DB row: {p}");
    assert!(inv(&p).contains(&t(4)));
    assert!(!p["validity"].as_array().unwrap().iter().any(|m| m["time"] == t(4) && m["valid"] == true));
    pages.insert("valid_mark_without_canonical".into(), p);
    if let Ok(path) = std::env::var("CVD_PRODUCER_VALIDITY_PAGES") {
        std::fs::write(path, serde_json::to_string_pretty(&serde_json::Value::Object(pages)).unwrap()).unwrap();
    }
    server.abort();
}

/// Served-application chain driver (E run8). Opt-in, disposable unix-socket PG
/// only. Seeds SYNTHETIC 1m BTC spot bars through the real outbox -> PG ->
/// hydrate path and serves the PRODUCTION `app_router` (HTTP + WS + static/)
/// on loopback. Test-only control is a file mailbox in `CVD_SERVED_CHAIN_DIR`
/// (`cmd.N` JSON -> `ack.N`); no production endpoint is added. Driven by
/// tests/served-chain-browser.mjs.
#[tokio::test]
#[ignore = "requires CVD_RETIREMENT_TEST_URL + CVD_SERVED_CHAIN_DIR; driven by tests/served-chain-browser.mjs"]
async fn postgres_served_app_chain() {
    use std::future::IntoFuture;
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit disposable URL required");
    let dir = std::path::PathBuf::from(std::env::var("CVD_SERVED_CHAIN_DIR").expect("CVD_SERVED_CHAIN_DIR"));
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test");
    assert!(unix_socket, "TCP or non-disposable database forbidden");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let mut state = tests::state().await;
    state.db = db.clone();
    // Serve the ASSEMBLED static tree (scripts/assemble-static.sh output with
    // wasm-host/assets.json), never the source static/ directory.
    state.static_dir = std::path::PathBuf::from(std::env::var("CVD_SERVED_STATIC_DIR").expect("CVD_SERVED_STATIC_DIR (assembled static tree) required"));
    assert!(state.static_dir.join("wasm-host/assets.json").is_file(), "assembled static tree lacks wasm-host/assets.json");
    let n: i64 = std::env::var("CVD_SERVED_CHAIN_BARS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let t0: i64 = now - now % 3600 - 60 * n;
    let mk = |tf: Tf, k: i64, close: f64, dcvd: f64| {
        let x = k as f64;
        let buy = 5000.0 + 1000.0 * (x / 7.0).sin();
        let p = 30000.0 + 200.0 * (x / 300.0).sin() + 50.0 * (x / 37.0).sin();
        Bar::from(models::BarRow {
            time: t0 + k * tf.ms() / 1000,
            price_open: p - 1.0, price_high: p + 5.0, price_low: p - 5.0, price_close: close,
            delta_usdt: buy - 5000.0,
            cvd_usdt: 1e6 * (x / 500.0).sin() + x * 10.0 + dcvd,
            buy_volume_base: buy / p, sell_volume_base: 5000.0 / p,
            buy_volume_usdt: buy, sell_volume_usdt: 5000.0, trades: 10,
        })
    };
    let close = |k: i64| 30000.0 + 200.0 * (k as f64 / 300.0).sin() + 50.0 * (k as f64 / 37.0).sin();
    let mut input = Vec::new();
    for k in 0..n { input.push(persistence::PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &mk(Tf::M1, k, close(k), 0.0))); }
    for k in 0..n / 5 { input.push(persistence::PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M5, &mk(Tf::M5, k, close(k) + 1.0, 0.0))); }
    state.outbox.enqueue_batch(&input).await.unwrap();
    state.outbox.drain(&db).await.unwrap();
    assert_eq!(state.outbox.pending_count().await.unwrap(), 0);
    seed_cvd_closed_from_db(&state).await.unwrap();
    hydrate_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).await.unwrap();
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state.clone())).into_future());
    std::fs::write(dir.join("port"), format!("{{\"port\":{},\"t0\":{t0},\"n\":{n}}}", addr.port())).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let mut seq = 0u32;
    loop {
        assert!(std::time::Instant::now() < deadline, "driver did not stop");
        let cmd = dir.join(format!("cmd.{seq}"));
        let Ok(text) = std::fs::read_to_string(&cmd) else { tokio::time::sleep(std::time::Duration::from_millis(20)).await; continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { tokio::time::sleep(std::time::Duration::from_millis(20)).await; continue };
        let op = v["op"].as_str().unwrap_or("");
        if op == "stop" { std::fs::write(dir.join(format!("ack.{seq}")), "stopped").unwrap(); break }
        let k = v["k"].as_i64().unwrap();
        let c = v["close"].as_f64().unwrap_or(f64::NAN);
        let bar = mk(Tf::M1, k, c, v["dcvd"].as_f64().unwrap_or(0.0));
        // The ack carries the exact emitted bar so the driver builds its
        // expected live basis from what the producer actually sent.
        let emitted = serde_json::to_string(&bar).unwrap();
        match op {
            "publish" => push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, bar).await,
            "live" => broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, bar).await,
            _ => panic!("unknown op {op}"),
        }
        std::fs::write(dir.join(format!("ack.{seq}")), emitted).unwrap();
        seq += 1;
    }
    server.abort();
}
