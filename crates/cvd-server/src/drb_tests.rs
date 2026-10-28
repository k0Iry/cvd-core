use super::*;
use crate::ingestion::Trade;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn row(seq: u64, ms: i64, name: &str) -> Value {
    json!({"instrument_name":name,"trade_seq":seq,"timestamp":ms,"price":100.0,"amount":0.001,"contracts":10.0,"direction":"buy","trade_id":format!("USDC-{seq}")})
}
async fn fixture(rows: Vec<Value>, lag: usize) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = axum::Router::new().route(
        "/get_last_trades_by_instrument",
        axum::routing::get(
            move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| {
                let rows = rows.clone();
                let calls = calls2.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let start = q["start_seq"].parse::<u64>().unwrap();
                    let end = q["end_seq"].parse::<u64>().unwrap();
                    let count = q["count"].parse::<usize>().unwrap();
                    let mut matches = rows
                        .into_iter()
                        .filter(|r| {
                            r["trade_seq"].as_u64().is_some_and(|s| s >= start && s <= end && (n >= lag || s < end))
                        })
                        .collect::<Vec<_>>();
                    matches.truncate(count);
                    let more = matches.last().and_then(|v| v["trade_seq"].as_u64()).is_some_and(|s| s < end);
                    axum::Json(json!({"jsonrpc":"2.0","result":{"trades":matches,"has_more":more}}))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}
fn t(seq: u64, ms: i64) -> Trade {
    Trade { id: seq.to_string(), timestamp_ms: ms, price: 100.0, base: 0.001, quote: 0.1, is_buy: true }
}

#[tokio::test]
async fn socket_to_actual_workers_heartbeat_and_malformed_eth_are_isolated() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let meta=axum::Router::new().route("/get_instrument",axum::routing::get(|axum::extract::Query(q):axum::extract::Query<std::collections::HashMap<String,String>>| async move {
        let name=&q["instrument_name"];
        axum::Json(json!({"jsonrpc":"2.0","result":{"instrument_name":name,"base_currency":if name.starts_with("BTC") {"BTC"}else{"ETH"},
          "quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open",
          "future_type":"linear","instrument_type":"linear","is_active":true,"contract_size":0.0001}}))
    }));
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(http, meta).await.unwrap() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let a = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let b = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let a: Value = serde_json::from_str(&a).unwrap();
        let b: Value = serde_json::from_str(&b).unwrap();
        assert_eq!(a["method"], "public/subscribe");
        assert_eq!(a["params"]["channels"].as_array().unwrap().len(), 2);
        assert_eq!(b["method"], "public/set_heartbeat");
        assert_eq!(b["params"]["interval"], 10);
        socket
            .send(Message::Text(json!({"jsonrpc":"2.0","id":1,"result":drb::channels()}).to_string().into()))
            .await
            .unwrap();
        // A subscribed trade is legal before the independent heartbeat setup ACK.
        socket.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC_USDC-PERPETUAL.100ms","data":[row(101,1000,drb::product(Symbol::Btc))]}}).to_string().into())).await.unwrap();
        socket.send(Message::Text(json!({"jsonrpc":"2.0","id":2,"result":"ok"}).to_string().into())).await.unwrap();
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}).to_string().into(),
            ))
            .await
            .unwrap();
        let test = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let test: Value = serde_json::from_str(&test).unwrap();
        assert_eq!(test["method"], "public/test");
        assert_eq!(test["id"], 3);
        socket
            .send(Message::Text(json!({"jsonrpc":"2.0","id":3,"result":{"version":"test"}}).to_string().into()))
            .await
            .unwrap();
        socket.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.ETH_USDC-PERPETUAL.100ms","data":[{"trade_seq":102}]}}).to_string().into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        socket.close(None).await.unwrap();
    });
    let state = Arc::new(crate::tests::state().await);
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let (write, read) = socket.split();
    let result =
        tokio::time::timeout(Duration::from_secs(3), run_with_base(state.clone(), write, read, base)).await.unwrap();
    assert!(result.is_err());
    server.await.unwrap();
    let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
    assert_eq!(btc.engine.lock().await.last_trade_id.as_deref(), Some("101"));
    assert!(!btc.recovery.read().await.needs_backfill());
    assert!(eth.recovery.read().await.needs_backfill());
}

