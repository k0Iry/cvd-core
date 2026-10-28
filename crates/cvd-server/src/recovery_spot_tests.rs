use super::spot::run_spot_worker_with_fetch;
use super::*;

fn t(id: u64) -> Trade {
    Trade { id: id.to_string(), timestamp_ms: 60_000 + id as i64, price: 10.0, base: 1.0, quote: 10.0, is_buy: true }
}

async fn heartbeat_origin_case(restart: bool, heartbeat_only: bool) {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    if restart {
        let (tx, rx) = mpsc::channel(8);
        let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, |_| async {
            panic!("first origin heartbeat must not fetch");
            #[allow(unreachable_code)]
            Ok(vec![])
        }));
        tx.send(SpotInput::Heartbeat(100)).await.unwrap();
        drop(tx);
        worker.await.unwrap();
        assert!(shard.engine.lock().await.last_trade_id.is_none());
        assert_eq!(shard.engine.lock().await.sec.trades, 0);
    }
    let observed = calls.clone();
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |anchor| {
        assert_eq!(anchor.cursor.as_deref(), Some("100"));
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async { Ok(vec![t(100), t(101), t(102)]) }
    }));
    if !restart {
        tx.send(SpotInput::Heartbeat(100)).await.unwrap();
    }
    tx.send(if heartbeat_only { SpotInput::Heartbeat(102) } else { SpotInput::Trade(t(102)) }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while shard.engine.lock().await.last_trade_id.as_deref() != Some("102") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.sec.trades, 2, "only 101 and 102 belong after the heartbeat origin");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(shard.recovery.read().await.state, "live");
}

#[tokio::test]
async fn first_heartbeat_cannot_mask_missing_first_match_or_tail() {
    heartbeat_origin_case(false, false).await;
    heartbeat_origin_case(false, true).await;
}

#[tokio::test]
async fn heartbeat_origin_survives_worker_socket_restart() {
    heartbeat_origin_case(true, false).await;
    heartbeat_origin_case(true, true).await;
}

#[tokio::test]
async fn repeated_bootstrap_heartbeats_do_not_move_origin_or_ingest_old_matches() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, |_| async {
        panic!("no forward gap");
        #[allow(unreachable_code)]
        Ok(vec![])
    }));
    for input in [
        SpotInput::Heartbeat(100),
        SpotInput::Heartbeat(99),
        SpotInput::Heartbeat(100),
        SpotInput::Trade(t(99)),
        SpotInput::Trade(t(100)),
        SpotInput::Trade(t(101)),
        SpotInput::Heartbeat(101),
    ] {
        tx.send(input).await.unwrap();
    }
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.sec.trades, 1);
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("101"));
}

#[tokio::test]
async fn live_gap_freezes_before_trigger_and_replays_exact_sequence() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async { Ok(vec![t(100), t(101), t(102), t(103)]) }
    }));
    for id in [100, 102, 103, 103, 101] {
        tx.send(SpotInput::Trade(t(id))).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while shard.engine.lock().await.last_trade_id.as_deref() != Some("103") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    let engine = shard.engine.lock().await;
    assert_eq!(engine.sec.trades, 4);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(shard.recovery.read().await.state, "live");
}

#[tokio::test]
async fn heartbeat_only_recovers_tail_and_old_heartbeat_does_not_refetch() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async { Ok(vec![t(100), t(101), t(102)]) }
    }));
    tx.send(SpotInput::Trade(t(100))).await.unwrap();
    tx.send(SpotInput::Heartbeat(102)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while shard.engine.lock().await.last_trade_id.as_deref() != Some("102") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tx.send(SpotInput::Heartbeat(102)).await.unwrap();
    tx.send(SpotInput::Heartbeat(99)).await.unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.sec.trades, 3);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn production_socket_and_rest_loopback_repair_live_gap_without_stopping_eth() {
    production_socket_gap_case(false).await;
}

#[tokio::test]
async fn production_socket_heartbeat_first_cannot_skip_missing_first_match() {
    production_socket_gap_case(true).await;
}

