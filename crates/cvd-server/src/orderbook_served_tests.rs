//! Served order-book chain driver: PRODUCTION `app_router` (HTTP + /book WS +
//! assembled static) on loopback over a disposable unix-socket PostgreSQL,
//! with the book hub fed by either a scripted SYNTHETIC replay exchange
//! (`CVD_BOOK_FEED=replay`, default) or the REAL public Binance feed
//! (`CVD_BOOK_FEED=real`). Control is a test-only file mailbox in
//! `CVD_SERVED_CHAIN_DIR` (`cmd.N` -> `ack.N`); no production endpoint is
//! added. Driven by tests/book-browser.mjs.
use super::*;
use crate::orderbook::{BinanceFeed, BookFeed, BookHub, BookKey, TextStream};
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

#[derive(Default)]
struct ScriptFeed {
    current: StdMutex<HashMap<String, mpsc::UnboundedSender<anyhow::Result<String>>>>,
    snapshots: StdMutex<HashMap<String, String>>,
    connects: StdMutex<HashMap<String, u64>>,
}

impl BookFeed for ScriptFeed {
    // Spot books are keyed `spot:<native>` so they never share a USD-M script.
    fn connect(
        &self,
        v: BookKey,
        n: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TextStream>> + Send + '_>> {
        let n = &script_key(v, n);
        let (tx, rx) = mpsc::unbounded_channel();
        self.current.lock().unwrap().insert(n.to_string(), tx);
        *self.connects.lock().unwrap().entry(n.to_string()).or_default() += 1;
        Box::pin(async move {
            let s = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) });
            Ok(Box::pin(s) as TextStream)
        })
    }
    fn snapshot(
        &self,
        v: BookKey,
        n: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + '_>> {
        let n = &script_key(v, n);
        let s = self.snapshots.lock().unwrap().get(n).cloned();
        Box::pin(async move { s.ok_or_else(|| anyhow::anyhow!("no snapshot")) })
    }
}

fn script_key(k: BookKey, n: &str) -> String {
    if k.exchange == crate::types::Exchange::Coinbase {
        return format!("coinbase:{n}");
    }
    match k.market {
        Market::Coinm => format!("coinm:{n}"),
        m if m.is_spot() => format!("spot:{n}"),
        _ => n.to_string(),
    }
}

fn snapshot_json(id: u64, mid: f64, tick: f64, levels: usize) -> String {
    let bids: Vec<_> = (0..levels)
        .map(|i| [format!("{:.2}", mid - tick * (i as f64 + 1.0)), format!("{:.3}", 0.5 + (i % 7) as f64)])
        .collect();
    let asks: Vec<_> = (0..levels)
        .map(|i| [format!("{:.2}", mid + tick * (i as f64 + 1.0)), format!("{:.3}", 0.25 + (i % 5) as f64)])
        .collect();
    serde_json::json!({"lastUpdateId": id, "bids": bids, "asks": asks}).to_string()
}

