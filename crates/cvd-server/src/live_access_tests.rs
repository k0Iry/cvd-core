//! Live access is public: no key, no artificial delay. A legacy `key` query is
//! ignored by ordinary deserialization and never controls access.
use super::*;
use crate::ingestion::{ingest_trade, Trade};
use futures::StreamExt;
use std::future::IntoFuture;

async fn serve(state: Arc<AppState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<std::io::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, tokio::spawn(axum::serve(listener, app_router(state)).into_future()))
}

/// Bounded wait until the 1m BTC spot channel has exactly `n` subscribers.
async fn wait_receivers(state: &Arc<AppState>, n: usize) {
    let shard = pick_shard(state, Exchange::Binance, Market::Spot, Symbol::Btc).clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while shard.tx_1m.receiver_count() != n {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("subscriber count never reached {n}"));
}

async fn fresh_frame(state: &Arc<AppState>, addr: std::net::SocketAddr, legacy: &str, price: f64, at_ms: i64) -> serde_json::Value {
    let q = format!("exchange=binance&market=spot&symbol=BTC&tf=1m&series={SPOT_SERIES}{legacy}");
    wait_receivers(state, 0).await;
    let (mut ws, response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?{q}")).await.unwrap();
    assert_eq!(response.status(), 101, "{q}");
    wait_receivers(state, 1).await; // subscription is live before any trade
    // The second trade (next second) flushes the first second's snapshot to subscribers.
    for (n, at) in [at_ms, at_ms + 1000].into_iter().enumerate() {
        let trade = Trade { id: format!("la-{price}-{n}"), timestamp_ms: at, price, base: 0.01, quote: price * 0.01, is_buy: true };
        ingest_trade(state, Exchange::Binance, Market::Spot, Symbol::Btc, trade).await.unwrap();
    }
    // Bar time convention: last second of the 1m bucket containing the trade.
    let want_time = at_ms.div_euclid(60_000) * 60 + 59;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                let f: serde_json::Value = serde_json::from_str(&text).unwrap();
                if f["bar"]["price_close"].as_f64() == Some(price) {
                    return Some(f);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| panic!("{q}: anonymous socket received no fresh bar"));
    let _ = ws.close(None).await;
    drop(ws);
    assert_eq!(frame["bar"]["time"].as_i64(), Some(want_time), "{q}: bar of the injected trade, undelayed");
    frame
}

#[tokio::test]
async fn anonymous_websocket_receives_fresh_bar_and_legacy_key_is_inert() {
    let state = Arc::new(tests::state().await);
    let (addr, server) = serve(state.clone()).await;
    // Deterministic trade times: start of the current minute + fixed offsets.
    let now = Utc::now().timestamp_millis();
    let start = now - now.rem_euclid(60_000);
    for (i, (legacy, price)) in [("", 101.0), ("&key=", 102.0), ("&key=dummy-legacy", 103.0)].into_iter().enumerate() {
        let frame = fresh_frame(&state, addr, legacy, price, start + i as i64 * 2000).await;
        assert_eq!(frame["series"], SPOT_SERIES);
        assert_eq!(frame["bar"]["time"].as_i64(), Some(start / 1000 + 59));
    }
    // Invalid selections still fail closed with or without a legacy key.
    let client = market_http_client().unwrap();
    for query in ["exchange=bybit", "tf=bad", "symbol=SOL"] {
        for legacy in ["", "&key=dummy-legacy"] {
            let r = client.get(format!("http://{addr}/history?{query}{legacy}")).send().await.unwrap();
            assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "{query}{legacy}");
        }
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
#[ignore = "requires CVD_RETIREMENT_TEST_URL at disposable Unix-socket cvd_retirement_test"]
async fn postgres_anonymous_history_is_uncapped_and_key_independent() {
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit disposable URL required");
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test");
    assert!(unix_socket, "TCP or non-disposable database forbidden");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let now = Utc::now().timestamp();
    let minute = now - now % 60;
    // Ages in minutes: two older than the retired one-hour cap, four inside it.
    let ages = [180_i64, 90, 45, 20, 5, 1];
    let times: Vec<i64> = ages.iter().map(|a| minute - a * 60).collect();
    // Coherent one-buy bars: buy 0.01 BTC at p, delta = buy_usdt - sell_usdt, cvd cumulative.
    let mut cvd = 0.0;
    for (k, t) in times.iter().enumerate() {
        let p = 100.0 + k as f64;
        let buy_usdt = p * 0.01;
        cvd += buy_usdt;
        sqlx::query("INSERT INTO bars VALUES ('binance','spot','BTC','1m',$1,$2,$2,$2,$2,$3,$4,0.01,0,$3,0,1)")
            .bind(t)
            .bind(p)
            .bind(buy_usdt)
            .bind(cvd)
            .execute(&db)
            .await
            .unwrap();
    }
    let mut state = tests::state().await;
    state.db = db.clone();
    let (addr, server) = serve(Arc::new(state)).await;
    let client = market_http_client().unwrap();
    let get = |extra: String| {
        let client = client.clone();
        async move {
            let r = client
                .get(format!("http://{addr}/history?exchange=binance&market=spot&symbol=BTC&tf=1m&series={SPOT_SERIES}{extra}"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success(), "{extra}");
            r.json::<serde_json::Value>().await.unwrap()
        }
    };
    let bar_times = |v: &serde_json::Value| v["bars"].as_array().unwrap().iter().map(|b| b["time"].as_i64().unwrap()).collect::<Vec<_>>();
    let cases = [
        (String::new(), times.clone(), false, false),
        ("&limit=2".to_string(), times[4..].to_vec(), true, false),
        (format!("&limit=2&before={}", times[4]), times[2..4].to_vec(), true, false),
        (format!("&limit=10&before={}", times[2]), times[..2].to_vec(), false, false),
        (format!("&limit=2&after={}", times[1]), times[2..4].to_vec(), true, true),
        (format!("&limit=10&after={}", times[3]), times[4..].to_vec(), true, false),
        (format!("&limit=10&after={}", times[5]), vec![], true, false),
    ];
    for (extra, want, more_before, more_after) in cases {
        let anonymous = get(extra.clone()).await;
        assert_eq!(bar_times(&anonymous), want, "{extra}");
        assert_eq!(anonymous["has_more_before"], more_before, "{extra}");
        assert_eq!(anonymous["has_more_after"], more_after, "{extra}");
        // Old open clients only: constant true, never a gate.
        assert_eq!(anonymous["realtime_enabled"], true, "{extra}");
        for legacy in ["&key=", "&key=dummy-legacy"] {
            assert_eq!(get(format!("{extra}{legacy}")).await, anonymous, "legacy key changed {extra}");
        }
    }
    let latest = get(String::new()).await;
    assert_eq!(latest["newest_time"].as_i64(), Some(times[5]), "recent bars are not cut back an hour");
    assert_eq!(latest["bars"][5]["price_close"].as_f64(), Some(105.0));
    // Unknown-but-valid source is an honest empty page, still realtime.
    let r = client
        .get(format!("http://{addr}/history?exchange=binance&market=spot&symbol=ETH&tf=1m&series={SPOT_SERIES}&key=x"))
        .send()
        .await
        .unwrap();
    let empty: serde_json::Value = r.json().await.unwrap();
    assert!(empty["bars"].as_array().unwrap().is_empty());
    assert_eq!(empty["realtime_enabled"], true);
    let both = client
        .get(format!("http://{addr}/history?exchange=binance&market=spot&symbol=BTC&tf=1m&before=1&after=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(both.status(), reqwest::StatusCode::BAD_REQUEST);
    server.abort();
    let _ = server.await;
}
