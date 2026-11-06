// Characterization for the behavior-preserving recovery cleanup: JSON-RPC
// result ownership (absent vs explicit null), the two authoritative-hole
// producers vs transient producers, the last-attempt cause (a later transient or
// deadline overrides an earlier hole and vice versa), and waiting-phase WS
// admission rejections. HTTP is a loopback replay; workers are production code.
use super::*;

type Q = axum::extract::Query<std::collections::HashMap<String, String>>;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn rpc_route(body: Value) -> axum::routing::MethodRouter {
    axum::routing::get(move || {
        let body = body.clone();
        async move { axum::Json(body) }
    })
}

#[tokio::test]
async fn rpc_result_absent_is_an_error_but_explicit_null_reaches_downstream_validation() {
    let instrument = json!({"instrument_name":"BTC_USDC-PERPETUAL","base_currency":"BTC","quote_currency":"USDC","counter_currency":"USDC","settlement_currency":"USDC","kind":"future","state":"open","future_type":"linear","instrument_type":"linear","is_active":true,"contract_size":0.0001});
    let app = axum::Router::new()
        .route("/absent/get_instrument", rpc_route(json!({"jsonrpc":"2.0"})))
        .route("/null/get_instrument", rpc_route(json!({"jsonrpc":"2.0","result":null})))
        .route("/null/get_last_trades_by_instrument", rpc_route(json!({"jsonrpc":"2.0","result":null})))
        .route("/absent/get_last_trades_by_instrument", rpc_route(json!({"jsonrpc":"2.0"})))
        .route("/ok/get_instrument", rpc_route(json!({"jsonrpc":"2.0","result":instrument.clone()})))
        // Version and error checks still precede the result, even when one is present.
        .route("/version/get_instrument", rpc_route(json!({"jsonrpc":"1.0","result":instrument.clone()})))
        .route("/error/get_instrument", rpc_route(json!({"jsonrpc":"2.0","error":{"code":10028},"result":instrument})));
    let base = serve(app).await;
    let http = reqwest::Client::new();
    let text = |r: std::result::Result<f64, drb::HistoryError>| match r {
        Err(drb::HistoryError::Fatal(e)) => format!("fatal: {e}"),
        Err(drb::HistoryError::Retry(e)) => format!("retry: {e}"),
        Ok(v) => format!("ok: {v}"),
    };
    assert_eq!(text(drb::metadata(&http, &format!("{base}/absent"), Symbol::Btc).await), "fatal: missing DRB RPC result");
    // Explicit null is returned as the result and rejected by the caller's own validation.
    assert_eq!(text(drb::metadata(&http, &format!("{base}/null"), Symbol::Btc).await), "fatal: missing/invalid instrument_name");
    assert_eq!(text(drb::metadata(&http, &format!("{base}/ok"), Symbol::Btc).await), "ok: 0.0001");
    assert_eq!(text(drb::metadata(&http, &format!("{base}/version"), Symbol::Btc).await), "fatal: invalid DRB RPC version");
    assert!(text(drb::metadata(&http, &format!("{base}/error"), Symbol::Btc).await).starts_with("retry: DRB RPC error:"));
    let latest = |r: std::result::Result<Option<Trade>, drb::HistoryError>| match r {
        Err(drb::HistoryError::Fatal(e)) => format!("fatal: {e}"),
        other => format!("{other:?}"),
    };
    assert_eq!(latest(drb::latest(&http, &format!("{base}/absent"), Symbol::Btc, 0.0001).await), "fatal: missing DRB RPC result");
    assert_eq!(latest(drb::latest(&http, &format!("{base}/null"), Symbol::Btc, 0.0001).await), "fatal: missing latest trades");
}

/// Exactly two producers are authoritative holes; every other history Retry is transient.
#[tokio::test]
async fn history_retry_producers_keep_their_messages_and_hole_classification() {
    let name = drb::product(Symbol::Btc);
    let http = reqwest::Client::new();
    let retry = |r: std::result::Result<Vec<Trade>, drb::HistoryError>| match r {
        Err(drb::HistoryError::Retry(e)) => e,
        other => panic!("expected retry, got {other:?}"),
    };
    // Primary: overlap skips 102.
    let (base, _) = fixture([row(101, 1000, name), row(103, 1000, name)].to_vec(), 0).await;
    let e = retry(drb::history(&http, &base, Symbol::Btc, 0.0001, Some(100), None, 103, 1000, (0, 0)).await);
    assert_eq!(e.to_string(), "unproven sequence 102 before 103");
    assert!(e.hole, "primary skip is an authoritative hole");
    // Primary: not yet visible / lagging are transient.
    let (base, _) = fixture(vec![], 0).await;
    let e = retry(drb::history(&http, &base, Symbol::Btc, 0.0001, Some(100), None, 103, 1000, (0, 0)).await);
    assert_eq!(e.to_string(), "REST sequence range not yet visible");
    assert!(!e.hole);
    let (base, _) = scripted(Arc::new(|_| (Duration::ZERO, vec![101]))).await; // has_more=false short of target
    let e = retry(drb::history(&http, &base, Symbol::Btc, 0.0001, Some(100), None, 103, 1000, (0, 0)).await);
    assert_eq!(e.to_string(), "REST has_more=false before fixed target; history lagging");
    assert!(!e.hole);
    // Archive chunk: the first requested interval is missing.
    let (archive, _) = fixture((103..=110).map(|s| row(s, 1000 + s as i64, name)).collect(), 0).await;
    let e = retry(drb::archive_chunk(&http, &archive, Symbol::Btc, 0.0001, Some(100), Some(1000), 111, 999_999_999).await);
    assert_eq!(e.to_string(), "archive missing sequence interval 101-102");
    assert!(e.hole, "archive skip is an authoritative hole");
    // Transport/HTTP retries stay transient.
    let e = retry(drb::history(&http, "http://127.0.0.1:1", Symbol::Btc, 0.0001, Some(100), None, 103, 1000, (0, 0)).await);
    assert!(!e.hole);
}

