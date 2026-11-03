// B: persisted restart through a FILE SQLite outbox + isolated PostgreSQL (t_f8cd5ba7).
// Opt-in: CVD_DURABILITY_PG_URL must name a run-owned Unix-socket cluster whose
// admin database name starts with `cvd_drb_durability_`; each scenario creates
// its own fresh database. Nothing is copied from the crashed engine.
use super::*;
use std::collections::BTreeMap;

type DbRow = (String, i64, f64, f64, f64, f64, f64, f64, f64, f64, f64, f64, i64);
type Key = (String, i64);

fn row_of(tf: &str, b: &crate::models::BarRow) -> DbRow {
    (
        tf.into(),
        b.time,
        b.price_open,
        b.price_high,
        b.price_low,
        b.price_close,
        b.delta_usdt,
        b.cvd_usdt,
        b.buy_volume_base,
        b.sell_volume_base,
        b.buy_volume_usdt,
        b.sell_volume_usdt,
        b.trades,
    )
}

/// Uninterrupted reference: exactly the closed bars its outbox receives.
async fn durable_reference() -> (BTreeMap<Key, DbRow>, Arc<crate::AppState>) {
    let reference = Arc::new(crate::tests::state().await);
    for n in 11..=LG_N {
        crate::ingestion::ingest_trade(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc, lg_trade(n))
            .await
            .unwrap();
    }
    let rows: Vec<(String,)> = sqlx::query_as("SELECT payload FROM pending_bars ORDER BY id")
        .fetch_all(&reference.outbox.pool)
        .await
        .unwrap();
    let mut out = BTreeMap::new();
    for (p,) in rows {
        let v: Value = serde_json::from_str(&p).unwrap();
        let tf = v["tf"].as_str().unwrap().to_string();
        let b: crate::models::BarRow = serde_json::from_value(v["bar"].clone()).unwrap();
        assert!(out.insert((tf.clone(), b.time), row_of(&tf, &b)).is_none(), "reference has one revision per key");
    }
    (out, reference)
}

async fn pg_rows(db: &sqlx::PgPool) -> BTreeMap<Key, DbRow> {
    let rows: Vec<DbRow> = sqlx::query_as(
        "SELECT tf, time, price_open, price_high, price_low, price_close, delta_usdt, cvd_usdt, buy_volume_base, \
         sell_volume_base, buy_volume_usdt, sell_volume_usdt, trades::bigint FROM bars \
         WHERE exchange='coinbase' AND market='usdc' AND symbol='BTC'",
    )
    .fetch_all(db)
    .await
    .unwrap();
    rows.into_iter().map(|r| ((r.0.clone(), r.1), r)).collect()
}

async fn scenario_db(admin: &str, name: &str) -> String {
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(admin).await.unwrap();
    let (db, sock): (String, String) =
        sqlx::query_as("SELECT current_database(), current_setting('unix_socket_directories')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(db.starts_with("cvd_drb_durability_"), "refusing non run-owned database {db}");
    let tcp: Option<String> = sqlx::query_scalar("SELECT inet_server_addr()::text").fetch_one(&pool).await.unwrap();
    assert!(tcp.is_none(), "must be a Unix-socket connection, got {tcp:?}");
    assert!(sock.starts_with("/opt/data/cache/scratch/"), "socket {sock} must be run-owned scratch");
    sqlx::query(&format!("CREATE DATABASE {name}")).execute(&pool).await.unwrap();
    pool.close().await;
    admin.replacen(&format!("/{db}"), &format!("/{name}"), 1)
}

async fn boot(outbox: &std::path::Path, url: &str) -> Arc<crate::AppState> {
    let mut s = crate::tests::state().await;
    s.outbox = Arc::new(crate::persistence::Outbox::open(outbox).await.unwrap());
    s.db = sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(url).await.unwrap();
    crate::init_db(&s.db).await.unwrap();
    crate::startup_recover(&s).await.unwrap(); // production drain -> seed
    Arc::new(s)
}

fn agg(b: &crate::aggregator::BucketAgg) -> (Option<i64>, [f64; 9], u64) {
    (
        b.cur_bucket_id,
        [
            b.open_price,
            b.high_price,
            b.low_price,
            b.last_price,
            b.delta_usdt,
            b.buy_volume_base,
            b.sell_volume_base,
            b.buy_volume_usdt,
            b.sell_volume_usdt,
        ],
        b.trades,
    )
}

/// Keys/time/OHLC/trades and 1m CVD are compared exactly. Additive volume/delta
/// of higher buckets rebuilt from minute rows (restore_minutes) may differ only
/// by f64 summation order; recorded as exact vs tolerance counts.
fn close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(1.0)
}