async fn production_socket_gap_case(heartbeat_first: bool) {
    use futures::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
    let state = Arc::new(crate::tests::state().await);
    let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc).clone();
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Eth).clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let rest_hits = hits.clone();
    let app = axum::Router::new().route(
        "/products/BTC-USD/trades",
        axum::routing::get(move || {
            let hits = rest_hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(
                    (100..=103)
                        .rev()
                        .map(|id| {
                            serde_json::json!({
                                "trade_id": id, "time": format!("1970-01-01T00:01:00.{id:03}Z"),
                                "price": "10", "size": "1", "side": "sell"
                            })
                        })
                        .collect::<Vec<_>>(),
                )
            }
        }),
    );
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    let rest_server = tokio::spawn(async move { axum::serve(rest, app).await.unwrap() });
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", ws.local_addr().unwrap());
    let (advance_tx, advance_rx) = tokio::sync::oneshot::channel();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = ws.accept().await.unwrap();
        let mut socket = accept_async(socket).await.unwrap();
        let Message::Text(subscription) = socket.next().await.unwrap().unwrap() else { panic!("subscription missing") };
        let sub: serde_json::Value = serde_json::from_str(&subscription).unwrap();
        assert_eq!(sub["channels"], serde_json::json!(["matches", "heartbeat"]));
        let send = |product: &str, id: u64| {
            Message::Text(
                serde_json::json!({
                    "type": "match", "product_id": product, "trade_id": id,
                    "time": format!("1970-01-01T00:01:00.{id:03}Z"),
                    "price": "10", "size": "1", "side": "sell"
                })
                .to_string()
                .into(),
            )
        };
        if heartbeat_first {
            socket
                .send(Message::Text(
                    serde_json::json!({"type":"heartbeat","product_id":"BTC-USD","last_trade_id":100})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        } else {
            socket.send(send("BTC-USD", 100)).await.unwrap();
        }
        socket.send(send("ETH-USD", 200)).await.unwrap();
        advance_rx.await.unwrap();
        socket.send(send("BTC-USD", 102)).await.unwrap();
        socket.send(send("BTC-USD", 103)).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::json!({"type":"heartbeat","product_id":"BTC-USD","last_trade_id":103}).to_string().into(),
            ))
            .await
            .unwrap();
        close_rx.await.unwrap();
        socket.close(None).await.unwrap();
    });
    let (socket, _) = connect_async(url).await.unwrap();
    let (write, read) = socket.split();
    let worker =
        tokio::spawn(crate::coinbase::run_coinbase_spot_market_engine_with_base(state.clone(), write, read, base));
    if !heartbeat_first {
        tokio::time::timeout(Duration::from_secs(3), async {
            while btc.engine.lock().await.last_trade_id.as_deref() != Some("100") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    advance_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while btc.engine.lock().await.last_trade_id.as_deref() != Some("103")
            || btc.recovery.read().await.state != "live"
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(btc.engine.lock().await.sec.trades, if heartbeat_first { 3 } else { 4 });
    assert_eq!(eth.engine.lock().await.last_trade_id.as_deref(), Some("200"));
    assert_eq!(btc.recovery.read().await.state, "live");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    close_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), worker).await.unwrap().unwrap().unwrap_err();
    server.await.unwrap();
    rest_server.abort();
}

#[tokio::test]
async fn continuous_and_repeated_inputs_never_fetch_history() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, |_| async {
        panic!("contiguous live traffic must never request history");
        #[allow(unreachable_code)]
        Ok(Vec::new())
    }));
    for input in [
        SpotInput::Trade(t(100)),
        SpotInput::Heartbeat(100),
        SpotInput::Trade(t(101)),
        SpotInput::Heartbeat(99),
        SpotInput::Trade(t(100)),
        SpotInput::Trade(t(101)),
    ] {
        tx.send(input).await.unwrap();
    }
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.sec.trades, 2);
    assert_eq!(shard.recovery.read().await.state, "live");
}