#[tokio::test]
#[ignore = "opt-in anonymous DRB actual adapter, temporary SQLite, lazy unused PostgreSQL"]
async fn public_drb_adapter_smoke() {
    use futures::StreamExt;
    let mut state = crate::tests::state().await;
    let dir = tempfile::tempdir().unwrap();
    state.outbox = Arc::new(crate::persistence::Outbox::open(&dir.path().join("drb.sqlite")).await.unwrap());
    let state = Arc::new(state);
    // Keep the same shards/outbox through both socket sessions: cursor continuity,
    // not merely two independent successful subscriptions.
    for session in 0..2 {
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async("wss://streams.drb.coinbase.com/ws/api/v2"),
        )
        .await
        .unwrap()
        .unwrap();
        let (write, read) = socket.split();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_with_base_until(state.clone(), write, read, drb::BASE.into(), async {
            let _ = close_rx.await;
        }));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        let first = loop {
            let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
            let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
            let b = btc.engine.lock().await.last_trade_id.clone();
            let e = eth.engine.lock().await.last_trade_id.clone();
            assert!(!btc.recovery.read().await.needs_backfill(), "BTC: {:?}", btc.recovery.read().await);
            assert!(!eth.recovery.read().await.needs_backfill(), "ETH: {:?}", eth.recovery.read().await);
            if let (Some(b), Some(e)) = (b, e) {
                break (b.parse::<u64>().unwrap(), e.parse::<u64>().unwrap());
            }
            assert!(tokio::time::Instant::now() < deadline, "initial live cursors not received");
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        // Cross an actual 15-second tail tick, including its HTTP request and FIFO handoff.
        tokio::time::sleep(Duration::from_secs(17)).await;
        let after = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
                let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
                assert!(!btc.recovery.read().await.needs_backfill(), "BTC: {:?}", btc.recovery.read().await);
                assert!(!eth.recovery.read().await.needs_backfill(), "ETH: {:?}", eth.recovery.read().await);
                let b = btc.engine.lock().await.last_trade_id.as_deref().unwrap_or("0").parse::<u64>().unwrap();
                let e = eth.engine.lock().await.last_trade_id.as_deref().unwrap_or("0").parse::<u64>().unwrap();
                assert!(b >= first.0 && e >= first.1, "cursor went backwards");
                if b > first.0 && e > first.1 {
                    break (b, e);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("no new sequences after tail tick");
        let _ = close_tx.send(());
        let completed =
            tokio::time::timeout(Duration::from_secs(10), task).await.expect("socket worker did not join").unwrap();
        assert!(completed.is_err(), "shutdown should return closed socket error");
        eprintln!(
            "anonymous DRB session {session}: BTC {}→{}, ETH {}→{}, temp pending={}",
            first.0,
            after.0,
            first.1,
            after.1,
            state.outbox.pending_count().await.unwrap()
        );
        for (symbol, seq) in [(Symbol::Btc, after.0), (Symbol::Eth, after.1)] {
            let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, symbol);
            assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some(seq.to_string().as_str()));
            assert!(!shard.recovery.read().await.needs_backfill());
        }
    }
}

#[tokio::test]
async fn sequence_pages_retain_same_millisecond_and_fail_on_missing_numbers() {
    let name = drb::product(Symbol::Btc);
    let (base, calls) = fixture((101..=1102).map(|s| row(s, 1000, name)).collect(), 0).await;
    let rows =
        drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, Some(100), None, 1102, 1000).await.unwrap();
    assert_eq!(rows.len(), 1002);
    assert_eq!(rows[0].id, "101");
    assert_eq!(rows.last().unwrap().id, "1102");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let (base, _) = fixture([row(101, 1000, name), row(103, 1000, name)].to_vec(), 0).await;
    assert!(matches!(
        drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, Some(100), None, 103, 1000).await,
        Err(drb::HistoryError::Retry(_))
    ));
}

