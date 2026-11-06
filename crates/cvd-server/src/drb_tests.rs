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
        drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, Some(100), None, 1102, 1000, (0, 0)).await.unwrap();
    assert_eq!(rows.len(), 1002);
    assert_eq!(rows[0].id, "101");
    assert_eq!(rows.last().unwrap().id, "1102");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let (base, _) = fixture([row(101, 1000, name), row(103, 1000, name)].to_vec(), 0).await;
    assert!(matches!(
        drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, Some(100), None, 103, 1000, (0, 0)).await,
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
    let err = drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, None, Some(60_000), 103, 60_001, (0, 0))
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
            86_400_001,
            (0, 0)
        )
        .await,
        Err(drb::HistoryError::Retry(_)) // >20h routes to the archive; an unreachable archive is transient, not terminal
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
    // Lagging history is a transient condition: the bounded episode still ends at
    // exactly 20 attempts, then the source stays recovering (never terminal/live).
    tokio::time::timeout(Duration::from_secs(25), async {
        while !shard.recovery.read().await.detail.as_deref().is_some_and(|d| d.contains("episode 1 ")) {
            assert!(!shard.recovery.read().await.needs_backfill());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let diag = shard.recovery.read().await.clone();
    assert_eq!(diag.state, "recovering");
    assert_eq!(diag.attempts, 20);
    assert_eq!(calls.load(Ordering::SeqCst), 40); // two fixed-range pages per attempt
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert!(diag.detail.unwrap().contains("after 20 attempts without committed progress; processed cursor 100; target 105"));
    drop(tx);
    task.await.unwrap();
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
        drb::history(&reqwest::Client::new(), &base, Symbol::Btc, 0.0001, Some(u64::MAX - 1), None, u64::MAX, 1000, (0, 0))
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
        (0, 0),
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

// ---- Long-gap archive bridge (t_f8cd5ba7) ----
const LG_N: u64 = 30_000;
/// Live tail observed after the fixed target, 23h later (aging-window fixtures).
const LG_TOP: u64 = LG_N + 9_000;
const LG_JUMP: i64 = 23 * 3_600_000;
fn lg_ms(seq: u64) -> i64 {
    if seq <= 10 {
        59_000 + seq as i64
    } else if seq <= LG_N {
        60_000 + ((seq - 11) / 2) as i64 * 6_000 // tied millisecond pairs
    } else {
        lg_ms(LG_N) + LG_JUMP + ((seq - LG_N) / 2) as i64 * 10
    }
}
fn lg_row(seq: u64) -> Value {
    let name = drb::product(Symbol::Btc);
    json!({"instrument_name":name,"trade_seq":seq,"timestamp":lg_ms(seq),"price":100.0 + (seq % 7) as f64,
        "amount":0.001,"contracts":10.0,"direction":if seq.is_multiple_of(3) {"sell"} else {"buy"},"trade_id":format!("USDC-{seq}")})
}
fn lg_trade(seq: u64) -> Trade {
    drb::parse_trade(&lg_row(seq), drb::product(Symbol::Btc), 0.0001).unwrap()
}
/// Loopback primary (rolling 24h retention) + `/archive` (full tape, fuzzy edges,
/// timestamp-desc order, first-call truncation, optional holes / divergent identity).
async fn lg_fixture(holes: Vec<u64>, diverge: Option<u64>) -> (String, Arc<AtomicUsize>) {
    lg_fixture_at(holes, diverge, Arc::new(std::sync::atomic::AtomicI64::new(lg_ms(LG_N)))).await
}
/// `now`: the fixture's advancing wall clock; primary keeps only `now - 24h ..`.
async fn lg_fixture_at(
    holes: Vec<u64>,
    diverge: Option<u64>,
    now: Arc<std::sync::atomic::AtomicI64>,
) -> (String, Arc<AtomicUsize>) {
    type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
    let calls = Arc::new(AtomicUsize::new(0));
    let instrument = || json!({"jsonrpc":"2.0","result":{"instrument_name":"BTC_USDC-PERPETUAL","base_currency":"BTC","quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","future_type":"linear","is_active":true,"contract_size":0.0001}});
    let now_pred = now.clone();
    let primary = move |axum::extract::Query(q): Q| {
        let target_ms = now.load(Ordering::SeqCst);
        async move {
        let (s, e) = (q["start_seq"].parse::<u64>().unwrap(), q["end_seq"].parse::<u64>().unwrap());
        let rows: Vec<Value> = (s..=e.min(LG_TOP))
            .filter(|n| target_ms - lg_ms(*n) <= 86_400_000)
            .take(q["count"].parse().unwrap())
            .map(lg_row)
            .collect();
        let more = rows.last().and_then(|v| v["trade_seq"].as_u64()).is_some_and(|n| n < e);
        axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":more}}))
        }
    };
    let (c2, h2) = (calls.clone(), holes.clone());
    let archive = move |axum::extract::Query(q): Q| {
        let (calls, holes) = (c2.clone(), h2.clone());
        async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let (s, e) = (q["start_seq"].parse::<u64>().unwrap(), q["end_seq"].parse::<u64>().unwrap());
            let mut rows: Vec<Value> = (s.saturating_sub(2).max(1)..=(e + 2).min(LG_TOP))
                .filter(|x| !(holes.contains(x) || n % 3 == 1 && *x == e)) // truncated edge row
                .map(|x| {
                    let mut r = lg_row(x);
                    if Some(x) == diverge { r["price"] = json!(1.0); }
                    r
                })
                .collect();
            rows.reverse(); // ordered by timestamp desc, not sequence
            if n % 4 == 2 { let d = rows[rows.len() / 2].clone(); rows.push(d); } // exact duplicate
            rows.truncate(1000);
            axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":false}}))
        }
    };
    let pred = |axum::extract::Query(q): Q| async move {
        let end = q["end_timestamp"].parse::<i64>().unwrap();
        let rows: Vec<Value> = (1..=LG_TOP).rev().filter(|n| lg_ms(*n) <= end).take(3).map(lg_row).collect();
        axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":true}}))
    };
    let app = axum::Router::new()
        .route("/get_last_trades_by_instrument", axum::routing::get(primary))
        // Primary by-time predecessor: only rows inside the rolling 24h retention.
        .route("/get_last_trades_by_instrument_and_time", axum::routing::get(move |axum::extract::Query(q): Q| {
            let now_ms = now_pred.load(Ordering::SeqCst);
            async move {
                let end = q["end_timestamp"].parse::<i64>().unwrap();
                let rows: Vec<Value> = (1..=LG_TOP).rev()
                    .filter(|n| lg_ms(*n) <= end && now_ms - lg_ms(*n) <= 86_400_000)
                    .take(q["count"].parse().unwrap()).map(lg_row).collect();
                axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":false}}))
            }
        }))
        .route("/archive/get_instrument", axum::routing::get(move || async move { axum::Json(instrument()) }))
        .route("/archive/get_last_trades_by_instrument", axum::routing::get(archive))
        .route("/archive/get_last_trades_by_instrument_and_time", axum::routing::get(pred));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}
async fn wait_state(shard: &crate::Shard, want: &str, secs: u64) {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if shard.recovery.read().await.state == want { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state {want} not reached"));
}
type Row = (i64, u64, i64, i64, i64);
async fn m1(shard: &crate::Shard) -> Vec<Row> {
    shard.for_tf(crate::types::Tf::M1).0.read().await.iter()
        .map(|b| (b.time, b.trades, (b.delta_usdt * 1e6).round() as i64, (b.buy_volume_base * 1e9).round() as i64, (b.cvd_usdt * 1e6).round() as i64))
        .collect()
}

#[tokio::test]
async fn long_gap_bridges_archive_to_primary_to_ws_and_survives_restart() {
    // Reference: every trade after the closed-minute boundary ingested exactly once.
    let reference = Arc::new(crate::tests::state().await);
    for n in 11..=LG_N { crate::ingestion::ingest_trade(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc, lg_trade(n)).await.unwrap(); }
    let rshard = crate::pick_shard(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let want = m1(rshard).await;
    let want_cvd = *rshard.cvd_closed_usdt.read().await;
    assert!(lg_ms(LG_N) - 60_000 > 86_400_000, "gap exceeds the old 24h guard");

    let (base, archive_calls) = lg_fixture(vec![], None).await;
    // Run 1: interrupted mid-bridge (simulated crash: task aborted, in-memory state dropped).
    let s1 = Arc::new(crate::tests::state().await);
    let sh1 = crate::pick_shard(&s1, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    sh1.engine.lock().await.recovery_from_ms = Some(60_000);
    let (tx, rx) = mpsc::channel(8);
    let w1 = tokio::spawn(drb::worker(s1.clone(), Symbol::Btc, rx, base.clone(), 0.0001));
    tx.send(lg_trade(LG_N)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let c = sh1.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
            if c.is_some_and(|c| c > 5_000) { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let diag = sh1.recovery.read().await.clone();
    assert_eq!(diag.state, "recovering");
    assert!(diag.detail.as_deref().unwrap_or("").contains("archive bridge"), "{diag:?}");
    w1.abort();
    let _ = w1.await;
    drop(tx);
    // Durable checkpoint = closed minutes only (what restart reloads); the cursor and
    // the unfinished minute are deliberately discarded.
    let closed = m1(sh1).await;
    let last = *closed.last().unwrap();
    let cvd = *sh1.cvd_closed_usdt.read().await;
    let bars: Vec<crate::Bar> = sh1.for_tf(crate::types::Tf::M1).0.read().await.iter().cloned().collect();

    // Run 2: restart from the closed-minute checkpoint.
    let s2 = Arc::new(crate::tests::state().await);
    let sh2 = crate::pick_shard(&s2, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    {
        let mut e = sh2.engine.lock().await;
        e.cvd_closed_usdt = cvd;
        e.recovery_from_ms = Some((last.0 + 1) * 1000);
        e.restore_minutes(&bars, last.0);
    }
    *sh2.cvd_closed_usdt.write().await = cvd;
    let (tx, rx) = mpsc::channel(8);
    let w2 = tokio::spawn(drb::worker(s2.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(lg_trade(LG_N)).await.unwrap();
    wait_state(sh2, "live", 60).await;
    drop(tx);
    w2.await.unwrap();
    let mut got: Vec<Row> = closed.clone();
    got.extend(m1(sh2).await);
    assert_eq!(got, want, "closed 1m bars (trades/delta/volume/cumulative) equal single uninterrupted ingest");
    assert_eq!(*sh2.cvd_closed_usdt.read().await, want_cvd);
    let (e2, er) = (sh2.engine.lock().await, rshard.engine.lock().await);
    assert_eq!(e2.last_trade_id.as_deref(), Some("30000"));
    assert_eq!((e2.sec.trades, e2.a1.trades), (er.sec.trades, er.a1.trades));
    assert!(archive_calls.load(Ordering::SeqCst) >= 10, "bounded archive pages, not one giant request");
}

#[tokio::test]
async fn long_gap_archive_hole_stays_blocked_with_exact_interval_and_identity_mismatch_is_fatal() {
    for (holes, diverge, needle) in [
        (vec![3_000u64, 3_001], None, "archive missing sequence interval 3000-3001"),
        (vec![], Some(28_010u64), "archive identity mismatch at sequence 28010"),
    ] {
        let (base, _) = lg_fixture(holes, diverge).await;
        let s = Arc::new(crate::tests::state().await);
        let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
        sh.engine.lock().await.recovery_from_ms = Some(60_000);
        let (tx, rx) = mpsc::channel(8);
        let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
        tx.send(lg_trade(LG_N)).await.unwrap();
        wait_state(sh, "needs_backfill", 90).await;
        let d = sh.recovery.read().await.clone();
        assert!(d.detail.as_deref().unwrap_or("").contains(needle), "{d:?}");
        let e = sh.engine.lock().await;
        // Never past the hole, never zero-reset; bars before it are intact.
        assert!(e.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok()).is_none_or(|c| c < 3_000));
        drop(e);
        drop(tx);
        let _ = w.await;
    }
}

async fn lg_reference(top: u64) -> (Vec<Row>, f64, u64) {
    let reference = Arc::new(crate::tests::state().await);
    for n in 11..=top { crate::ingestion::ingest_trade(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc, lg_trade(n)).await.unwrap(); }
    let rshard = crate::pick_shard(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let cvd = *rshard.cvd_closed_usdt.read().await;
    let trades = rshard.engine.lock().await.sec.trades as u64;
    (m1(rshard).await, cvd, trades)
}

#[tokio::test]
async fn long_gap_overflow_keeps_known_discarded_tail_required_before_live() {
    // Overflow the bounded long-gap buffer with a WS tail (> RECOVERY_CHANNEL_CAPACITY)
    // and send NO later WS trade: the newest observed sequence must still be required.
    assert!((LG_TOP - LG_N) as usize > crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let (base, _) = lg_fixture(vec![], None).await;
    let s = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    sh.engine.lock().await.recovery_from_ms = Some(60_000);
    let (tx, rx) = mpsc::channel(8);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    for n in LG_N..=LG_TOP { tx.send(lg_trade(n)).await.unwrap(); }
    let mut lied = None;
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let st = sh.recovery.read().await.state.clone();
            let c = sh.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
            if st == "live" {
                if c != Some(LG_TOP) { lied = Some(c); }
                break;
            }
            assert_ne!(st, "needs_backfill", "{:?}", sh.recovery.read().await.detail);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("live not reached");
    assert_eq!(lied, None, "reported live while known tail through {LG_TOP} was outstanding");
    drop(tx);
    w.await.unwrap();
    let (want, want_cvd, want_trades) = lg_reference(LG_TOP).await;
    assert_eq!(m1(sh).await, want);
    assert_eq!(*sh.cvd_closed_usdt.read().await, want_cvd);
    assert_eq!(sh.engine.lock().await.sec.trades as u64, want_trades);
}

#[tokio::test]
async fn long_gap_identity_and_primary_handoff_follow_fresh_window_as_target_ages() {
    // Primary retention advances to the freshest WS observation (23h past the fixed
    // target): the target-2000 identity window and target-relative handoff are evicted.
    let now = Arc::new(std::sync::atomic::AtomicI64::new(lg_ms(LG_TOP)));
    assert!(lg_ms(LG_TOP) - lg_ms(LG_N - drb_gate_back()) > 86_400_000, "old identity window evicted");
    assert!(lg_ms(LG_TOP) - lg_ms(LG_TOP - drb_gate_back()) < 86_400_000, "fresh identity window retained");
    let (base, _) = lg_fixture_at(vec![], None, now).await;
    let s = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    sh.engine.lock().await.recovery_from_ms = Some(60_000);
    let (tx, rx) = mpsc::channel(8);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(lg_trade(LG_N)).await.unwrap();
    tx.send(lg_trade(LG_TOP)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let st = sh.recovery.read().await.state.clone();
            assert_ne!(st, "needs_backfill", "{:?}", sh.recovery.read().await.detail);
            if st == "live" { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("live not reached");
    assert_eq!(sh.engine.lock().await.last_trade_id.as_deref(), Some(LG_TOP.to_string().as_str()));
    drop(tx);
    w.await.unwrap();
    let (want, want_cvd, _) = lg_reference(LG_TOP).await;
    assert_eq!(m1(sh).await, want, "no skip across the evicted primary range");
    assert_eq!(*sh.cvd_closed_usdt.read().await, want_cvd);
}
fn drb_gate_back() -> u64 { 2_000 }

/// Opt-in bounded REAL code-path probe (public primary + archive, no writes):
/// `CVD_DRB_REAL_PROBE=1 cargo test -p cvd-server real_archive_chunk_probe -- --ignored --nocapture`.
/// Per asset: 1 metadata x2, 1 latest, 1 identity window x2, <=3 archive pages, primary replay of 2000 seqs.
#[tokio::test]
#[ignore]
async fn real_archive_chunk_probe() {
    if std::env::var("CVD_DRB_REAL_PROBE").as_deref() != Ok("1") { return; }
    const SPAN: u64 = 2_000;
    let client = reqwest::Client::new();
    let archive = drb::archive_base(drb::BASE);
    for symbol in [Symbol::Btc, Symbol::Eth] {
        let size = drb::metadata(&client, drb::BASE, symbol).await.unwrap();
        let latest = drb::latest(&client, drb::BASE, symbol, size).await.unwrap().unwrap();
        let top: u64 = latest.id.parse().unwrap();
        let t0 = std::time::Instant::now();
        drb::archive_identity(&client, drb::BASE, &archive, symbol, size, top).await.unwrap();
        let cursor = top - 3_000;
        let target = cursor + SPAN + 1;
        // fresh_ms far ahead keeps the chunk on the archive through target-1 (no handoff).
        let chunk = drb::archive_chunk(&client, &archive, symbol, size, Some(cursor), Some(0), target, latest.timestamp_ms + 30 * 3_600_000).await.unwrap();
        let archive_secs = t0.elapsed().as_secs_f64();
        let primary = drb::history(&client, drb::BASE, symbol, size, Some(cursor), None, target, latest.timestamp_ms, (0, 0)).await.unwrap();
        assert_eq!(chunk.len() as u64, SPAN, "archive chunk exact length");
        let primary: Vec<_> = primary.into_iter().filter(|t| t.id.parse::<u64>().unwrap() < target).collect();
        assert_eq!(primary.len(), chunk.len());
        for (a, b) in chunk.iter().zip(&primary) {
            assert_eq!((&a.id, a.timestamp_ms, a.price, a.base, a.quote, a.is_buy), (&b.id, b.timestamp_ms, b.price, b.base, b.quote, b.is_buy));
        }
        println!("REAL {} latest_seq={top} chunk={}..={} trades={} archive_identity+chunk_secs={archive_secs:.2} first_ms={} last_ms={}",
            drb::product(symbol), cursor + 1, target - 1, chunk.len(), chunk[0].timestamp_ms, chunk.last().unwrap().timestamp_ms);
    }
}

#[path = "drb_durability_tests.rs"]
mod durability;

// ---- Multi-chunk archive -> primary catch-up liveness (t_09c16261) ----
// Production limits are exercised unchanged: primary PAGE=1000, MAX_PAGES=100,
// MAX_TRADES=100_000, DEADLINE=30s, ATTEMPTS=20. Only page latency is controlled.
const MC_A: u64 = 3_000; // archive-only prefix (25h before the target)
fn mc_ms(seq: u64) -> i64 {
    if seq <= 10 { 59_000 + seq as i64 }
    else if seq <= MC_A { 60_000 + (seq - 11) as i64 * 10 }
    else { 60_000 + 25 * 3_600_000 + (seq - MC_A) as i64 }
}
fn mc_row(seq: u64) -> Value {
    json!({"instrument_name":drb::product(Symbol::Btc),"trade_seq":seq,"timestamp":mc_ms(seq),"price":100.0 + (seq % 7) as f64,
        "amount":0.001,"contracts":10.0,"direction":if seq.is_multiple_of(3) {"sell"} else {"buy"},"trade_id":format!("USDC-{seq}")})
}
fn mc_trade(seq: u64) -> Trade { drb::parse_trade(&mc_row(seq), drb::product(Symbol::Btc), 0.0001).unwrap() }
/// Primary retains only `top`-relative 24h (i.e. seq > MC_A) and sleeps `lag` per page;
/// `/archive` serves the whole tape. `hole` is missing from the primary forever.
#[allow(dead_code)]
async fn mc_fixture(top: u64, lag: Duration, hole: Option<u64>) -> (String, Arc<AtomicUsize>) {
    mc_fixture_with(top, Arc::new(move |_| lag), hole).await
}
type McLag = Arc<dyn Fn(u64) -> Duration + Send + Sync>;
/// `lag(start_seq)` controls each primary page's latency.
async fn mc_fixture_with(top: u64, lag: McLag, hole: Option<u64>) -> (String, Arc<AtomicUsize>) {
    type Q = axum::extract::Query<std::collections::HashMap<String, String>>;
    let pages = Arc::new(AtomicUsize::new(0));
    let p2 = pages.clone();
    let now = mc_ms(top);
    let keep = move |n: &u64| now - mc_ms(*n) <= 86_400_000;
    let primary = move |axum::extract::Query(q): Q| {
        let pages = p2.clone();
        let lag = lag.clone();
        async move {
            pages.fetch_add(1, Ordering::SeqCst);
            let start = q["start_seq"].parse::<u64>().unwrap();
            let t0 = std::time::Instant::now();
            tokio::time::sleep(lag(start)).await;
            eprintln!("mc_page start_seq={start} end_seq={} count={} lag={:?}", q["end_seq"], q["count"], t0.elapsed());
            let (s, e) = (q["start_seq"].parse::<u64>().unwrap(), q["end_seq"].parse::<u64>().unwrap());
            let rows: Vec<Value> = (s..=e.min(top)).filter(keep).filter(|n| Some(*n) != hole)
                .take(q["count"].parse().unwrap()).map(mc_row).collect();
            let more = rows.last().and_then(|v| v["trade_seq"].as_u64()).is_some_and(|n| n < e);
            axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":more}}))
        }
    };
    let archive = move |axum::extract::Query(q): Q| async move {
        let (s, e) = (q["start_seq"].parse::<u64>().unwrap(), q["end_seq"].parse::<u64>().unwrap());
        let rows: Vec<Value> = (s..=e.min(top)).take(1000).map(mc_row).collect();
        axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":false}}))
    };
    let pred = |axum::extract::Query(q): Q| async move {
        let end = q["end_timestamp"].parse::<i64>().unwrap();
        let rows: Vec<Value> = (1..=10u64).rev().filter(|n| mc_ms(*n) <= end).take(3).map(mc_row).collect();
        axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":true}}))
    };
    let instrument = json!({"jsonrpc":"2.0","result":{"instrument_name":"BTC_USDC-PERPETUAL","base_currency":"BTC","quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","future_type":"linear","is_active":true,"contract_size":0.0001}});
    let app = axum::Router::new()
        .route("/get_last_trades_by_instrument", axum::routing::get(primary))
        .route("/archive/get_instrument", axum::routing::get(move || async move { axum::Json(instrument) }))
        .route("/archive/get_last_trades_by_instrument", axum::routing::get(archive))
        .route("/archive/get_last_trades_by_instrument_and_time", axum::routing::get(pred));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), pages)
}
async fn mc_run(top: u64, lag: Duration, hole: Option<u64>, secs: u64) -> (Arc<AppState>, String, Option<u64>, usize, Duration) {
    mc_run_with(top, Arc::new(move |_| lag), hole, secs).await
}
async fn mc_run_with(top: u64, lag: McLag, hole: Option<u64>, secs: u64) -> (Arc<AppState>, String, Option<u64>, usize, Duration) {
    let (base, pages) = mc_fixture_with(top, lag, hole).await;
    let s = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    sh.engine.lock().await.recovery_from_ms = Some(60_000);
    let (tx, rx) = mpsc::channel(8);
    let start = std::time::Instant::now();
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    tx.send(mc_trade(top)).await.unwrap();
    let mut lied = None;
    let state = tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let st = sh.recovery.read().await.state.clone();
            let c = sh.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
            if st == "live" && c != Some(top) { lied = Some(c); }
            if st == "live" || st == "needs_backfill" { break st; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("terminal state not reached");
    assert_eq!(lied, None, "reported live before fixed target {top}");
    let elapsed = start.elapsed();
    let detail = sh.recovery.read().await.detail.clone().unwrap_or_default();
    let cursor = sh.engine.lock().await.last_trade_id.as_deref().and_then(|s| s.parse::<u64>().ok());
    eprintln!("mc_run top={top} -> {state} cursor={cursor:?} pages={} elapsed={elapsed:?} detail={detail}", pages.load(Ordering::SeqCst));
    drop(tx);
    let _ = w.await;
    (s, if state == "live" { "live".into() } else { detail }, cursor, pages.load(Ordering::SeqCst), elapsed)
}

#[tokio::test]
async fn archive_to_multi_chunk_primary_catch_up_renews_only_on_committed_progress() {
    // 3 full 100k primary chunks + remainder after the archive bridge; 150ms/page
    // makes one chunk ~15s, so the healthy total (>45s) exceeds one 30s budget.
    let top = MC_A + 320_000;
    let (s, outcome, cursor, pages, elapsed) = mc_run(top, Duration::from_millis(150), None, 240).await;
    assert_eq!(outcome, "live", "healthy multi-chunk catch-up must not exhaust the budget");
    assert_eq!(cursor, Some(top));
    assert!(pages >= 320, "production page limits: >=320 primary pages of 1000");
    assert!(elapsed > Duration::from_secs(30), "crossed the old single-budget boundary: {elapsed:?}");
    let reference = Arc::new(crate::tests::state().await);
    for n in 11..=top { crate::ingestion::ingest_trade(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc, mc_trade(n)).await.unwrap(); }
    let r = crate::pick_shard(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    assert_eq!(m1(sh).await, m1(r).await, "exact bars/volumes/CVD");
    assert_eq!(*sh.cvd_closed_usdt.read().await, *r.cvd_closed_usdt.read().await);
    assert_eq!(sh.engine.lock().await.sec.trades, r.engine.lock().await.sec.trades);
}

#[tokio::test]
async fn slow_primary_pages_commit_bounded_prefix_before_deadline() {
    // 100 pages at 400ms = 40s > 30s deadline: an all-or-nothing chunk never commits.
    let top = MC_A + 110_000;
    let (_, outcome, cursor, _, _) = mc_run(top, Duration::from_millis(400), None, 240).await;
    assert_eq!(outcome, "live");
    assert_eq!(cursor, Some(top));
}

#[tokio::test]
async fn in_flight_slow_page_crossing_soft_stop_returns_validated_prefix() {
    // The archive bridge commits through MC_A+610 (renewing the budget); primary
    // pages then start at MC_A+611 (fast), +1611, +2611, +3611 (12s each, < 15s
    // client timeout). The third slow page starts ~24s into the renewed budget,
    // before the 25s soft stop, and would finish ~36s, after the unchanged 30s
    // deadline. The validated prefix through MC_A+3610 must be handed back,
    // not dropped with the in-flight future.
    let top = MC_A + 5_000;
    let slow: McLag = Arc::new(|s| if [MC_A + 1_611, MC_A + 2_611, MC_A + 3_611].contains(&s) { Duration::from_secs(12) } else { Duration::from_millis(1) });
    let (s, outcome, cursor, _, _) = mc_run_with(top, slow, None, 240).await;
    assert_eq!(outcome, "live", "in-flight slow page must not discard the validated prefix");
    assert_eq!(cursor, Some(top));
    let reference = Arc::new(crate::tests::state().await);
    for n in 11..=top { crate::ingestion::ingest_trade(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc, mc_trade(n)).await.unwrap(); }
    let (a, b) = (crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc), crate::pick_shard(&reference, Exchange::Coinbase, Market::Usdc, Symbol::Btc));
    assert_eq!(m1(a).await, m1(b).await, "exact bars/volumes/CVD");
    assert_eq!(*a.cvd_closed_usdt.read().await, *b.cvd_closed_usdt.read().await);
}

#[tokio::test]
async fn primary_hole_after_progress_still_ends_bounded_without_skipping() {
    // Progress renews the budget, but a permanent hole yields retries only: terminal.
    let top = MC_A + 20_000;
    let hole = MC_A + 12_345;
    let (_, outcome, cursor, _, elapsed) = mc_run(top, Duration::from_millis(1), Some(hole), 240).await;
    assert!(outcome.contains("budget exhausted") && outcome.contains(&format!("unproven sequence {hole}")), "{outcome}");
    assert!(cursor.is_some_and(|c| c < hole), "never past the hole");
    // Wall time is load-dependent (each committed prefix legitimately renews); the
    // structural bound is the unchanged 20-attempt cap once progress stops.
    assert!(outcome.contains("after 20 attempts without committed progress"), "{outcome}");
    let _ = elapsed;
}

// ---- Transient upstream outage longer than one bounded recovery episode ----
// (incident 2026-11-05: `error sending request` exhausted 30s/20 attempts and
// made the source sticky `needs_backfill` until a manual restart/reset).
struct Outage {
    down: std::sync::atomic::AtomicBool,
    calls: AtomicUsize,
    inflight: AtomicUsize,
    peak: AtomicUsize,
}
fn ot_ms(seq: u64) -> i64 { 60_000 + seq as i64 * 7 }
fn ot_row(seq: u64) -> Value {
    json!({"instrument_name":drb::product(Symbol::Btc),"trade_seq":seq,"timestamp":ot_ms(seq),"price":100.0 + (seq % 5) as f64,"amount":0.001,"contracts":10.0,"direction":if seq.is_multiple_of(3) {"sell"} else {"buy"},"trade_id":format!("USDC-{seq}")})
}
fn ot_trade(seq: u64) -> Trade { drb::parse_trade(&ot_row(seq), drb::product(Symbol::Btc), 0.0001).unwrap() }
async fn ot_fixture(top: u64) -> (String, Arc<Outage>) {
    let o = Arc::new(Outage { down: true.into(), calls: AtomicUsize::new(0), inflight: AtomicUsize::new(0), peak: AtomicUsize::new(0) });
    let o2 = o.clone();
    let o3 = o.clone();
    let app = axum::Router::new().route(
        "/get_last_trades_by_instrument_and_time",
        // Closed-minute predecessor for startup recovery (boundary = ot_ms(100)).
        axum::routing::get(move || {
            let o = o3.clone();
            async move {
                if o.down.load(Ordering::SeqCst) {
                    return axum::response::IntoResponse::into_response(axum::http::StatusCode::SERVICE_UNAVAILABLE);
                }
                axum::response::IntoResponse::into_response(axum::Json(json!({"jsonrpc":"2.0","result":{"trades":[ot_row(99)]}})))
            }
        }),
    ).route(
        "/get_last_trades_by_instrument",
        axum::routing::get(move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| {
            let o = o2.clone();
            async move {
                o.calls.fetch_add(1, Ordering::SeqCst);
                let now = o.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                o.peak.fetch_max(now, Ordering::SeqCst);
                struct Done(Arc<Outage>);
                impl Drop for Done { fn drop(&mut self) { self.0.inflight.fetch_sub(1, Ordering::SeqCst); } }
                let guard = Done(o.clone());
                tokio::time::sleep(Duration::from_millis(150)).await; // held response: in-flight witness
                drop(guard);
                if o.down.load(Ordering::SeqCst) {
                    return axum::response::IntoResponse::into_response(axum::http::StatusCode::SERVICE_UNAVAILABLE);
                }
                let start = q["start_seq"].parse::<u64>().unwrap();
                let end = q["end_seq"].parse::<u64>().unwrap().min(top);
                let count = q["count"].parse::<u64>().unwrap();
                let last = end.min(start + count - 1);
                let rows: Vec<Value> = (start..=last).map(ot_row).collect();
                axum::response::IntoResponse::into_response(axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":last < q["end_seq"].parse::<u64>().unwrap()}})))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), o)
}
async fn ot_seeded() -> Arc<AppState> {
    let s = Arc::new(crate::tests::state().await);
    crate::ingestion::ingest_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, ot_trade(100)).await.unwrap();
    s
}
async fn ot_oracle(top: u64) -> Arc<AppState> {
    let s = Arc::new(crate::tests::state().await);
    for n in 100..=top { crate::ingestion::ingest_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, ot_trade(n)).await.unwrap(); }
    s
}
async fn ot_assert_parity(s: &AppState, top: u64) {
    let o = ot_oracle(top).await;
    let (a, b) = (crate::pick_shard(s, Exchange::Coinbase, Market::Usdc, Symbol::Btc), crate::pick_shard(&o, Exchange::Coinbase, Market::Usdc, Symbol::Btc));
    assert_eq!(a.engine.lock().await.last_trade_id.as_deref(), Some(top.to_string().as_str()));
    assert_eq!(m1(a).await, m1(b).await, "exact bars/volume/CVD");
    assert_eq!(*a.cvd_closed_usdt.read().await, *b.cvd_closed_usdt.read().await);
    assert_eq!(a.engine.lock().await.sec.trades, b.engine.lock().await.sec.trades);
}
/// Poll until live; the source must never become terminal and must never be
/// reported live before `want` is ingested. Returns whether an in-flight request
/// was positively observed while polling.
async fn ot_until_live(sh: &crate::Shard, o: &Outage, want: u64, restore_at: Option<std::time::Instant>, secs: u64) -> bool {
    let mut witnessed = false;
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let d = sh.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "transient outage became terminal: {:?}", d.detail);
            witnessed |= o.inflight.load(Ordering::SeqCst) == 1;
            if d.state == "live" {
                let c = sh.engine.lock().await.last_trade_id.clone();
                assert_eq!(c.as_deref(), Some(want.to_string().as_str()), "early live");
                break;
            }
            if restore_at.is_some_and(|t| std::time::Instant::now() >= t) { o.down.store(false, Ordering::SeqCst); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("did not return live; {:?}", sh.recovery.try_read().map(|d| d.detail.clone())));
    witnessed
}

#[tokio::test]
async fn drb_outage_longer_than_episode_resumes_without_restart_or_new_trade() {
    let (base, o) = ot_fixture(10_000).await;
    let s = ot_seeded().await;
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let eth = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Eth);
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base.clone(), 0.0001));
    let (etx, erx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let ew = tokio::spawn(drb::worker(s.clone(), Symbol::Eth, erx, base.clone(), 0.0001));
    // Phase 1: gap 101..105, upstream down 40s (> 30s episode), no further input.
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(105)).await;
    etx.send(t(1, 60_000)).await.unwrap();
    let t0 = std::time::Instant::now();
    let w1 = ot_until_live(sh, &o, 105, Some(t0 + Duration::from_secs(40)), 150).await;
    assert!(t0.elapsed() > Duration::from_secs(40));
    let phase1_calls = o.calls.load(Ordering::SeqCst);
    // Bounded: <=20 attempts per 30s episode (2 pages max when up), capped backoff between.
    assert!(phase1_calls <= 2 * 20 + 4, "phase1 calls {phase1_calls}");
    ot_assert_parity(&s, 105).await;
    assert_eq!(eth.recovery.read().await.state, "live", "other symbol unaffected");

    // Phase 2: second outage under a continuous tail larger than the pending bound.
    o.down.store(true, Ordering::SeqCst);
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(107)).await;
    let top = 107 + crate::recovery::RECOVERY_CHANNEL_CAPACITY as u64 + 900;
    let mut max_buffered = 0;
    for (i, n) in (108..=top).enumerate() {
        crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(n)).await;
        if i.is_multiple_of(256) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let d = sh.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "{:?}", d.detail);
            assert_ne!(d.state, "live");
            max_buffered = max_buffered.max(d.buffered_trades);
        }
    }
    assert!(max_buffered <= crate::recovery::RECOVERY_CHANNEL_CAPACITY + 1, "{max_buffered}");
    let t1 = std::time::Instant::now();
    let w2 = ot_until_live(sh, &o, top, Some(t1 + Duration::from_secs(35)), 200).await;
    assert!(w1 || w2, "positive in-flight witness missing");
    assert_eq!(o.peak.load(Ordering::SeqCst), 1, "at most one active request");
    ot_assert_parity(&s, top).await;

    // Phase 3: socket closes while backing off; a reconnect resumes exactly.
    o.down.store(true, Ordering::SeqCst);
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(top + 5)).await;
    tokio::time::sleep(Duration::from_secs(33)).await;
    assert_eq!(sh.recovery.read().await.state, "recovering");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), w).await.expect("worker leaked on close").unwrap();
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(top + 9)).await;
    ot_until_live(sh, &o, top + 9, Some(std::time::Instant::now() + Duration::from_secs(5)), 120).await;
    ot_assert_parity(&s, top + 9).await;
    assert_eq!(o.peak.load(Ordering::SeqCst), 1);
    drop(tx);
    drop(etx);
    tokio::time::timeout(Duration::from_secs(10), w).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(10), ew).await.unwrap().unwrap();
}

/// R1 socket handoff (DRB): old worker knows 8294 with cursor 100 while history
/// is down; FIFO closes. New worker on the same shard receives only a valid
/// lower prefix 103, history is restored, no higher execution follows. Must not
/// report live below 8294, and must catch up to 8294 without a reset.
#[tokio::test]
async fn drb_socket_handoff_keeps_known_frontier_on_lower_prefix() {
    const FRONTIER: u64 = 8294;
    let (base, o) = ot_fixture(FRONTIER).await;
    let s = ot_seeded().await;
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base.clone(), 0.0001));
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(FRONTIER)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(sh.recovery.read().await.state, "recovering");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), w).await.expect("worker leaked on close").unwrap();
    assert_eq!(sh.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert_eq!(sh.engine.lock().await.drb_known_frontier.as_ref().map(|t| t.id.clone()), Some(FRONTIER.to_string()), "obligation handed to the runtime owner");
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    o.down.store(false, Ordering::SeqCst);
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(103)).await;
    ot_until_live(sh, &o, FRONTIER, None, 120).await;
    ot_assert_parity(&s, FRONTIER).await;
    assert_eq!(o.peak.load(Ordering::SeqCst), 1);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), w).await.unwrap().unwrap();
    assert!(sh.engine.lock().await.drb_known_frontier.is_none(), "obligation released after the worker joins");
}

