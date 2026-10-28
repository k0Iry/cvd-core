//! Opt-in REAL PostgreSQL integration for the indicator history/warm-up
//! paths: actual `load_history_page` and `hydrate_shard` against an isolated
//! disposable database, with the real runner process and the public sample /
//! fixture libraries. Ignored by default.
//!
//! Run (non-production, Unix-socket only):
//! `CVD_PLUGIN_PG_TEST_URL=postgresql://cvd_test@localhost:55439/cvd_plugin_test?host=<socket-dir> \
//!  cargo test --offline -p cvd-server pg_plugin -- --ignored --test-threads=1`
//!
//! Safety: asserts `current_database() = 'cvd_plugin_test'` and a Unix-socket
//! connection (`inet_server_addr() IS NULL`) BEFORE any write; uses one
//! connection, `search_path = pg_temp` and a connection-local TEMP `bars`
//! table, so nothing persistent is created or modified.

use super::*;
use crate::indicators::{compute_history_guarded, IndicatorManager, ScopeBase, Status};
use crate::plugin_tests::{config, wait_status};
use cvd_indicator_sdk::{codes, SeriesValues};

const N: i64 = 40;
const TFS: [&str; 5] = ["1m", "5m", "15m", "30m", "1h"];

fn t(i: i64) -> i64 {
    i * 60 + 59
}

fn line(o: &cvd_indicator_sdk::Output, key: &str) -> Vec<Option<f64>> {
    match &o.series[key] {
        SeriesValues::Line { values } => values.clone(),
        other => panic!("{key} not a line: {other:?}"),
    }
}

async fn snapshot(db: &PgPool) -> String {
    sqlx::query_scalar("SELECT json_agg(b ORDER BY exchange,market,symbol,tf,time)::text FROM bars b")
        .fetch_one(db)
        .await
        .unwrap()
}

async fn page(
    db: &PgPool,
    mgr: &IndicatorManager,
    ids: &[String],
    exchange: &str,
    market: &str,
    tf: &str,
    limit: usize,
    before: Option<i64>,
    after: Option<i64>,
) -> models::HistoryPayload {
    load_history_page(db, exchange, "BTC", market, tf, limit, before, after, None, true, mgr, ids).await.unwrap()
}