#[tokio::test]
async fn real_fifo_worker_waits_for_rest_then_replays_and_isolates_eth() {
    let (base, calls) = fixture((101..=103).map(|s| row(s, 1000, drb::product(Symbol::Btc))).collect(), 1).await;
    let state = Arc::new(crate::tests::state().await);
    let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    {
        let mut e = btc.engine.lock().await;
        e.last_trade_id = Some("100".into());
        e.last_trade_ms = Some(999);
    }
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(t(103, 1000)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if btc.recovery.read().await.state == "live" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(btc.engine.lock().await.last_trade_id.as_deref(), Some("103"));
    assert_eq!(btc.engine.lock().await.sec.trades, 3);
    let diag = btc.recovery.read().await;
    assert_eq!(diag.rest_count, 3);
    assert_eq!(diag.rest_earliest_ms, Some(1000));
    assert_eq!(diag.rest_latest_ms, Some(1000));
    assert!(calls.load(Ordering::SeqCst) >= 2);
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
    assert_eq!(eth.engine.lock().await.last_trade_id, None);
}

#[tokio::test]
async fn late_websocket_identity_after_rest_replay_is_checked_before_cursor_skip() {
    for conflict in [false, true] {
        let (base, _) = fixture((101..=103).map(|s| row(s, 1000, drb::product(Symbol::Btc))).collect(), 0).await;
        let state = Arc::new(crate::tests::state().await);
        let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
        {
            let mut e = shard.engine.lock().await;
            e.last_trade_id = Some("100".into());
            e.last_trade_ms = Some(999);
        }
        let (tx, rx) = mpsc::channel(8);
        let worker = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, base, 0.0001));
        tx.send(t(103, 1000)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while shard.engine.lock().await.last_trade_id.as_deref() != Some("103") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut late = t(101, 1000);
        if conflict {
            late.price = 101.0;
            late.quote = 0.101;
        }
        tx.send(late).await.unwrap();
        drop(tx);
        worker.await.unwrap();
        assert_eq!(shard.engine.lock().await.sec.trades, 3);
        assert_eq!(shard.recovery.read().await.needs_backfill(), conflict);
        assert!(!crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth)
            .recovery
            .read()
            .await
            .needs_backfill());
    }
}

#[test]
fn replay_identity_window_keeps_first_page_through_later_pages() {
    let mut seen = std::collections::BTreeMap::new();
    for seq in 101..=4200 {
        drb::remember(&mut seen, seq, &t(seq, 1000)).unwrap();
    }
    assert!(seen.contains_key(&101));
    let mut conflicting = t(101, 1000);
    conflicting.price = 101.0;
    assert!(drb::remember(&mut seen, 101, &conflicting).is_err());
}

#[tokio::test]
async fn closed_minute_restart_proves_predecessor_and_replays_only_new_minute() {
    let name = drb::product(Symbol::Btc);
    let (base, _) =
        fixture((101..=103).map(|n| row(n, if n == 101 { 59_001 } else { 60_001 }, name)).collect(), 0).await;
    let host = base.strip_prefix("http://").unwrap();
    // A time endpoint on the same listener is required for closed-minute recovery;
    // an absent predecessor must fail closed instead of treating latest as coverage.
    let err = drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, None, Some(60_000), 103, 60_001)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("404"), "{host}: {err}");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seq_rows = (101..=103).map(|n| row(n, if n == 101 { 59_001 } else { 60_001 }, name)).collect::<Vec<_>>();
    let app=axum::Router::new()
      .route("/get_last_trades_by_instrument_and_time",axum::routing::get(move ||async move {
          axum::Json(json!({"jsonrpc":"2.0","result":{"trades":[row(100,59_000,drb::product(Symbol::Btc))],"has_more":true}}))
      }))
      .route("/get_last_trades_by_instrument",axum::routing::get(move ||{let rows=seq_rows.clone();async move {
          axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":false}}))
      }}));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    shard.engine.lock().await.recovery_from_ms = Some(60_000);
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, format!("http://{addr}"), 0.0001));
    tx.send(t(103, 60_001)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if shard.recovery.read().await.state == "live" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("103"));
    assert_eq!(shard.engine.lock().await.sec.trades, 2);
    assert!(matches!(
        drb::history(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            Symbol::Btc,
            0.0001,
            None,
            Some(0),
            103,
            86_400_001
        )
        .await,
        Err(drb::HistoryError::Fatal(_))
    ));
}

