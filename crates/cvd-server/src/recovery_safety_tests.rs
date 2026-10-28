use super::*;

#[tokio::test]
#[ignore = "public-network Coinbase spot worker smoke"]
async fn public_spot_worker_smoke() {
    public_worker_probe().await;
}

async fn public_worker_probe() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let market = Market::Spot;
    let state = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&state, Exchange::Coinbase, market, Symbol::Btc).clone();
    sh.engine.lock().await.recovery_from_ms = Some(chrono::Utc::now().timestamp_millis() - 90_000);
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async("wss://ws-feed.exchange.coinbase.com"),
    )
    .await
    .unwrap()
    .unwrap();
    let (mut write, mut read) = socket.split();
    write
        .send(Message::Text(
            serde_json::json!({"type":"subscribe","product_ids":["BTC-USD"],"channels":["matches"]}).to_string().into(),
        ))
        .await
        .unwrap();
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let mut workers = tokio::task::JoinSet::new();
    // Production Coinbase spot entrypoint (coinbase.rs) with the production REST base.
    workers.spawn(spot::run_spot_worker_with_base(
        state.clone(),
        Symbol::Btc,
        rx,
        "https://api.exchange.coinbase.com".into(),
    ));
    let result: Result<()> = async {
        let deadline=Instant::now()+Duration::from_secs(45);
        loop {
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>anyhow::bail!("spot smoke timed out: {:?}",sh.recovery.read().await),
                msg=read.next()=>{
                    let msg=msg.context("websocket ended")??;
                    let Message::Text(text)=msg else {continue};
                    if let Ok(row)=serde_json::from_str::<crate::models::CoinbaseMatch>(&text) {
                        if row.msg_type=="match" {
                            let trade=base_trade(row.trade_id.to_string(),crate::parse_iso8601_to_ms(&row.time)?,&row.price,&row.size,&row.side,true)?;
                            enqueue_spot(&state,Symbol::Btc,&tx,SpotInput::Trade(trade)).await;
                        }
                    }
                }
                _=tokio::time::sleep(Duration::from_millis(100))=>{
                    let diag=sh.recovery.read().await.clone();
                    anyhow::ensure!(!diag.needs_backfill(),"spot recovery paused: {diag:?}");
                    if diag.state=="live" {break;}
                }
            }
        }
        Ok(())
    }.await;
    drop(tx);
    finish_workers(workers).await.unwrap();
    result.unwrap();
    assert!(sh.engine.lock().await.last_trade_id.is_some());
}

fn t(id: &str, ms: i64) -> Trade {
    Trade { id: id.into(), timestamp_ms: ms, price: 10.0, base: 1.0, quote: 10.0, is_buy: true }
}

#[tokio::test]
async fn exact_replay_ids_survive_same_ms_eviction_but_not_hide_unknown_ids() {
    // Opaque IDs remain a spot-only policy; DRB uses integer sequences.
    let state = crate::tests::state().await;
    let sh = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let rows = (0..4097).map(|i| t(&format!("opaque-{i}"), 1000)).collect::<Vec<_>>();
    for row in &rows {
        ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, row.clone()).await.unwrap();
    }
    sh.engine.lock().await.remember_replay(&rows);
    ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, rows[0].clone()).await.unwrap();
    assert_eq!(sh.engine.lock().await.sec.trades, rows.len() as u64);
    ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, t("unseen", 1000)).await.unwrap();
    assert_eq!(sh.engine.lock().await.sec.trades, rows.len() as u64 + 1);
}