/// Source close for venue `v` (binance=100, coinbase=200) at row i.
fn close(base: f64, i: i64) -> f64 {
    base + i as f64
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL CVD_PLUGIN_PG_TEST_URL (cvd_plugin_test); uses a temporary bars table"]
async fn pg_plugin_history_and_hydration_real_runner() {
    let url = std::env::var("CVD_PLUGIN_PG_TEST_URL").expect("explicit isolated test URL required");
    let db = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    // ---- Safety gate before ANY write ----
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_plugin_test", "use the dedicated disposable test database");
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert!(unix_socket, "must be a Unix-socket (non-TCP) connection");
    sqlx::query("SET search_path TO pg_temp").execute(&db).await.unwrap();
    sqlx::query("CREATE TEMP TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let schema: String = sqlx::query_scalar("SELECT n.nspname::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.oid = 'bars'::regclass")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(schema.starts_with("pg_temp"), "bars must resolve to the temp table, got {schema}");

    // Synthetic public numbers: binance=100+i, coinbase=200+i, retired bybit /
    // unknown = 1e6 (and NaN on one row) which must never reach any output.
    for (exchange, markets, base) in [
        ("binance", &["spot", "usdm", "coinm"][..], 100.0),
        ("coinbase", &["spot", "usdc"][..], 200.0),
        ("bybit", &["spot", "usdm"][..], 1_000_000.0),
        ("unknown", &["spot"][..], 1_000_000.0),
    ] {
        for market in markets {
            for tf in TFS {
                sqlx::query("INSERT INTO bars SELECT $1,$2,'BTC',$3,g*60+59,$4+g-0.5,$4+g+1,$4+g-1,$4+g,(g%7)-3,g*10,1,0,$4+g,0,1 FROM generate_series(0,$5-1) g")
                    .bind(exchange).bind(market).bind(tf).bind(base).bind(N)
                    .execute(&db).await.unwrap();
            }
        }
    }
    sqlx::query("UPDATE bars SET price_close='NaN' WHERE exchange='bybit' AND time=$1")
        .bind(t(20))
        .execute(&db)
        .await
        .unwrap();

    let mgr = IndicatorManager::start(&config(
        &[
            ("test.zero_context", "zero_context_indicator", "{}", "['source', 'aggregate']"),
            ("sample.channel", "sample_indicator", "{ length = 3 }", "['source', 'aggregate']"),
            ("test.fault", "fault_indicator", "{}", "['source']"),
        ],
        2000,
    ));
    for id in ["test.zero_context", "sample.channel", "test.fault"] {
        wait_status(&mgr, id, Status::Ready, 15).await;
    }
    let all: Vec<String> = ["test.zero_context", "sample.channel", "test.fault"].map(String::from).to_vec();
    let zero = vec!["test.zero_context".to_string()];
    assert_eq!(mgr.max_context(&zero, "1m"), 0);
    assert_eq!(mgr.max_context(&all, "1m"), 500);

    let baseline = snapshot(&db).await;

    // ---- Finite data: every TF, source + aggregate, none/before/after ----
    for tf in TFS {
        for (exchange, market, base, agg_base) in
            [("binance", "spot", 100.0, 150.0), ("coinbase", "usdc", 200.0, 400.0 / 3.0 + 0.0)]
        {
            let cases: [(Option<i64>, Option<i64>, std::ops::Range<i64>); 3] =
                [(None, None, 35..40), (Some(t(20)), None, 15..20), (None, Some(t(10)), 11..16)];
            for (before, after, rows) in cases {
                let p = page(&db, &mgr, &all, exchange, market, tf, 5, before, after).await;
                let times: Vec<i64> = p.bars.iter().map(|b| b.time).collect();
                assert_eq!(
                    times,
                    rows.clone().map(t).collect::<Vec<_>>(),
                    "{exchange}/{market}/{tf} {before:?}/{after:?}"
                );
                for b in &p.bars {
                    let i = (b.time - 59) / 60;
                    assert_eq!(b.price_close, close(base, i));
                    // Retired namespaces excluded: aggregate is the active venues' mean only.
                    let expect_agg = if market == "spot" {
                        agg_base + i as f64
                    } else {
                        (100.0 * 2.0 + 200.0 + 3.0 * i as f64) / 3.0
                    };
                    assert!((b.aggregate_price_close - expect_agg).abs() < 1e-9, "aggregate excludes retired rows");
                }
                for id in &all {
                    for (mode, r) in &p.indicators[id].modes {
                        if id == "test.fault" && *mode == "aggregate" {
                            continue;
                        }
                        assert_eq!(r.status, "ok", "{id}/{mode} {tf}");
                        let o = r.output.as_ref().unwrap();
                        assert_eq!(o.times, times, "{id}/{mode}: output.times == returned bars");
                    }
                }
                let z = &p.indicators["test.zero_context"].modes;
                let src: Vec<f64> = p.bars.iter().map(|b| b.price_close).collect();
                let agg: Vec<f64> = p.bars.iter().map(|b| b.aggregate_price_close).collect();
                assert_eq!(
                    line(z["source"].output.as_ref().unwrap(), "close"),
                    src.iter().map(|x| Some(*x)).collect::<Vec<_>>()
                );
                assert_eq!(
                    line(z["aggregate"].output.as_ref().unwrap(), "close"),
                    agg.iter().map(|x| Some(*x)).collect::<Vec<_>>()
                );
                // Warm-up: first SMA(3) uses two earlier DB rows not in the page.
                let i0 = rows.start;
                let s = line(p.indicators["sample.channel"].modes["source"].output.as_ref().unwrap(), "sma");
                let want = (close(base, i0 - 2) + close(base, i0 - 1) + close(base, i0)) / 3.0;
                assert!((s[0].unwrap() - want).abs() < 1e-9, "warm-up from DB context rows: {:?} vs {want}", s[0]);
                let f = line(p.indicators["test.fault"].modes["source"].output.as_ref().unwrap(), "close");
                assert_eq!(f, src.iter().map(|x| Some(*x)).collect::<Vec<_>>());
            }
        }
    }

    // ---- Empty-after-boundary pages (incl. zero-context library) ----
    for ids in [&zero, &all] {
        for tf in TFS {
            let p = page(&db, &mgr, ids, "binance", "spot", tf, 5, None, Some(t(N - 1))).await;
            assert!(p.bars.is_empty(), "no rows after latest boundary");
            assert!(!p.has_more_after);
            for id in ids.iter() {
                for (mode, r) in &p.indicators[id].modes {
                    assert_eq!(r.status, "ok", "{id}/{mode}: empty tail is an empty ok page");
                    assert!(r.output.as_ref().unwrap().times.is_empty());
                }
            }
        }
    }

    // ---- Invalid source / aggregate rows (deliberate test edits) ----
    sqlx::query("UPDATE bars SET price_close='NaN' WHERE exchange='binance' AND market='spot' AND tf='1m' AND time=$1")
        .bind(t(30))
        .execute(&db)
        .await
        .unwrap();
    // Coinbase spot Infinity poisons the SPOT aggregate of Binance spot at 5m row 30.
    sqlx::query(
        "UPDATE bars SET price_close='Infinity' WHERE exchange='coinbase' AND market='spot' AND tf='5m' AND time=$1",
    )
    .bind(t(30))
    .execute(&db)
    .await
    .unwrap();
    let edited = snapshot(&db).await;
    assert_ne!(edited, baseline);
    for tf in ["1m", "5m"] {
        // Window covering the invalid row: F = unavailable, no numbers.
        let p = page(&db, &mgr, &all, "binance", "spot", tf, 15, None, None).await;
        assert!(p.bars.iter().all(|b| b.time != t(30)), "{tf}: invalid row dropped from base data");
        assert_eq!(p.bars.len(), 15);
        for id in &all {
            for (mode, r) in &p.indicators[id].modes {
                assert_eq!(r.status, "unavailable", "{tf} {id}/{mode}");
                assert!(r.output.is_none());
                assert_eq!(r.error.as_ref().unwrap().code, codes::UNAVAILABLE);
            }
        }
        let text = serde_json::to_string(&p).unwrap();
        assert!(!text.contains("NaN") && !text.contains("inf"), "{text}");
        // Clean window after the invalid row: zero-context exact; 500-ctx sample still covers it.
        let p = page(&db, &mgr, &all, "binance", "spot", tf, 5, None, Some(t(30))).await;
        assert_eq!(p.bars.iter().map(|b| b.time).collect::<Vec<_>>(), (31..36).map(t).collect::<Vec<_>>());
        let z = &p.indicators["test.zero_context"].modes;
        assert_eq!(z["source"].status, "ok");
        assert_eq!(
            line(z["source"].output.as_ref().unwrap(), "close"),
            (31..36).map(|i| Some(close(100.0, i))).collect::<Vec<_>>()
        );
        assert_eq!(
            line(z["aggregate"].output.as_ref().unwrap(), "close"),
            (31..36).map(|i| Some(150.0 + i as f64)).collect::<Vec<_>>()
        );
        assert_eq!(p.indicators["sample.channel"].modes["source"].status, "unavailable");
        // Before-page entirely below the invalid row: clean and exact for all.
        let p = page(&db, &mgr, &all, "binance", "spot", tf, 5, Some(t(30)), None).await;
        for id in &all {
            assert!(p.indicators[id].modes.values().all(|r| r.status == "ok"), "{tf} {id}");
        }
    }
    // Other venues/TFs untouched by the edits stay ok (no cross-source poison).
    let p = page(&db, &mgr, &all, "binance", "usdm", "1m", 15, None, None).await;
    assert!(p.indicators.values().all(|h| h.modes.values().all(|r| r.status == "ok")));

    // ---- Actual hydration: poison recorded BEFORE filtering ----
    let state = tests::state_with(mgr.clone()).await;
    let mut state = state;
    state.db = db.clone();
    hydrate_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).await.unwrap();
    let sh = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
    for (tf, h) in [(Tf::M1, &sh.hist_1m), (Tf::M5, &sh.hist_5m)] {
        assert_eq!(sh.invalid_times_for(tf), vec![t(30)], "{tf:?} poison recorded");
        let hist = h.read().await;
        assert_eq!(hist.len() as i64, N - 1);
        assert!(hist.iter().all(|b| b.time != t(30) && b.is_renderable()));
    }
    for tf in [Tf::M15, Tf::M30, Tf::H1] {
        assert!(sh.invalid_times_for(tf).is_empty(), "{tf:?} clean");
    }
    let h1: Vec<Bar> = sh.hist_1m.read().await.iter().cloned().collect();
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    let inv = sh.invalid_times_for(Tf::M1);
    let out = compute_history_guarded(&mgr, &all, &scope, 0, &h1, h1.len() - 5, &inv).await;
    assert_eq!(out["test.zero_context"].modes["source"].status, "ok", "context clean after poison");
    assert_eq!(out["test.fault"].modes["source"].status, "ok", "2-bar context clears row 30");
    assert_eq!(out["sample.channel"].modes["source"].status, "unavailable", "500-bar context still poisoned");
    let keep = h1.iter().position(|b| b.time == t(31)).unwrap();
    let out = compute_history_guarded(&mgr, &all, &scope, 0, &h1, keep, &inv).await;
    assert_eq!(out["test.fault"].modes["source"].status, "unavailable", "context covers poison");
    assert_eq!(out["test.zero_context"].modes["source"].status, "ok");
    // Validated replacement of the same bucket clears poison (recovery).
    sh.clear_invalid(Tf::M1, t(30));
    let out = compute_history_guarded(&mgr, &all, &scope, 0, &h1, keep, &sh.invalid_times_for(Tf::M1)).await;
    assert!(out.values().all(|h| h.modes.values().all(|r| r.status == "ok")));
    // Retired namespaces never seeded a shard.
    assert!(state.shards.keys().all(|(e, _, _)| e.as_str() != "bybit"));

    // Reads never rewrote persisted rows (only the two deliberate edits above).
    assert_eq!(snapshot(&db).await, edited, "reads must not modify rows");
    let nonfinite: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM bars WHERE price_close IN ('NaN'::float8, 'Infinity'::float8)")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(nonfinite, 2 * 5 + 1 + 1, "bybit NaN rows (2 markets x 5 TF) + binance NaN + coinbase Inf");
}