#[tokio::test]
#[ignore = "requires CVD_RETIREMENT_TEST_URL + CVD_SERVED_CHAIN_DIR + CVD_SERVED_STATIC_DIR; driven by tests/book-browser.mjs"]
async fn postgres_served_book_chain() {
    use std::future::IntoFuture;
    let url = std::env::var("CVD_RETIREMENT_TEST_URL").expect("explicit disposable URL required");
    let dir = std::path::PathBuf::from(std::env::var("CVD_SERVED_CHAIN_DIR").expect("CVD_SERVED_CHAIN_DIR"));
    let db = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&db).await.unwrap();
    let unix_socket: bool = sqlx::query_scalar("SELECT inet_server_addr() IS NULL").fetch_one(&db).await.unwrap();
    assert_eq!(database, "cvd_retirement_test");
    assert!(unix_socket, "TCP or non-disposable database forbidden");
    // Ordinary table inside the asserted disposable unix-socket database (a
    // temp table would vanish if the pool replaced its connection).
    sqlx::query("CREATE TABLE bars (exchange TEXT NOT NULL, market TEXT NOT NULL, symbol TEXT NOT NULL, tf TEXT NOT NULL, time BIGINT NOT NULL, price_open DOUBLE PRECISION NOT NULL, price_high DOUBLE PRECISION NOT NULL, price_low DOUBLE PRECISION NOT NULL, price_close DOUBLE PRECISION NOT NULL, delta_usdt DOUBLE PRECISION NOT NULL, cvd_usdt DOUBLE PRECISION NOT NULL, buy_volume_base DOUBLE PRECISION NOT NULL, sell_volume_base DOUBLE PRECISION NOT NULL, buy_volume_usdt DOUBLE PRECISION NOT NULL, sell_volume_usdt DOUBLE PRECISION NOT NULL, trades BIGINT NOT NULL, PRIMARY KEY(exchange,market,symbol,tf,time))").execute(&db).await.unwrap();
    let mut state = tests::state().await;
    // FILE-backed outbox, unique inside the run-owned mailbox dir: the default
    // test MEMORY outbox lives on one private SQLite connection and loses its
    // schema if the pool replaces that connection (see
    // memory_outbox_schema_is_lost_on_connection_replacement).
    let outbox_path = dir.join(format!("outbox-{}-{}.sqlite", std::process::id(), now_nanos()));
    assert!(!outbox_path.exists(), "outbox path must be fresh");
    state.outbox = Arc::new(persistence::Outbox::open(&outbox_path).await.unwrap());
    state.db = db.clone();
    state.static_dir = std::path::PathBuf::from(std::env::var("CVD_SERVED_STATIC_DIR").expect("CVD_SERVED_STATIC_DIR"));
    assert!(state.static_dir.join("wasm-host/assets.json").is_file(), "assembled static tree required");
    // Synthetic USD-M BTC 1m bars so the price chart/history path is exercised alongside the book.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let t0 = now - now % 60 - 60 * 400;
    // Internally coherent SYNTHETIC fixtures: cvd_k = OFFSET + sum_{i<=k} delta_i
    // and buy_usdt - sell_usdt == delta_k. tests/book-browser.mjs recomputes
    // the witnesses independently from these published constants:
    //   BTC 1m: offset  250000, delta -30 if k%5==0 else 10, sell 5990
    //   BTC 5m: offset  500000, delta -40 if k%3==0 else 25, sell 18600
    //   ETH 1m: offset -120000, delta -15 if k%4==0 else  7, sell 2400
    let mut input = Vec::new();
    let mut cvd = 250_000.0;
    for k in 0..400i64 {
        let p = 30000.0 + 50.0 * (k as f64 / 20.0).sin();
        let d = if k % 5 == 0 { -30.0 } else { 10.0 };
        cvd += d;
        let bar = Bar::from(models::BarRow {
            time: t0 + k * 60 + 59,
            price_open: p - 1.0,
            price_high: p + 4.0,
            price_low: p - 4.0,
            price_close: p,
            delta_usdt: d,
            cvd_usdt: cvd,
            buy_volume_base: 0.2,
            sell_volume_base: 0.1,
            buy_volume_usdt: 5990.0 + d,
            sell_volume_usdt: 5990.0,
            trades: 3,
        });
        input.push(persistence::PendingBar::new(Exchange::Binance, Market::Usdm, Symbol::Btc, Tf::M1, &bar));
    }
    // Distinct SYNTHETIC 5m base history so a TF-only change has real,
    // different CVD/Flow input (empty->empty is not proof).
    let t5 = now - now % 300 - 300 * 400;
    let mut cvd = 500_000.0;
    for k in 0..400i64 {
        let p = 31000.0 + 80.0 * (k as f64 / 15.0).cos();
        let d = if k % 3 == 0 { -40.0 } else { 25.0 };
        cvd += d;
        let bar = Bar::from(models::BarRow {
            time: t5 + k * 300 + 299,
            price_open: p - 2.0,
            price_high: p + 9.0,
            price_low: p - 9.0,
            price_close: p,
            delta_usdt: d,
            cvd_usdt: cvd,
            buy_volume_base: 0.9,
            sell_volume_base: 0.6,
            buy_volume_usdt: 18600.0 + d,
            sell_volume_usdt: 18600.0,
            trades: 11,
        });
        input.push(persistence::PendingBar::new(Exchange::Binance, Market::Usdm, Symbol::Btc, Tf::M5, &bar));
    }
    // SYNTHETIC USD-M ETH 1m base history (distinct) for the ETH book segment.
    let mut cvd = -120_000.0;
    for k in 0..400i64 {
        let p = 2000.0 + 3.0 * (k as f64 / 11.0).sin();
        let d = if k % 4 == 0 { -15.0 } else { 7.0 };
        cvd += d;
        let bar = Bar::from(models::BarRow {
            time: t0 + k * 60 + 59,
            price_open: p - 0.2,
            price_high: p + 0.5,
            price_low: p - 0.5,
            price_close: p,
            delta_usdt: d,
            cvd_usdt: cvd,
            buy_volume_base: 1.5,
            sell_volume_base: 1.2,
            buy_volume_usdt: 2400.0 + d,
            sell_volume_usdt: 2400.0,
            trades: 5,
        });
        input.push(persistence::PendingBar::new(Exchange::Binance, Market::Usdm, Symbol::Eth, Tf::M1, &bar));
    }
    state.outbox.enqueue_batch(&input).await.unwrap();
    state.outbox.drain(&db).await.unwrap();
    hydrate_shard(&state, Exchange::Binance, Market::Usdm, Symbol::Btc).await.unwrap();
    hydrate_shard(&state, Exchange::Binance, Market::Usdm, Symbol::Eth).await.unwrap();
    let real = std::env::var("CVD_BOOK_FEED").as_deref() == Ok("real");
    let script = Arc::new(ScriptFeed::default());
    state.books = if real {
        BookHub::new(Arc::new(BinanceFeed { http: market_http_client().unwrap() }))
    } else {
        BookHub::new(script.clone())
    };
    let count_rows =
        || async { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM bars").fetch_one(&db).await.unwrap() };
    let rows0 = count_rows().await;
    let pending0 = state.outbox.pending_count().await.unwrap();
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state.clone())).into_future());
    std::fs::write(dir.join("port"), format!("{{\"port\":{},\"real\":{real},\"t0\":{t0},\"t5\":{t5}}}", addr.port()))
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let mut seq = 0u32;
    loop {
        assert!(std::time::Instant::now() < deadline, "driver did not stop");
        let cmd = dir.join(format!("cmd.{seq}"));
        let Ok(text) = std::fs::read_to_string(&cmd) else {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            continue;
        };
        let sym = v["sym"].as_str().unwrap_or("").to_string();
        let ack = match v["op"].as_str().unwrap_or("") {
            "stop" => {
                std::fs::write(dir.join(format!("ack.{seq}")), "stopped").unwrap();
                break;
            }
            "snapshot" => {
                let s = snapshot_json(
                    v["id"].as_u64().unwrap(),
                    v["mid"].as_f64().unwrap(),
                    v["tick"].as_f64().unwrap_or(0.1),
                    v["levels"].as_u64().unwrap_or(200) as usize,
                );
                script.snapshots.lock().unwrap().insert(sym, s);
                serde_json::json!({"ok": true})
            }
            "event" => {
                let native = sym.split_once(':').map(|(_, n)| n).unwrap_or(&sym);
                let mut e = serde_json::json!({"e":"depthUpdate","E": now * 1000 + v["u"].as_i64().unwrap(),"T":0,"s":native,"U":v["U"],"u":v["u"],"pu":v["pu"],
                    "b": v["b"].clone(), "a": v["a"].clone()});
                // Spot diffs carry no `pu` field at all (venue shape).
                if sym.starts_with("spot:") {
                    e.as_object_mut().unwrap().remove("pu");
                    e.as_object_mut().unwrap().remove("T");
                }
                let e = e.to_string();
                let sent = script.current.lock().unwrap().get(&sym).map(|tx| tx.send(Ok(e)).is_ok()).unwrap_or(false);
                serde_json::json!({"ok": sent})
            }
            // SYNTHETIC Coinbase Exchange level2_batch shapes (no sequence
            // ids): a full WS snapshot (sent on the same stream) then
            // absolute l2update changes with an exchange `time`.
            "cb_snapshot" => {
                let native = sym.split_once(':').map(|(_, n)| n).unwrap_or(&sym);
                let (mid, tick) = (v["mid"].as_f64().unwrap(), v["tick"].as_f64().unwrap_or(0.01));
                let side = |k: &str, sgn: f64, base: f64| -> Vec<[String; 2]> {
                    (0..v[k].as_u64().unwrap_or(200) as usize)
                        .map(|i| {
                            [
                                format!("{:.2}", mid + sgn * tick * (i as f64 + 1.0)),
                                format!("{:.8}", base + (i % 7) as f64 * 0.125),
                            ]
                        })
                        .collect()
                };
                let e = serde_json::json!({"type": "snapshot", "product_id": v["product"].as_str().unwrap_or(native),
                    "bids": side("bids", -1.0, 0.5), "asks": side("asks", 1.0, 0.25)})
                .to_string();
                let sent = script.current.lock().unwrap().get(&sym).map(|tx| tx.send(Ok(e)).is_ok()).unwrap_or(false);
                serde_json::json!({"ok": sent})
            }
            "cb_update" => {
                let native = sym.split_once(':').map(|(_, n)| n).unwrap_or(&sym);
                let time = chrono::DateTime::from_timestamp_millis(now * 1000 + v["dt_ms"].as_i64().unwrap_or(0))
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
                let e = serde_json::json!({"type": "l2update", "product_id": v["product"].as_str().unwrap_or(native),
                    "changes": v["changes"].clone(), "time": time})
                .to_string();
                let sent = script.current.lock().unwrap().get(&sym).map(|tx| tx.send(Ok(e)).is_ok()).unwrap_or(false);
                serde_json::json!({"ok": sent})
            }
            // SYNTHETIC Coinbase DRB JSON-RPC envelopes (`book.<native>.100ms`):
            // subscribe ack, full snapshot (change_id, no prev), then change
            // with prev_change_id/change_id and [action, price, amount] levels
            // (amount = BASE coin; JSON numbers like the live feed).
            "drb_ack" => {
                let native = sym.split_once(':').map(|(_, n)| n).unwrap_or(&sym);
                let e = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [format!("book.{native}.100ms")]})
                    .to_string();
                let sent = script.current.lock().unwrap().get(&sym).map(|tx| tx.send(Ok(e)).is_ok()).unwrap_or(false);
                serde_json::json!({"ok": sent})
            }
            "drb_snapshot" => {
                let native = sym.split_once(':').map(|(_, n)| n).unwrap_or(&sym);
                let (mid, tick) = (v["mid"].as_f64().unwrap(), v["tick"].as_f64().unwrap_or(0.1));
                let side = |k: &str, sgn: f64, base: f64| -> Vec<serde_json::Value> {
                    (0..v[k].as_u64().unwrap_or(200) as usize)
                        .map(|i| {
                            let p = format!("{:.2}", mid + sgn * tick * (i as f64 + 1.0)).parse::<f64>().unwrap();
                            serde_json::json!(["new", p, base + (i % 7) as f64 * 0.125])
                        })
                        .collect()
                };
                let e = serde_json::json!({"jsonrpc": "2.0", "method": "subscription", "params": {"channel": format!("book.{native}.100ms"),
                    "data": {"type": "snapshot", "timestamp": now * 1000 + v["dt_ms"].as_i64().unwrap_or(0), "change_id": v["cid"],
                        "instrument_name": v["instrument"].as_str().unwrap_or(native),
                        "bids": side("bids", -1.0, 0.5), "asks": side("asks", 1.0, 0.25)}}})
                .to_string();
                let sent = script.current.lock().unwrap().get(&sym).map(|tx| tx.send(Ok(e)).is_ok()).unwrap_or(false);
                serde_json::json!({"ok": sent})
            }
            "drb_change" => {
                let native = sym.split_once(':').map(|(_, n)| n).unwrap_or(&sym);
                let e = serde_json::json!({"jsonrpc": "2.0", "method": "subscription", "params": {"channel": v["channel"].as_str().map(str::to_string).unwrap_or(format!("book.{native}.100ms")),
                    "data": {"type": "change", "timestamp": now * 1000 + v["dt_ms"].as_i64().unwrap_or(0), "prev_change_id": v["prev"], "change_id": v["cid"],
                        "instrument_name": v["instrument"].as_str().unwrap_or(native), "bids": v["b"].clone(), "asks": v["a"].clone()}}})
                .to_string();
                let sent = script.current.lock().unwrap().get(&sym).map(|tx| tx.send(Ok(e)).is_ok()).unwrap_or(false);
                serde_json::json!({"ok": sent})
            }
            "replace_outbox_connection" => {
                let before = state.outbox.pending_count().await.unwrap();
                let rows_before = count_rows().await;
                // Retire the pool's one physical connection; the next query opens a new one.
                drop(state.outbox.pool.acquire().await.unwrap().detach());
                let after = state.outbox.pending_count().await;
                let schema: Vec<String> = sqlx::query_scalar(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('pending_bars','retired_pending_bars') ORDER BY name",
                )
                .fetch_all(&state.outbox.pool)
                .await
                .unwrap_or_default();
                serde_json::json!({"ok": after.is_ok(), "pending_before": before,
                    "pending_after": after.as_ref().ok(), "error": after.as_ref().err().map(|e| e.to_string()),
                    "schema": schema, "file_backed": outbox_path.is_file(),
                    "bars_rows_before": rows_before, "bars_rows_after": count_rows().await})
            }
            "drop" => serde_json::json!({"ok": script.current.lock().unwrap().remove(&sym).is_some()}),
            "state" => {
                let active: Vec<_> =
                    state.books.active().into_iter().map(|(s, n)| serde_json::json!([s.label(), n])).collect();
                serde_json::json!({"active": active, "connects": script.connects.lock().unwrap().clone(),
                    "bars_rows": count_rows().await, "bars_rows_at_start": rows0,
                    "outbox_pending": state.outbox.pending_count().await.unwrap(), "outbox_pending_at_start": pending0})
            }
            op => panic!("unknown op {op}"),
        };
        std::fs::write(dir.join(format!("ack.{seq}")), ack.to_string()).unwrap();
        seq += 1;
    }
    server.abort();
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

/// Failure-class proof for the historical served gate `no such table:
/// pending_bars`: the test MEMORY outbox's schema lives only on its one
/// physical connection, so retiring that connection loses it.
#[tokio::test]
async fn memory_outbox_schema_is_lost_on_connection_replacement() {
    let outbox = persistence::Outbox::memory().await;
    assert_eq!(outbox.pending_count().await.unwrap(), 0);
    drop(outbox.pool.acquire().await.unwrap().detach());
    let err = outbox.pending_count().await.unwrap_err().to_string();
    assert!(err.contains("no such table: pending_bars"), "{err}");
}

#[tokio::test]
async fn file_outbox_schema_survives_connection_replacement() {
    let dir = std::env::temp_dir().join(format!("outbox-replace-{}-{}", std::process::id(), now_nanos()));
    let path = dir.join("outbox.sqlite");
    {
        let outbox = persistence::Outbox::open(&path).await.unwrap();
        assert_eq!(outbox.pending_count().await.unwrap(), 0);
        drop(outbox.pool.acquire().await.unwrap().detach());
        assert_eq!(outbox.pending_count().await.unwrap(), 0);
        assert_eq!(outbox.retired_count().await.unwrap(), 0);
        outbox.pool.close().await;
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