#[tokio::test]
async fn lagging_rest_and_new_live_trades_keep_fixed_target_and_replay_once() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = calls.clone();
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |anchor| {
        assert_eq!(anchor.cursor.as_deref(), Some("100"));
        let attempt = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            if attempt == 0 {
                Ok(vec![t(100), t(101), t(102)])
            } else {
                Ok((100..=105).map(t).collect())
            }
        }
    }));
    tx.send(SpotInput::Trade(t(100))).await.unwrap();
    tx.send(SpotInput::Trade(t(103))).await.unwrap();
    tx.send(SpotInput::Trade(t(104))).await.unwrap();
    tx.send(SpotInput::Trade(t(105))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while shard.engine.lock().await.last_trade_id.as_deref() != Some("105")
            || shard.recovery.read().await.state != "live"
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.sec.trades, 6);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn buffer_overflow_is_terminal_and_only_pauses_its_symbol() {
    let state = Arc::new(crate::tests::state().await);
    let btc = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Eth);
    let (tx, _rx) = mpsc::channel(1);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(100))).await;
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Heartbeat(102)).await;
    assert!(btc.recovery.read().await.needs_backfill());
    assert!(!eth.recovery.read().await.needs_backfill());
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(103))).await;
    assert_eq!(tx.capacity(), 0);
}

#[tokio::test]
async fn closing_input_cancels_pending_rest_without_advancing_cursor() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, |_| {
        std::future::pending::<Result<Vec<Trade>>>()
    }));
    tx.send(SpotInput::Trade(t(100))).await.unwrap();
    tx.send(SpotInput::Heartbeat(102)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while shard.recovery.read().await.state != "recovering" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(2), worker).await.unwrap().unwrap();
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
}

#[tokio::test]
async fn heartbeat_advancing_during_fetch_uses_same_recovery_episode_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let calls = Arc::new(AtomicUsize::new(0));
    let fetch_calls = calls.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started = Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |anchor| {
        let n = fetch_calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            started.lock().unwrap().take().unwrap().send(()).unwrap();
        }
        let release = release.clone();
        async move {
            if n == 0 {
                release.lock().await.take().unwrap().await.unwrap();
            }
            assert_eq!(anchor.cursor.as_deref(), if n == 0 { Some("100") } else { Some("102") });
            Ok((100..=if n == 0 { 102 } else { 103 }).map(t).collect())
        }
    }));
    tx.send(SpotInput::Trade(t(100))).await.unwrap();
    tx.send(SpotInput::Heartbeat(102)).await.unwrap();
    started_rx.await.unwrap();
    tx.send(SpotInput::Heartbeat(103)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while tx.capacity() != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while shard.recovery.read().await.state != "live"
            || shard.engine.lock().await.last_trade_id.as_deref() != Some("103")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "{error}; diagnostics={:?}; cursor={:?}; calls={}",
            shard.recovery.try_read().ok(),
            shard.engine.try_lock().ok().and_then(|e| e.last_trade_id.clone()),
            calls.load(Ordering::SeqCst)
        )
    });
    drop(tx);
    worker.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(shard.recovery.read().await.attempts, 2);
    assert_eq!(shard.engine.lock().await.sec.trades, 4);
}

#[tokio::test]
async fn missing_authoritative_cursor_enters_terminal_backfill_without_advancing() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    {
        let mut engine = shard.engine.lock().await;
        engine.last_trade_id = Some("100".into());
        engine.last_trade_ms = Some(t(100).timestamp_ms);
    }
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, |_| async {
        Ok(vec![t(104), t(105)])
    }));
    tx.send(SpotInput::Trade(t(105))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !shard.recovery.read().await.needs_backfill() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert!(shard.stream_error.read().await.as_deref().unwrap().contains("needs_backfill"));
}

#[tokio::test]
async fn exhausted_rest_attempts_pause_without_silent_resume() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    {
        let mut engine = shard.engine.lock().await;
        engine.last_trade_id = Some("100".into());
        engine.last_trade_ms = Some(t(100).timestamp_ms);
    }
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, |_| async { Ok(Vec::new()) }));
    tx.send(SpotInput::Heartbeat(101)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(25), async {
        while !shard.recovery.read().await.needs_backfill() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    worker.await.unwrap();
    let diagnostics = shard.recovery.read().await;
    assert_eq!(diagnostics.attempts, RECOVERY_ATTEMPTS);
    assert!(diagnostics.detail.as_deref().unwrap().contains("budget exhausted"));
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
}