/// Scripted primary: `script(call)` -> (delay, rows) for the 101.. sequence range.
type Script = Arc<dyn Fn(usize) -> (Duration, Vec<u64>) + Send + Sync>;
async fn scripted(script: Script) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let app = axum::Router::new().route(
        "/get_last_trades_by_instrument",
        axum::routing::get(move |axum::extract::Query(q): Q| {
            let (c, script) = (c.clone(), script.clone());
            async move {
                let (delay, seqs) = script(c.fetch_add(1, Ordering::SeqCst));
                tokio::time::sleep(delay).await;
                let end = q["end_seq"].parse::<u64>().unwrap();
                let rows: Vec<Value> = seqs.into_iter().filter(|s| *s <= end).map(ot_row).collect();
                axum::Json(json!({"jsonrpc":"2.0","result":{"trades":rows,"has_more":false}}))
            }
        }),
    );
    (serve(app).await, calls)
}

/// Run one BTC worker from cursor 100 with a gap to 105 and report the first
/// episode's outcome: Ok(detail) while still recovering after the episode, or
/// Err(detail) once terminal.
async fn first_episode(script: Script) -> std::result::Result<String, String> {
    let (base, _) = scripted(script).await;
    let s = ot_seeded().await;
    let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
    let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
    let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
    crate::recovery::enqueue_trade(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc, &tx, ot_trade(105)).await;
    let out = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let d = sh.recovery.read().await.clone();
            let detail = d.detail.clone().unwrap_or_default();
            if d.needs_backfill() {
                break Err(detail);
            }
            assert_ne!(d.state, "live", "gap must not go live");
            if detail.contains("temporarily unavailable") {
                break Ok(detail);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("first bounded episode did not end");
    assert_eq!(sh.engine.lock().await.last_trade_id.as_deref(), Some("100"), "never past the gap");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(20), w).await.expect("worker did not stop after its FIFO closed").expect("worker panicked");
    out
}

#[tokio::test]
async fn drb_later_transient_overrides_an_earlier_hole() {
    let out = first_episode(Arc::new(|n| (Duration::ZERO, if n == 0 { vec![101, 103] } else { vec![] }))).await;
    let detail = out.expect("a later transient attempt must keep the episode transient");
    assert!(detail.contains("REST sequence range not yet visible"), "{detail}");
}

#[tokio::test]
async fn drb_later_hole_overrides_an_earlier_transient() {
    let out = first_episode(Arc::new(|n| (Duration::ZERO, if n == 0 { vec![] } else { vec![101, 103] }))).await;
    let detail = out.expect_err("a hole on the last attempt stays terminal");
    assert!(detail.contains("budget exhausted") && detail.contains("unproven sequence 102 before 103"), "{detail}");
}

#[tokio::test]
async fn drb_deadline_or_timeout_overrides_an_earlier_hole() {
    // After the hole, each request is held ~14s (< 15s client timeout); the
    // episode ends by its deadline or a held empty answer: never terminal.
    let out =
        first_episode(Arc::new(|n| if n == 0 { (Duration::ZERO, vec![101, 103]) } else { (Duration::from_secs(14), vec![]) }))
            .await;
    let detail = out.expect("a last attempt that timed out is transient");
    assert!(!detail.contains("unproven sequence"), "{detail}");
}

/// Waiting-phase WS admission: identity conflicts and invalid sequence IDs are
/// still rejected with their existing diagnostics, in flight and in cooldown.
#[tokio::test]
async fn drb_waiting_phase_admission_rejections_are_unchanged() {
    for (phase, bad, want) in [
        ("in-flight", "conflict", "conflicting DRB sequence 105"),
        ("in-flight", "invalid", "invalid buffered sequence"),
        ("cooldown", "conflict", "conflicting DRB sequence 105"),
        ("cooldown", "invalid", "invalid buffered sequence"),
    ] {
        let hold = if phase == "in-flight" { Duration::from_secs(10) } else { Duration::ZERO };
        let (base, calls) = scripted(Arc::new(move |_| (hold, vec![]))).await;
        let s = ot_seeded().await;
        let sh = crate::pick_shard(&s, Exchange::Coinbase, Market::Usdc, Symbol::Btc);
        let (tx, rx) = mpsc::channel(crate::recovery::RECOVERY_CHANNEL_CAPACITY);
        let w = tokio::spawn(drb::worker(s.clone(), Symbol::Btc, rx, base, 0.0001));
        tx.send(ot_trade(105)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let detail = sh.recovery.read().await.detail.clone().unwrap_or_default();
                let ready = match phase {
                    "in-flight" => calls.load(Ordering::SeqCst) == 1,
                    _ => detail.contains("temporarily unavailable"),
                };
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let input = if bad == "conflict" {
            let mut t = ot_trade(105);
            t.price += 1.0;
            t
        } else {
            let mut t = ot_trade(106);
            t.id = "x".into();
            t
        };
        tx.send(input).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !sh.recovery.read().await.needs_backfill() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{phase}/{bad}: not rejected"));
        let detail = sh.recovery.read().await.detail.clone().unwrap_or_default();
        assert_eq!(detail, want, "{phase}/{bad}");
        assert_eq!(sh.engine.lock().await.last_trade_id.as_deref(), Some("100"));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
    }
}