#[tokio::test]
async fn closing_inputs_joins_an_inflight_flush_without_aborting_or_double_counting() {
    let state = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc).clone();
    let (tx, rx) = mpsc::channel(8);
    let mut workers = tokio::task::JoinSet::new();
    workers.spawn(crate::coinbase_perp::drb::worker(
        state.clone(),
        Symbol::Btc,
        rx,
        "http://127.0.0.1:1".into(),
        0.0001,
    ));
    tx.send(t("100", 59001)).await.unwrap();
    tx.send(t("101", 60001)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sh.engine.lock().await.last_trade_id.as_deref() != Some("101") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let block = sh.hist_1m.write().await;
    tx.send(t("102", 61001)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.outbox.pending_count().await.unwrap() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
    let joining = tokio::spawn(finish_workers(workers));
    tokio::task::yield_now().await;
    assert!(!joining.is_finished());
    drop(block);
    tokio::time::timeout(Duration::from_secs(2), joining).await.unwrap().unwrap().unwrap();
    assert_eq!(sh.engine.lock().await.a1.trades, 1);
    assert_eq!(sh.engine.lock().await.last_trade_id.as_deref(), Some("102"));
}

#[tokio::test]
async fn reconnect_drops_committed_sequence_duplicates_without_double_counting() {
    let state = Arc::new(crate::tests::state().await);
    let sh = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    ingest_trade(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc, t("100", 1000)).await.unwrap();
    let (tx, rx) = mpsc::channel(8);
    let worker = tokio::spawn(crate::coinbase_perp::drb::worker(
        state.clone(),
        Symbol::Btc,
        rx,
        "http://127.0.0.1:1".into(),
        0.0001,
    ));
    tx.send(t("100", 1000)).await.unwrap();
    tx.send(t("101", 1001)).await.unwrap();
    drop(tx);
    worker.await.unwrap();
    assert_eq!(sh.engine.lock().await.last_trade_id.as_deref(), Some("101"));
    assert_eq!(sh.engine.lock().await.sec.trades, 2);
}

#[tokio::test]
async fn queue_overflow_is_terminal_and_health_cannot_hide_recovery() {
    use axum::response::IntoResponse;
    let state = Arc::new(crate::tests::state().await);
    state.ready.store(true, std::sync::atomic::Ordering::Release);
    for sh in state.shards.values() {
        *sh.stream_error.write().await = None;
        sh.recovery.write().await.state = "live".into();
        sh.engine.lock().await.last_trade_ms = Some(chrono::Utc::now().timestamp_millis());
    }
    let sh = crate::pick_shard(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let (tx, _rx) = mpsc::channel(1);
    enqueue_trade(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, t("1", 1000)).await;
    enqueue_trade(&state, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, t("2", 1000)).await;
    assert!(sh.recovery.read().await.needs_backfill());
    // Simulate a racing/legacy error clear: the recovery state remains authoritative.
    *sh.stream_error.write().await = None;
    let response = crate::health(axum::extract::State(state.clone())).await.into_response();
    let body = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["healthy"], false);
}

#[test]
fn spot_handoff_rejects_missing_numeric_ids() {
    let anchor = RecoveryAnchor { cursor: Some("100".into()), from_ms: 1000 };
    assert!(validate_spot_handoff(&anchor, &[t("101", 1100), t("104", 1400)]).is_err());
    assert!(validate_spot_handoff(&anchor, &[t("101", 1100), t("102", 1200)]).is_ok());
}

#[test]
fn verified_time_range_allows_an_idle_start_without_a_fake_trade() {
    let mut c = RecoveryCoordinator::new(RecoveryAnchor { cursor: None, from_ms: 1000 }, 8);
    c.push_ws(t("20", 2000)).unwrap();
    assert!(matches!(c.observe_batch(vec![t("10", 1500), t("20", 2000)], Some(1000)), RecoveryDecision::Ready(_)));
}

#[test]
fn stale_rest_cursor_is_retryable_not_a_permanent_gap() {
    let mut c = RecoveryCoordinator::new(RecoveryAnchor { cursor: Some("100".into()), from_ms: 1000 }, 8);
    c.push_ws(t("101", 1100)).unwrap();
    assert!(matches!(c.observe_rest(vec![t("99", 999)]), RecoveryDecision::Retry(_)));
    assert!(matches!(c.observe_rest(vec![t("99", 1000)]), RecoveryDecision::Retry(_)));
}

#[test]
fn rest_never_runs_ahead_of_the_received_websocket_fence() {
    let mut c = RecoveryCoordinator::new(RecoveryAnchor { cursor: Some("a".into()), from_ms: 1000 }, 8);
    c.push_ws(t("overlap", 2000)).unwrap();
    let RecoveryDecision::Ready(rows) = c.observe_rest(vec![t("a", 1000), t("overlap", 2000), t("ahead", 3000)]) else {
        panic!("no overlap")
    };
    assert!(!rows.iter().any(|t| t.id == "ahead"));
}

#[tokio::test]
async fn terminal_source_cannot_keep_ingesting_queued_trades() {
    let state = crate::tests::state().await;
    let sh = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    set_needs_backfill(sh, RecoveryDiagnostics::default(), "test gap".into()).await;
    assert!(ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, t("new", 1000)).await.is_err());
    assert_eq!(sh.engine.lock().await.sec.trades, 0);
}
