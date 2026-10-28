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
            i.quantity_semantics() == types::QuantitySemantics::InverseContracts,
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
            &indicators::IndicatorManager::empty(),
            &[],
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
        &indicators::IndicatorManager::empty(),
        &[],
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
    // Optional ACTUAL public plugins (CVD/Flow only) through the independent runner.
    // Unset keeps the bare-host mode (no indicators installed).
    let mut plugin_ids: Vec<String> = Vec::new();
    if let Some(dir) = std::env::var_os("CVD_PUBLIC_INDICATOR_DIR") {
        let runner = crate::plugin_tests::artifacts().0.clone();
        let mut text = format!("[indicators]\nrunner = {:?}\n", runner.to_str().unwrap());
        for name in ["libcvd_indicator_cvd.so", "libcvd_indicator_flow.so"] {
            let lib = std::path::Path::new(&dir).join(name);
            assert!(lib.is_file(), "public library missing: {}", lib.display());
            let c = cvd_indicator_host::RunnerClient::spawn(cvd_indicator_host::RunnerConfig::new(&runner, &lib))
                .await
                .unwrap_or_else(|e| panic!("{}: {}", lib.display(), e.local_detail()));
            let id = c.descriptor().id.clone();
            c.shutdown().await;
            text += &format!("[[indicators.library]]\nid = {id:?}\npath = {:?}\n", lib.to_str().unwrap());
            plugin_ids.push(id);
        }
        state.indicators = indicators::IndicatorManager::start(&config::Config::parse(&text).unwrap());
        for id in &plugin_ids {
            crate::plugin_tests::wait_status(&state.indicators, id, indicators::Status::Ready, 20).await;
        }
    }

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
    let indicators_doc: serde_json::Value =
        client.get(format!("http://{addr}/indicators")).send().await.unwrap().json().await.unwrap();
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
                // Actual public plugin envelopes: every installed id computed ok on this source.
                for id in &plugin_ids {
                    let env = &full["indicators"][id];
                    assert_eq!(env["modes"]["source"]["status"], "ok", "{q} {id}: {env}");
                    for mode in ["source", "aggregate"] {
                        let r = &env["modes"][mode];
                        if r.is_null() {
                            continue;
                        }
                        assert_eq!(r["status"], "ok", "{q} {id}/{mode}: {r}");
                        assert!(!r["output"]["times"].as_array().unwrap().is_empty(), "{q} {id}/{mode}: no output");
                    }
                }
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
        for id in &plugin_ids {
            for mode in ["source", "aggregate"] {
                assert!(
                    frames.iter().any(|f| f["type"] == "indicator" && f["id"] == id.as_str() && f["mode"] == mode),
                    "{q}: no actual {id}/{mode} plugin envelope on /ws"
                );
            }
        }
        ws_out.insert(q, serde_json::Value::Array(frames));
        let _ = ws.close(None).await;
    }
    assert_eq!(snapshot().await, before_rows, "old/new source rows unchanged by live publication");
    if let Ok(dir) = std::env::var("CVD_R_EXPORT_DIR") {
        let corpus = serde_json::json!({
            "label": "SYNTHETIC input bars/trades; ACTUAL production app_router HTTP/WS serialization (pg_temp, cvd_retirement_test). Not production data, not Safari acceptance.",
            "t0": t0,
            "meta": meta,
            "indicators_endpoint": indicators_doc,
            "public_plugins": plugin_ids,
            "history": history,
            "ws": ws_out,
        });
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/producer-corpus.json"), serde_json::to_vec_pretty(&corpus).unwrap()).unwrap();
    }
    server.abort();
    let _ = server.await;
}