/// R1b startup frontier (DRB): runtime initialized from history (closed-minute
/// boundary, no ingested cursor). The old worker sees 8294 while history is
/// down and commits nothing; FIFO closes. Successor sees only lower 103 with
/// history restored. Must not go live below 8294; must catch up to 8294.
#[tokio::test]
async fn drb_startup_handoff_keeps_known_frontier_without_cursor() {
    const FRONTIER: u64 = 8294;
    let (base, o) = ot_fixture(FRONTIER).await;
    let s = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    sh.engine.lock().await.recovery_from_ms = Some(ot_ms(100));
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base.clone(), 0.0001));
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(FRONTIER)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(sh.recovery.read().await.state, "recovering");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), w).await.expect("worker leaked on close").unwrap();
    assert_eq!(sh.engine.lock().await.last_trade_id, None, "no replay commit while history was down");
    assert_eq!(sh.engine.lock().await.drb_known_frontier.as_ref().map(|t| t.id.clone()), Some(FRONTIER.to_string()), "startup obligation handed to the runtime owner");
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    o.down.store(false, Ordering::SeqCst);
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(103)).await;
    ot_until_live(sh, &o, FRONTIER, None, 120).await;
    ot_assert_parity(&s, FRONTIER).await;
    assert_eq!(o.peak.load(Ordering::SeqCst), 1);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), w).await.unwrap().unwrap();
    assert!(sh.engine.lock().await.drb_known_frontier.is_none(), "obligation released after the worker joins");
}

#[path = "drb_optional_contracts_tests.rs"]
mod optional_contracts;