#[tokio::test]
#[ignore = "requires CVD_DURABILITY_PG_URL + CVD_DURABILITY_DIR (run-owned Unix-socket cluster)"]
async fn persisted_restart_at_three_commit_boundaries_matches_uninterrupted_reference() {
    use crate::persistence::failpoint::{self, Stage};
    let admin = std::env::var("CVD_DURABILITY_PG_URL").expect("run-owned PG URL");
    let dir = std::path::PathBuf::from(std::env::var("CVD_DURABILITY_DIR").expect("run-owned scratch dir"));
    let (want, reference) = durable_reference().await;
    let rshard = crate::pick_shard(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let mut report = Vec::new();
    for (i, (stage, nth, drain_in_run1)) in [
        (Stage::BeforeOutboxCommit, 700u64, true),
        (Stage::AfterOutboxCommitBeforePublish, 733, false),
        (Stage::AfterPgUpsert, 911, true),
    ]
    .into_iter()
    .enumerate()
    {
        let tag = format!("s{}_{}", i + 1, std::process::id());
        let url = scenario_db(&admin, &format!("cvd_drb_durability_{tag}")).await;
        let outbox = dir.join(format!("outbox-{tag}.sqlite"));
        assert!(!outbox.exists(), "fresh outbox file");
        let (base, _) = lg_fixture(vec![], None).await;

        // Run 1 — crash at the armed boundary.
        let s1 = boot(&outbox, &url).await;
        crate::pick_shard(&s1, Exchange::Coinbase, Market::Usdc, Symbol::Btc).engine.lock().await.recovery_from_ms =
            Some(60_000);
        let drainer = drain_in_run1.then(|| tokio::spawn(s1.outbox.clone().run(s1.db.clone())));
        failpoint::arm(stage, nth);
        let hit = failpoint::HIT.notified();
        let (tx, rx) = mpsc::channel(8);
        let w1 = tokio::spawn(drb::worker(s1.clone(), Symbol::Btc, rx, base.clone(), 0.0001));
        tx.send(lg_trade(LG_N)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(180), hit).await.expect("failpoint reached");
        w1.abort();
        let _ = w1.await;
        if let Some(d) = drainer {
            d.abort();
            let _ = d.await;
        }
        drop(tx);
        let pending_at_crash = s1.outbox.pending_count().await.unwrap();
        let pg_at_crash = pg_rows(&s1.db).await.len();
        let mem_1m_at_crash = crate::pick_shard(&s1, Exchange::Coinbase, Market::Usdc, Symbol::Btc)
            .for_tf(crate::types::Tf::M1)
            .0
            .read()
            .await
            .len();
        s1.outbox.pool.close().await;
        s1.db.close().await;
        drop(s1); // all memory (bars, CVD, buckets, cursors) discarded

        // Run 2 — fresh AppState over the same files/DB; real drain -> seed -> hydrate.
        let s2 = boot(&outbox, &url).await;
        assert_eq!(s2.outbox.pending_count().await.unwrap(), 0, "startup drained committed outbox");
        let pg_after_drain = pg_rows(&s2.db).await.len();
        let sh2 = crate::pick_shard(&s2, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
        let seeded_from = sh2.engine.lock().await.recovery_from_ms;
        assert!(seeded_from.is_some(), "seeded from PG, not copied memory");
        let drainer = tokio::spawn(s2.outbox.clone().run(s2.db.clone()));
        let (tx, rx) = mpsc::channel(8);
        let w2 = tokio::spawn(drb::worker(s2.clone(), Symbol::Btc, rx, base, 0.0001));
        crate::hydrate_all_shards(s2.clone()).await;
        tx.send(lg_trade(LG_N)).await.unwrap();
        let live = tokio::time::timeout(Duration::from_secs(180), async {
            while sh2.recovery.read().await.state != "live" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if live.is_err() {
            let e = sh2.engine.lock().await;
            panic!(
                "{stage:?} not live: {:?} last={:?} from={:?} seeded={seeded_from:?} pg_after_drain={pg_after_drain} crash pg={pg_at_crash} pend={pending_at_crash}",
                sh2.recovery.read().await.clone(),
                e.last_trade_id,
                e.recovery_from_ms
            );
        }
        drop(tx);
        w2.await.unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            while s2.outbox.pending_count().await.unwrap() > 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("outbox drained");
        drainer.abort();
        let _ = drainer.await;

        // DB parity: same key set (no missing / extra buckets) and every field.
        let got = pg_rows(&s2.db).await;
        assert_eq!(got.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>(), "{stage:?}: bucket key set");
        let (mut exact, mut tol, mut max_rel) = (0usize, 0usize, 0f64);
        for (k, w) in &want {
            let g = &got[k];
            assert_eq!((&g.0, g.1, g.2, g.3, g.4, g.5, g.12), (&w.0, w.1, w.2, w.3, w.4, w.5, w.12), "{stage:?} {k:?}");
            if k.0 == "1m" {
                assert_eq!(g.7.to_bits(), w.7.to_bits(), "{stage:?} {k:?} 1m CVD exact");
            }
            for (a, b) in [(g.6, w.6), (g.7, w.7), (g.8, w.8), (g.9, w.9), (g.10, w.10), (g.11, w.11)] {
                assert!(close(a, b), "{stage:?} {k:?} {a} vs {b}");
                if a.to_bits() == b.to_bits() {
                    exact += 1
                } else {
                    tol += 1;
                    max_rel = max_rel.max((a - b).abs() / a.abs().max(1.0));
                }
            }
        }
        // Restored live state: closed CVD, open second, and partial 1m..1h buckets.
        assert_eq!(
            sh2.cvd_closed_usdt.read().await.to_bits(),
            rshard.cvd_closed_usdt.read().await.to_bits(),
            "{stage:?} closed CVD"
        );
        let partial;
        {
            let (e2, er) = (sh2.engine.lock().await, rshard.engine.lock().await);
            assert_eq!(e2.last_trade_id.as_deref(), Some("30000"));
            assert_eq!((e2.sec.cur_sec, e2.sec.trades), (er.sec.cur_sec, er.sec.trades));
            for (g, w, tf) in [
                (&e2.a1, &er.a1, "1m"),
                (&e2.a5, &er.a5, "5m"),
                (&e2.a15, &er.a15, "15m"),
                (&e2.a30, &er.a30, "30m"),
                (&e2.a1h, &er.a1h, "1h"),
            ] {
                let (g, w) = (agg(g), agg(w));
                assert_eq!((g.0, g.2, &g.1[..4]), (w.0, w.2, &w.1[..4]), "{stage:?} open {tf} bucket");
                for (a, b) in g.1[4..].iter().zip(&w.1[4..]) {
                    assert!(close(*a, *b), "{stage:?} open {tf} {a} vs {b}");
                }
            }
            partial = [er.a5.trades, er.a15.trades, er.a30.trades, er.a1h.trades];
        }
        // Served closed-history cache after hydrate+live equals the reference cache.
        for tf in [crate::types::Tf::M1, crate::types::Tf::M5, crate::types::Tf::M15, crate::types::Tf::M30, crate::types::Tf::H1] {
            let r: Vec<_> = rshard.for_tf(tf).0.read().await.iter().map(|b| (b.time, b.trades, b.price_close, b.cvd_usdt.to_bits())).collect();
            let g: Vec<_> = sh2.for_tf(tf).0.read().await.iter().map(|b| (b.time, b.trades, b.price_close, b.cvd_usdt.to_bits())).collect();
            assert_eq!(g[g.len() - r.len().min(g.len())..], r[r.len() - r.len().min(g.len())..], "{stage:?} {tf:?} cache");
            assert_eq!(g.len(), r.len(), "{stage:?} {tf:?} cache length");
        }
        report.push(format!(
            "{stage:?}: nth={nth} pg_rows_at_crash={pg_at_crash} outbox_pending_at_crash={pending_at_crash} \
             mem_1m_at_crash={mem_1m_at_crash} pg_rows_after_startup_drain={pg_after_drain} seeded_recovery_from_ms={seeded_from:?} \
             final_rows={} reference_rows={} exact_float_fields={exact} tolerance_float_fields={tol} max_rel={max_rel:e} \
             open_trades_5m/15m/30m/1h={partial:?}",
            got.len(),
            want.len()
        ));
        s2.outbox.pool.close().await;
        s2.db.close().await;
    }
    for r in &report {
        println!("B-DURABILITY {r}");
    }
}