#[tokio::test]
async fn recovery_episode_keeps_fixed_target_and_20_attempt_limit_despite_new_trades() {
    let (base, calls) = fixture(vec![row(101, 1000, drb::product(Symbol::Btc))], 0).await;
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    {
        let mut e = shard.engine.lock().await;
        e.last_trade_id = Some("100".into());
        e.last_trade_ms = Some(999);
    }
    let (tx, rx) = mpsc::channel(16);
    let task = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(t(105, 1000)).await.unwrap();
    for n in 106..=109 {
        tx.send(t(n, 1000)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    tokio::time::timeout(Duration::from_secs(25), async {
        while !shard.recovery.read().await.needs_backfill() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    task.await.unwrap();
    let diag = shard.recovery.read().await.clone();
    assert_eq!(diag.attempts, 20);
    assert_eq!(calls.load(Ordering::SeqCst), 40); // two fixed-range pages per attempt
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert!(diag.detail.unwrap().contains("target 105"));
}

#[tokio::test]
async fn conflicting_duplicate_pauses_only_its_symbol_without_mutating_committed_cvd() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, "http://127.0.0.1:1".into(), 0.0001));
    tx.send(t(100, 1000)).await.unwrap();
    let mut conflict = t(100, 1000);
    conflict.price = 101.0;
    conflict.quote = 0.101;
    tx.send(conflict).await.unwrap();
    drop(tx);
    task.await.unwrap();
    assert_eq!(shard.engine.lock().await.sec.trades, 1);
    assert!(shard.recovery.read().await.needs_backfill());
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
    assert!(!eth.recovery.read().await.needs_backfill());
}

#[tokio::test]
async fn documented_positive_gateway_errors_remain_retryable() {
    use std::sync::atomic::AtomicI64;
    let code = Arc::new(AtomicI64::new(10028));
    let current = code.clone();
    let app = axum::Router::new().route(
        "/{method}",
        axum::routing::get(move || {
            let code = current.load(Ordering::SeqCst);
            async move {
                axum::Json(
                    json!({"jsonrpc":"2.0","error":{"code":code,"message":"test-only documented gateway error"}}),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = crate::market_http_client().unwrap();
    // First-party RPC error vocabulary, not JSON-RPC's generic negative codes.
    for value in [10028, 10040, 10041, 10047, 11051, 11056, 13025, 13028] {
        code.store(value, Ordering::SeqCst);
        assert!(
            matches!(drb::metadata(&http, &base, Symbol::Btc).await, Err(drb::HistoryError::Retry(_))),
            "metadata error {value} must retry"
        );
        assert!(
            matches!(drb::latest(&http, &base, Symbol::Btc, 0.0001).await, Err(drb::HistoryError::Retry(_))),
            "latest error {value} must retry"
        );
    }
    code.store(10020, Ordering::SeqCst); // invalid_or_unsupported_instrument
    assert!(matches!(drb::metadata(&http, &base, Symbol::Btc).await, Err(drb::HistoryError::Fatal(_))));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn http_transients_are_retryable_but_invalid_units_are_fatal() {
    let calls = Arc::new(AtomicUsize::new(0));
    let next = calls.clone();
    let app = axum::Router::new().route("/get_instrument", axum::routing::get(move || {
        let n = next.fetch_add(1, Ordering::SeqCst);
        async move {
        if n == 0 { return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy".to_string()); }
        if n == 1 { return (axum::http::StatusCode::TOO_MANY_REQUESTS, "throttled".to_string()); }
        if n == 2 { return (axum::http::StatusCode::OK, json!({"jsonrpc":"2.0","error":{"code":-32000,"message":"temporarily unavailable"}}).to_string()); }
        (axum::http::StatusCode::OK, json!({"jsonrpc":"2.0","result":{"instrument_name":"BTC_USDC-PERPETUAL","base_currency":"BTC","quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open","future_type":"linear","instrument_type":"inverse","is_active":true,"contract_size":0.0001}}).to_string())
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for _ in 0..3 {
        assert!(matches!(
            drb::metadata(&reqwest::Client::new(), &base, Symbol::Btc).await,
            Err(drb::HistoryError::Retry(_))
        ));
    }
    assert!(matches!(
        drb::metadata(&reqwest::Client::new(), &base, Symbol::Btc).await,
        Err(drb::HistoryError::Fatal(_))
    ));
}

#[tokio::test]
async fn transient_btc_metadata_keeps_eth_live_then_recovers_btc_on_same_socket() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let btc_meta = Arc::new(AtomicUsize::new(0));
    let calls = btc_meta.clone();
    let http_app = axum::Router::new()
        .route("/get_instrument", axum::routing::get(move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>| {
            let n = if q["instrument_name"].starts_with("BTC") { calls.fetch_add(1, Ordering::SeqCst) } else { 1 };
            async move {
                if n == 0 { return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy".to_string()); }
                let name=&q["instrument_name"];
                (axum::http::StatusCode::OK, json!({"jsonrpc":"2.0","result":{"instrument_name":name,"base_currency":if name.starts_with("BTC") {"BTC"} else {"ETH"},"quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open","future_type":"linear","instrument_type":"linear","is_active":true,"contract_size":0.0001}}).to_string())
            }
        }))
        .route("/get_last_trades_by_instrument", axum::routing::get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
            let rows: Vec<_> = if q["instrument_name"].starts_with("BTC") && q.contains_key("start_seq") {
                (101..=104).filter(|s| *s >= q["start_seq"].parse::<u64>().unwrap() && *s <= q["end_seq"].parse::<u64>().unwrap())
                    .map(|s| row(s,1000,drb::product(Symbol::Btc))).collect()
            } else { Vec::new() };
            axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":false}}))
        }));
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(http, http_app).await.unwrap() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (late_tx, late_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap(); // subscribe
        ws.next().await.unwrap().unwrap(); // heartbeat
        ws.send(Message::Text(json!({"jsonrpc":"2.0","id":1,"result":drb::channels()}).to_string().into()))
            .await
            .unwrap();
        ws.send(Message::Text(json!({"jsonrpc":"2.0","id":2,"result":"ok"}).to_string().into())).await.unwrap();
        for (symbol, seq) in [(Symbol::Btc, 103), (Symbol::Eth, 201)] {
            ws.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":format!("trades.{}.100ms",drb::product(symbol)),"data":[row(seq,1000,drb::product(symbol))]}}).to_string().into())).await.unwrap();
        }
        let _ = late_rx.await;
        ws.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC_USDC-PERPETUAL.100ms","data":[row(104,1000,drb::product(Symbol::Btc))]}}).to_string().into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    });
    let state = Arc::new(crate::tests::state().await);
    let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
    {
        let mut e = btc.engine.lock().await;
        e.last_trade_id = Some("100".into());
        e.last_trade_ms = Some(999);
    }
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let (write, read) = socket.split();
    let task = tokio::spawn(run_with_base(state.clone(), write, read, base));
    tokio::time::timeout(Duration::from_secs(3), async {
        while eth.engine.lock().await.last_trade_id.as_deref() != Some("201") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ETH must progress before BTC retry");
    assert_eq!(btc_meta.load(Ordering::SeqCst), 1);
    assert_eq!(btc.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert!(!btc.recovery.read().await.needs_backfill());
    tokio::time::timeout(Duration::from_secs(18), async {
        while btc_meta.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("BTC metadata not retried on tail tick");
    // Permit the main socket loop to consume the validated-size handoff.
    tokio::time::sleep(Duration::from_millis(100)).await;
    late_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while btc.engine.lock().await.last_trade_id.as_deref() != Some("104") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("BTC should replay 101..104 from the durable cursor");
    assert_eq!(btc.engine.lock().await.sec.trades, 4);
    assert_eq!(eth.engine.lock().await.sec.trades, 1);
    server.await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap().is_err());
}

#[tokio::test]
async fn stop_during_held_tail_http_still_joins_inflight_ingestion_flush() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let app = axum::Router::new()
        .route("/get_instrument", axum::routing::get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
            let name=&q["instrument_name"];
            axum::Json(json!({"jsonrpc":"2.0","result":{"instrument_name":name,"base_currency":if name.starts_with("BTC") {"BTC"}else{"ETH"},"quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open","future_type":"linear","instrument_type":"linear","is_active":true,"contract_size":0.0001}}))
        }))
        .route("/get_last_trades_by_instrument", axum::routing::get(move || {
            let signal = signal.clone();
            async move { signal.notify_one(); std::future::pending::<axum::Json<Value>>().await }
        }));
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(http, app).await.unwrap() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (send_tx, send_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap();
        ws.next().await.unwrap().unwrap();
        ws.send(Message::Text(json!({"jsonrpc":"2.0","id":1,"result":drb::channels()}).to_string().into()))
            .await
            .unwrap();
        ws.send(Message::Text(json!({"jsonrpc":"2.0","id":2,"result":"ok"}).to_string().into())).await.unwrap();
        for (seq, ms) in [(100, 59001), (101, 60001)] {
            ws.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.ETH_USDC-PERPETUAL.100ms","data":[row(seq,ms,drb::product(Symbol::Eth))]}}).to_string().into())).await.unwrap();
        }
        let _ = send_rx.await;
        ws.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.ETH_USDC-PERPETUAL.100ms","data":[row(102,61001,drb::product(Symbol::Eth))]}}).to_string().into())).await.unwrap();
        std::future::pending::<()>().await;
    });
    let state = Arc::new(crate::tests::state().await);
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let (write, read) = socket.split();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(run_with_base_until(state.clone(), write, read, base, async {
        let _ = stop_rx.await;
    }));
    tokio::time::timeout(Duration::from_secs(3), async {
        while eth.engine.lock().await.last_trade_id.as_deref() != Some("101") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let block = eth.hist_1m.write().await;
    send_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.outbox.pending_count().await.unwrap() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("in-flight flush did not reach held history lock");
    tokio::time::timeout(Duration::from_secs(18), entered.notified()).await.expect("tail HTTP did not start");
    stop_tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "shutdown must wait for admitted flush");
    drop(block);
    let result = tokio::time::timeout(Duration::from_secs(3), task).await.expect("workers not joined").unwrap();
    assert!(result.is_err());
    assert_eq!(eth.engine.lock().await.last_trade_id.as_deref(), Some("102"));
    assert_eq!(eth.engine.lock().await.a1.trades, 1);
}

#[tokio::test]
async fn transient_metadata_reconnects_and_latest_failure_keeps_ws_live() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let metadata_calls = Arc::new(AtomicUsize::new(0));
    let latest_calls = Arc::new(AtomicUsize::new(0));
    let mc = metadata_calls.clone();
    let lc = latest_calls.clone();
    let app = axum::Router::new()
        .route("/get_instrument", axum::routing::get(move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>| {
            let n = mc.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 { return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy".to_string()); }
                let name=&q["instrument_name"];
                (axum::http::StatusCode::OK, json!({"jsonrpc":"2.0","result":{"instrument_name":name,"base_currency":if name.starts_with("BTC") {"BTC"}else{"ETH"},"quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open","future_type":"linear","instrument_type":"linear","is_active":true,"contract_size":0.0001}}).to_string())
            }
        }))
        .route("/get_last_trades_by_instrument", axum::routing::get(move || {
            let n = lc.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 { (axum::http::StatusCode::TOO_MANY_REQUESTS, "throttled".to_string()) }
                else { (axum::http::StatusCode::OK, json!({"jsonrpc":"2.0","result":{"trades":[]}}).to_string()) }
            }
        }));
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(http, app).await.unwrap() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for session in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            if session == 0 {
                continue;
            }
            socket.next().await.unwrap().unwrap(); // subscribe
            socket.next().await.unwrap().unwrap(); // heartbeat setup
            socket
                .send(Message::Text(json!({"jsonrpc":"2.0","id":1,"result":drb::channels()}).to_string().into()))
                .await
                .unwrap();
            socket.send(Message::Text(json!({"jsonrpc":"2.0","id":2,"result":"ok"}).to_string().into())).await.unwrap();
            socket.send(Message::Text(json!({"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC_USDC-PERPETUAL.100ms","data":[row(101,1000,drb::product(Symbol::Btc))]}}).to_string().into())).await.unwrap();
            tokio::time::sleep(Duration::from_secs(32)).await;
        }
    });
    let state = Arc::new(crate::tests::state().await);
    let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let (write, read) = socket.split();
    assert!(run_with_base(state.clone(), write, read, base.clone()).await.is_err());
    assert!(!btc.recovery.read().await.needs_backfill(), "metadata HTTP503 must not mark terminal");
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let (write, read) = socket.split();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(run_with_base_until(state.clone(), write, read, base, async {
        let _ = stop_rx.await;
    }));
    tokio::time::timeout(Duration::from_secs(36), async {
        while latest_calls.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap(); // BTC+ETH at both 15s and 30s ticks: HTTP429 recovered.
    assert_eq!(btc.engine.lock().await.last_trade_id.as_deref(), Some("101"));
    assert!(!btc.recovery.read().await.needs_backfill(), "latest HTTP429 must not pause WS");
    let _ = stop_tx.send(());
    assert!(tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn continuous_input_does_not_consume_retry_budget_before_backoff() {
    let (base, calls) = fixture(vec![], 0).await;
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    {
        let mut e = shard.engine.lock().await;
        e.last_trade_id = Some("100".into());
        e.last_trade_ms = Some(999);
    }
    let (tx, rx) = mpsc::channel(64);
    let worker = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(t(102, 1000)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for n in 103..123 {
        tx.send(t(n, 1000)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "incoming trades must not shorten one-second REST backoff");
    assert!(!shard.recovery.read().await.needs_backfill());
    drop(tx);
    worker.await.unwrap();
}

#[tokio::test]
async fn terminal_does_not_wait_for_admitted_flush_or_admit_later_trade() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, "http://127.0.0.1:1".into(), 0.0001));
    tx.send(t(100, 59001)).await.unwrap();
    tx.send(t(101, 60001)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while shard.engine.lock().await.last_trade_id.as_deref() != Some("101") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let block = shard.hist_1m.write().await;
    tx.send(t(102, 61001)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.outbox.pending_count().await.unwrap() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The engine is held by an already-admitted, non-cancellation-safe flush.
    tokio::time::timeout(
        Duration::from_secs(1),
        crate::recovery::reject_source(
            &state,
            Exchange::Coinbase,
            Market::Usdc,
            Symbol::Btc,
            "terminal while flushing".into(),
        ),
    )
    .await
    .expect("terminal must not wait on the engine/history lock");
    assert!(shard.recovery.read().await.needs_backfill());
    tx.send(t(103, 62001)).await.unwrap();
    drop(block);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(2), worker).await.unwrap().unwrap();
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("102"));
    assert_eq!(shard.engine.lock().await.a1.trades, 1);
    let diag = shard.recovery.read().await;
    assert_eq!(diag.detail.as_deref(), Some("terminal while flushing"));
}

#[tokio::test]
async fn terminal_during_rest_lag_cannot_be_overwritten_or_replayed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = axum::Router::new().route("/get_last_trades_by_instrument", axum::routing::get(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 { return axum::Json(json!({"jsonrpc":"2.0","result":{"trades":[],"has_more":false}})); }
            tokio::time::sleep(Duration::from_millis(250)).await;
            axum::Json(json!({"jsonrpc":"2.0","result":{"trades":(101..=103).map(|s|row(s,1000,drb::product(Symbol::Btc))).collect::<Vec<_>>(),"has_more":false}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    {
        let mut e = shard.engine.lock().await;
        e.last_trade_id = Some("100".into());
        e.last_trade_ms = Some(999);
    }
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(drb::worker(state.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(t(103, 1000)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    crate::recovery::reject_source(
        &state,
        Exchange::Coinbase,
        Market::Usdc,
        Symbol::Btc,
        "concurrent socket loss".into(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    drop(tx);
    worker.await.unwrap();
    let diag = shard.recovery.read().await;
    assert!(diag.needs_backfill(), "{diag:?}");
    assert_eq!(diag.detail.as_deref(), Some("concurrent socket loss"));
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
}

#[tokio::test]
async fn malformed_rpc_and_sequence_overflow_fail_closed() {
    let name = drb::product(Symbol::Btc);
    let (base, _) = fixture(vec![row(u64::MAX, 1000, name)], 0).await;
    assert!(matches!(
        drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, Some(u64::MAX - 1), None, u64::MAX, 1000)
            .await,
        Err(drb::HistoryError::Fatal(_))
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/get_last_trades_by_instrument",
            axum::routing::get(|| async {
                axum::Json(json!({"jsonrpc":"2.0","error":{"code":-32000,"message":"denied"}}))
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let err = drb::history(
        &reqwest::Client::new(),
        &format!("http://{addr}"),
        Symbol::Btc,
        0.0001,
        Some(100),
        None,
        101,
        1000,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("RPC error"));
}

#[test]
fn validates_linear_units_and_preserves_sequence_identity() {
    let metadata = serde_json::json!({"instrument_name":"BTC_USDC-PERPETUAL","base_currency":"BTC","quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open","future_type":"linear","instrument_type":"linear","is_active":true,"contract_size":0.0001});
    let size = drb::validate_instrument(&metadata, "BTC_USDC-PERPETUAL", "BTC").unwrap();
    let row = serde_json::json!({"instrument_name":"BTC_USDC-PERPETUAL","trade_seq":19295843,"timestamp":1792329442974_i64,"price":81820.1,"amount":0.0035,"contracts":35.0,"direction":"buy","trade_id":"USDC-79940693","starbase_match_id":237564719945232384_u64});
    let trade = drb::parse_trade(&row, "BTC_USDC-PERPETUAL", size).unwrap();
    assert_eq!(trade.id, "19295843");
    assert!(trade.is_buy);
    assert!((trade.quote - 286.37035).abs() < 1e-7);
    let mut bad = row.clone();
    bad["amount"] = serde_json::json!(35);
    assert!(drb::parse_trade(&bad, "BTC_USDC-PERPETUAL", size).is_err());
    let mut bad = metadata.clone();
    bad["instrument_type"] = serde_json::json!("inverse");
    assert!(drb::validate_instrument(&bad, "BTC_USDC-PERPETUAL", "BTC").is_err());
    let mut sell = row.clone();
    sell["direction"] = json!("sell");
    assert!(!drb::parse_trade(&sell, "BTC_USDC-PERPETUAL", size).unwrap().is_buy);
    let mut huge = row.clone();
    huge["price"] = json!(1e308);
    huge["amount"] = json!(1e308);
    huge["contracts"] = json!(1.0);
    assert!(drb::parse_trade(&huge, "BTC_USDC-PERPETUAL", 1e308).is_err());
}
