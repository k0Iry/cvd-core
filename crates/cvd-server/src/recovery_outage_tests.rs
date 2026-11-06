//! Coinbase spot: a temporary history outage longer than one bounded recovery
//! episode must stay recoverable (capped backoff, bounded memory) and resume
//! automatically once history returns, while genuine holes stay terminal.
use super::spot::run_spot_worker_with_fetch;
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

fn t(id: u64) -> Trade {
    Trade { id: id.to_string(), timestamp_ms: 60_000 + id as i64, price: 10.0 + (id % 7) as f64, base: 1.0 + (id % 3) as f64, quote: 0.0, is_buy: id.is_multiple_of(2) }
        .with_quote()
}
trait Q {
    fn with_quote(self) -> Self;
}
impl Q for Trade {
    fn with_quote(mut self) -> Self {
        self.quote = self.price * self.base;
        self
    }
}

type Row = (i64, u64, i64, i64, i64);
async fn m1(shard: &crate::Shard) -> Vec<Row> {
    shard
        .for_tf(crate::types::Tf::M1)
        .0
        .read()
        .await
        .iter()
        .map(|b| (b.time, b.trades, (b.delta_usdt * 1e6).round() as i64, (b.buy_volume_base * 1e9).round() as i64, (b.cvd_usdt * 1e6).round() as i64))
        .collect()
}

struct Upstream {
    down: AtomicBool,
    hole: Option<u64>,
    top: AtomicUsize,
    calls: AtomicUsize,
    inflight: AtomicUsize,
    peak: AtomicUsize,
}

fn spawn_worker(
    state: Arc<AppState>,
    up: Arc<Upstream>,
    rx: mpsc::Receiver<SpotInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_spot_worker_with_fetch(state, Symbol::Btc, rx, move |anchor| {
        let up = up.clone();
        async move {
            up.calls.fetch_add(1, SeqCst);
            let now = up.inflight.fetch_add(1, SeqCst) + 1;
            up.peak.fetch_max(now, SeqCst);
            // Decrement on completion OR cancellation (dropped at an episode deadline).
            struct Done(Arc<Upstream>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.inflight.fetch_sub(1, SeqCst);
                }
            }
            let guard = Done(up.clone());
            tokio::time::sleep(Duration::from_millis(200)).await; // held in-flight request
            drop(guard);
            if up.down.load(SeqCst) {
                // Every request issued during the outage is anchored at the unchanged cursor.
                // (After restoration a handed-over frontier may legitimately chain a
                // second recovery from the newly committed cursor.)
                assert_eq!(anchor.cursor.as_deref(), Some("100"), "cursor must never move during an outage");
                // A genuine reqwest transport error (connection refused), like the incident.
                let e = reqwest::Client::new().get("http://127.0.0.1:1/").send().await.unwrap_err();
                return Err(e.into());
            }
            let top = up.top.load(SeqCst) as u64;
            Ok((100..=top).filter(|n| Some(*n) != up.hole).map(t).collect())
        }
    }))
}

async fn seeded() -> Arc<AppState> {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    // The committed prefix through 100 is ingested exactly like the oracle.
    ingest_trade(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, t(100)).await.unwrap();
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    // Virtual time only after the SQLite pool exists (a paused clock starves its open).
    tokio::time::pause();
    state
}

async fn oracle(top: u64) -> Arc<AppState> {
    tokio::time::resume();
    let s = Arc::new(crate::tests::state().await);
    for n in 100..=top {
        ingest_trade(&s, Exchange::Coinbase, Market::Spot, Symbol::Btc, t(n)).await.unwrap();
    }
    s
}

/// Acceptance 1 (spot): outage > 30s episode, upstream restored, no new input.
#[tokio::test]
async fn spot_outage_longer_than_episode_resumes_without_new_input() {
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let up = Arc::new(Upstream {
        down: AtomicBool::new(true),
        hole: None,
        top: AtomicUsize::new(103),
        calls: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let worker = spawn_worker(state.clone(), up.clone(), rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(103))).await;
    let start = Instant::now();
    let mut restored = false;
    let mut witnessed_inflight = false;
    let mut first_episode_calls = None;
    tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let d = shard.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "transient outage became terminal at {:?}: {:?}", start.elapsed(), d.detail);
            witnessed_inflight |= up.inflight.load(SeqCst) == 1;
            if d.state == "live" {
                assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("103"), "early live");
                break;
            }
            if first_episode_calls.is_none() && d.detail.as_deref().is_some_and(|x| x.contains("episode 1 ended")) {
                first_episode_calls = Some(up.calls.load(SeqCst));
            }
            if !restored && start.elapsed() >= Duration::from_secs(45) {
                up.down.store(false, SeqCst); // upstream restored; no further input is sent
                restored = true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("spot source never resumed after the outage ended");
    assert!(restored);
    assert!(witnessed_inflight, "positive in-flight witness missing");
    assert_eq!(up.peak.load(SeqCst), 1, "at most one history request in flight");
    assert!(first_episode_calls.unwrap() <= RECOVERY_ATTEMPTS, "first episode keeps the unchanged attempt cap");
    let e = shard.engine.lock().await;
    assert_eq!(e.last_trade_id.as_deref(), Some("103"));
    drop(e);
    let o = oracle(103).await;
    let os = crate::pick_shard(&o, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    assert_eq!(m1(shard).await, m1(os).await);
    assert_eq!(shard.engine.lock().await.sec.trades, os.engine.lock().await.sec.trades);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), worker).await.expect("worker leaked").unwrap();
}

/// Acceptance 2-4 (spot): long outage under continued input stays bounded,
/// re-attempts without busy-looping, never goes live early, and replays the
/// exact sequence set after restoration (oracle parity).
#[tokio::test]
async fn spot_long_outage_under_continuous_input_is_bounded_then_exact() {
    const TOP: u64 = 100 + 3 * RECOVERY_CHANNEL_CAPACITY as u64;
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let eth = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Eth);
    let up = Arc::new(Upstream {
        down: AtomicBool::new(true),
        hole: None,
        top: AtomicUsize::new(TOP as usize),
        calls: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let worker = spawn_worker(state.clone(), up.clone(), rx);
    // Other symbol keeps flowing independently.
    let (etx, erx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let eth_worker = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Eth, erx, |_| async {
        anyhow::bail!("ETH never needs history in this test")
    }));
    let start = Instant::now();
    let mut witnessed = 0usize;
    let mut max_buffered = 0usize;
    let mut eth_id = 1_000u64;
    // Gap at 101/102; then a continuous live tail through TOP during the outage.
    for (i, n) in (103..=TOP).enumerate() {
        enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(n))).await;
        if i.is_multiple_of(16) {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if up.inflight.load(SeqCst) == 1 {
                witnessed += 1;
            }
            let d = shard.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "outage became terminal: {:?}", d.detail);
            assert_ne!(d.state, "live", "live during outage");
            max_buffered = max_buffered.max(d.buffered_trades);
        }
        if i.is_multiple_of(64) {
            eth_id += 1; // contiguous ETH executions: no ETH gap
            enqueue_spot(&state, Symbol::Eth, &etx, SpotInput::Trade(t(eth_id))).await;
        }
    }
    // Keep the outage going well past several episodes with no further input.
    while start.elapsed() < Duration::from_secs(900) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let d = shard.recovery.read().await.clone();
        assert!(!d.needs_backfill(), "outage became terminal: {:?}", d.detail);
        assert_eq!(d.state, "recovering");
        max_buffered = max_buffered.max(d.buffered_trades);
    }
    let calls = up.calls.load(SeqCst);
    // 900s of outage: re-attempting (several episodes) but bounded, not busy:
    // <= 20 attempts per episode, and episodes separated by capped backoff.
    assert!(calls > RECOVERY_ATTEMPTS, "must keep re-attempting after the first episode: {calls}");
    assert!(calls <= 20 * 20, "retry frequency must be bounded: {calls} calls in 900s");
    assert!(max_buffered <= RECOVERY_CHANNEL_CAPACITY, "buffer {max_buffered}");
    assert!(witnessed > 0, "positive in-flight witness missing");
    assert_eq!(up.peak.load(SeqCst), 1);
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert_eq!(eth.recovery.read().await.state, "live", "other symbol continues");
    up.down.store(false, SeqCst);
    tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let d = shard.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "{:?}", d.detail);
            if d.state == "live" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("did not resume after restoration");
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some(TOP.to_string().as_str()), "known frontier kept");
    let o = oracle(TOP).await;
    let os = crate::pick_shard(&o, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    assert_eq!(m1(shard).await, m1(os).await, "exact bars/volume/CVD");
    assert_eq!(*shard.cvd_closed_usdt.read().await, *os.cvd_closed_usdt.read().await);
    assert_eq!(shard.engine.lock().await.sec.trades, os.engine.lock().await.sec.trades);
    drop(tx);
    drop(etx);
    tokio::time::timeout(Duration::from_secs(5), worker).await.expect("worker leaked").unwrap();
    tokio::time::timeout(Duration::from_secs(5), eth_worker).await.expect("eth worker leaked").unwrap();
}

/// Socket close while backing off, then reconnect: the new worker resumes from
/// the durable cursor; nothing is lost or double counted.
#[tokio::test]
async fn spot_reconnect_during_outage_backoff_resumes_exactly() {
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let up = Arc::new(Upstream {
        down: AtomicBool::new(true),
        hole: None,
        top: AtomicUsize::new(110),
        calls: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w1 = spawn_worker(state.clone(), up.clone(), rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(105))).await;
    tokio::time::sleep(Duration::from_secs(35)).await;
    assert_eq!(shard.recovery.read().await.state, "recovering");
    drop(tx); // socket closes mid-outage
    tokio::time::timeout(Duration::from_secs(5), w1).await.expect("worker leaked on close").unwrap();
    assert!(!shard.recovery.read().await.needs_backfill());
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w2 = spawn_worker(state.clone(), up.clone(), rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Heartbeat(110)).await;
    tokio::time::sleep(Duration::from_secs(40)).await;
    up.down.store(false, SeqCst);
    tokio::time::timeout(Duration::from_secs(300), async {
        while shard.recovery.read().await.state != "live" {
            assert!(!shard.recovery.read().await.needs_backfill());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("did not resume after reconnect");
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("110"));
    let o = oracle(110).await;
    let os = crate::pick_shard(&o, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    assert_eq!(m1(shard).await, m1(os).await);
    assert_eq!(up.peak.load(SeqCst), 1);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w2).await.unwrap().unwrap();
}

/// Negative control: an authoritative ID hole (REST overlaps but skips an ID)
/// is not a transient outage and still ends terminal after the bounded episode.
#[tokio::test]
async fn spot_authoritative_hole_remains_terminal() {
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let up = Arc::new(Upstream {
        down: AtomicBool::new(false),
        hole: Some(102),
        top: AtomicUsize::new(104),
        calls: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w = spawn_worker(state.clone(), up.clone(), rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(104))).await;
    tokio::time::timeout(Duration::from_secs(120), async {
        while !shard.recovery.read().await.needs_backfill() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("authoritative hole must stay terminal");
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert!(up.calls.load(SeqCst) <= RECOVERY_ATTEMPTS);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
}

/// Classification goes through the real HTTP client/parser, not injected errors.
#[tokio::test]
async fn spot_fetch_errors_classified_through_real_http_parser() {
    use super::spot::{classify_fetch_error, AttemptEnd};
    use axum::{http::StatusCode, routing::get, Router};
    let app = Router::new()
        .route("/s503/products/BTC-USD/trades", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
        .route("/s429/products/BTC-USD/trades", get(|| async { StatusCode::TOO_MANY_REQUESTS }))
        .route("/s404/products/BTC-USD/trades", get(|| async { StatusCode::NOT_FOUND }))
        .route("/bad/products/BTC-USD/trades", get(|| async { "{not json" }))
        .route("/row/products/BTC-USD/trades", get(|| async {
            axum::Json(serde_json::json!([{"trade_id":101,"time":"not-a-time","price":"1","size":"1","side":"buy"}]))
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let state = crate::tests::state().await;
    let anchor = RecoveryAnchor { cursor: Some("100".into()), from_ms: 0 };
    let mut out = Vec::new();
    for (path, want) in [
        ("s503", AttemptEnd::Transient),
        ("s429", AttemptEnd::Transient),
        ("s404", AttemptEnd::Invalid),
        ("bad", AttemptEnd::Invalid),
        ("row", AttemptEnd::Invalid),
    ] {
        let e = fetch_recent_with_spot_base(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, &anchor, &format!("http://{addr}/{path}"))
            .await
            .unwrap_err();
        out.push((path, classify_fetch_error(&e), want));
    }
    // Connection refused (closed port) is a genuine transport outage.
    let e = fetch_recent_with_spot_base(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc, &anchor, "http://127.0.0.1:1")
        .await
        .unwrap_err();
    out.push(("refused", classify_fetch_error(&e), AttemptEnd::Transient));
    for (p, got, want) in out {
        assert_eq!(got, want, "{p}");
    }
}

/// Non-retryable/malformed authoritative responses through the actual worker
/// still end terminal after the unchanged bounded episode (no eternal retry).
#[tokio::test(flavor = "current_thread")]
async fn spot_invalid_response_remains_terminal_through_worker() {
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let w = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_| {
        c.fetch_add(1, SeqCst);
        async { Err(anyhow::anyhow!("invalid replay trade side")) } // validation error, not reqwest
    }));
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(104))).await;
    tokio::time::timeout(Duration::from_secs(120), async {
        while !shard.recovery.read().await.needs_backfill() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("invalid authoritative data must stay terminal");
    assert!(calls.load(SeqCst) <= RECOVERY_ATTEMPTS);
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    drop(tx);
    w.await.unwrap();
}

/// Requests that never return (each episode ends by its deadline) keep the
/// diagnostic bounded (no recursive nesting) and the source non-live; a hole
/// seen earlier does not decide the category once the last attempt timed out.
#[tokio::test]
async fn spot_timeout_only_episodes_keep_bounded_diagnostics() {
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let w = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_| {
        let n = c.fetch_add(1, SeqCst);
        async move {
            if n == 0 {
                // First attempt: an authoritative hole (100,101,103,104 vs buffered 104).
                return Ok([100, 101, 103, 104].into_iter().map(t).collect());
            }
            std::future::pending::<()>().await;
            unreachable!()
        }
    }));
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(104))).await;
    let mut max_len = 0;
    let mut episodes_seen = 0;
    for _ in 0..1200 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let d = shard.recovery.read().await.clone();
        assert!(!d.needs_backfill(), "{:?}", d.detail);
        assert_eq!(d.state, "recovering");
        let detail = d.detail.unwrap_or_default();
        max_len = max_len.max(detail.len());
        if let Some(i) = detail.find("episode ") {
            episodes_seen = episodes_seen.max(detail[i + 8..].split(' ').next().unwrap().parse::<u32>().unwrap_or(0));
        }
        assert!(detail.matches("temporarily unavailable").count() <= 1, "nested diagnostic: {detail}");
    }
    assert!(episodes_seen >= 5, "kept re-attempting: {episodes_seen}");
    assert!(max_len < 600, "diagnostic grew to {max_len}");
    assert!(calls.load(SeqCst) <= episodes_seen as usize + 2, "one in-flight request per episode");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
}

/// After coalescing to the required-through frontier H, a stale lower duplicate
/// left alone in the buffer must not let the handoff stop below H (no early live).
#[tokio::test]
async fn spot_coalesced_frontier_not_undercut_by_stale_lower_duplicate() {
    let h = 102 + RECOVERY_CHANNEL_CAPACITY as u64; // buffer holds 103..=h when full
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let up = Arc::new(Upstream {
        down: AtomicBool::new(true),
        hole: None,
        top: AtomicUsize::new(h as usize),
        calls: AtomicUsize::new(0),
        inflight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w = spawn_worker(state.clone(), up.clone(), rx);
    for (i, n) in (103..=h).enumerate() {
        enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(n))).await;
        if i.is_multiple_of(64) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Stale duplicate of an already-buffered lower execution triggers coalescing
    // and is then the only buffered identity.
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(103))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    up.down.store(false, SeqCst);
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let d = shard.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "{:?}", d.detail);
            if d.state == "live" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("never live");
    assert_eq!(
        shard.engine.lock().await.last_trade_id.as_deref(),
        Some(h.to_string().as_str()),
        "reported live below the known required-through frontier"
    );
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
}

/// R1 socket handoff: the old worker knows frontier 8294 while the committed
/// cursor is 100 and history is unavailable; its FIFO closes. The new worker on
/// the same shard sees only a valid lower prefix (100 < 103 < 8294), history is
/// restored, and no higher execution/heartbeat ever arrives. The source must not
/// be reported live below 8294 and must catch up to 8294 without a reset.
#[tokio::test]
async fn spot_socket_handoff_keeps_known_frontier_on_lower_prefix() {
    const FRONTIER: u64 = 8294;
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let down = Arc::new(AtomicBool::new(true));
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let spawn = |rx| {
        let (down, inflight, peak) = (down.clone(), inflight.clone(), peak.clone());
        tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_anchor| {
            let (down, inflight, peak) = (down.clone(), inflight.clone(), peak.clone());
            async move {
                peak.fetch_max(inflight.fetch_add(1, SeqCst) + 1, SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                inflight.fetch_sub(1, SeqCst);
                if down.load(SeqCst) {
                    let e = reqwest::Client::new().get("http://127.0.0.1:1/").send().await.unwrap_err();
                    return Err(e.into());
                }
                Ok((100..=FRONTIER).map(t).collect())
            }
        }))
    };
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w1 = spawn(rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(FRONTIER))).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(shard.recovery.read().await.state, "recovering");
    drop(tx); // FIFO/socket replaced while recovering
    tokio::time::timeout(Duration::from_secs(5), w1).await.expect("worker leaked on close").unwrap();
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    assert_eq!(shard.engine.lock().await.spot_known_frontier, Some(FRONTIER), "obligation handed to the runtime owner");
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w2 = spawn(rx);
    down.store(false, SeqCst); // exact history restored
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(103))).await; // valid lower prefix only
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let d = shard.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "{:?}", d.detail);
            if d.state == "live" {
                let c = shard.engine.lock().await.last_trade_id.clone();
                assert_eq!(c.as_deref(), Some("8294"), "live below the old known frontier");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("did not catch up to the known frontier");
    let o = oracle(FRONTIER).await;
    let os = crate::pick_shard(&o, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    assert_eq!(m1(shard).await, m1(os).await);
    assert_eq!(peak.load(SeqCst), 1);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w2).await.unwrap().unwrap();
    assert!(shard.engine.lock().await.spot_known_frontier.is_none(), "obligation released after the worker joins");
}

/// R1b startup frontier (spot): runtime initialized from history (closed-minute
/// boundary, no ingested cursor, no bootstrap origin). Old worker sees 8294 with
/// history down, commits nothing; FIFO closes. Successor sees only lower 103
/// with history restored. Must not go live below 8294; must catch up exactly.
#[tokio::test]
async fn spot_startup_handoff_keeps_known_frontier_without_cursor() {
    const FRONTIER: u64 = 8294;
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    shard.engine.lock().await.recovery_from_ms = Some(t(100).timestamp_ms);
    tokio::time::pause();
    let down = Arc::new(AtomicBool::new(true));
    let peak = Arc::new(AtomicUsize::new(0));
    let inflight = Arc::new(AtomicUsize::new(0));
    let spawn = |rx| {
        let (down, inflight, peak) = (down.clone(), inflight.clone(), peak.clone());
        tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |anchor| {
            let (down, inflight, peak) = (down.clone(), inflight.clone(), peak.clone());
            async move {
                peak.fetch_max(inflight.fetch_add(1, SeqCst) + 1, SeqCst);
                // Counted until completion OR cancellation, including the IO below.
                struct Held(Arc<AtomicUsize>);
                impl Drop for Held {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, SeqCst);
                    }
                }
                let _held = Held(inflight);
                tokio::time::sleep(Duration::from_millis(200)).await;
                if down.load(SeqCst) {
                    let e = reqwest::Client::new().get("http://127.0.0.1:1/").send().await.unwrap_err();
                    return Err(e.into());
                }
                let from = anchor.cursor.as_deref().map_or(100, |c| c.parse::<u64>().unwrap() + 1);
                Ok((from..=FRONTIER).map(t).collect())
            }
        }))
    };
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w1 = spawn(rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(FRONTIER))).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(shard.recovery.read().await.state, "recovering");
    // Positive witness: close the old worker while a fetch is actively held.
    tokio::time::timeout(Duration::from_secs(60), async {
        while inflight.load(SeqCst) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no active held fetch witnessed before close");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w1).await.expect("worker leaked on close").unwrap();
    assert_eq!(inflight.load(SeqCst), 0, "in-flight counter leaked by a fetch canceled on close");
    assert_eq!(shard.engine.lock().await.last_trade_id, None, "no replay commit while history was down");
    assert_eq!(shard.engine.lock().await.spot_bootstrap_id, None);
    assert_eq!(shard.engine.lock().await.spot_known_frontier, Some(FRONTIER), "startup obligation handed to the runtime owner");
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w2 = spawn(rx);
    down.store(false, SeqCst);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(103))).await;
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let d = shard.recovery.read().await.clone();
            assert!(!d.needs_backfill(), "{:?}", d.detail);
            if d.state == "live" {
                let c = shard.engine.lock().await.last_trade_id.clone();
                assert_eq!(c.as_deref(), Some("8294"), "live below the old known frontier");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("did not catch up to the known frontier");
    let o = oracle(FRONTIER).await;
    let os = crate::pick_shard(&o, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    assert_eq!(m1(shard).await, m1(os).await);
    assert_eq!(peak.load(SeqCst), 1);
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w2).await.unwrap().unwrap();
    assert!(shard.engine.lock().await.spot_known_frontier.is_none(), "obligation released after the worker joins");
}

/// R1b bootstrap-origin control: a first heartbeat alone is a witnessed origin,
/// not a replay obligation, and must not be retained across a socket close.
#[tokio::test]
async fn spot_first_heartbeat_origin_is_not_a_retained_obligation() {
    let state = Arc::new(crate::tests::state().await);
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let calls = Arc::new(AtomicUsize::new(0));
    let spawn = |rx| {
        let calls = calls.clone();
        tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_| {
            calls.fetch_add(1, SeqCst);
            async { Ok(Vec::new()) }
        }))
    };
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w = spawn(rx);
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Heartbeat(500)).await;
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
    assert_eq!(shard.engine.lock().await.spot_bootstrap_id, Some(500));
    assert_eq!(shard.engine.lock().await.spot_known_frontier, None, "origin heartbeat is not an obligation");
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let w = spawn(rx);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(calls.load(SeqCst), 0, "successor must not start recovery from the origin itself");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
}

// ---- Cleanup characterization: last-attempt cause and shared input accounting ----

#[derive(Clone, Copy, Debug)]
enum Answer {
    Hole,
    Empty,
    Invalid,
    Transport,
}

/// Run one spot worker (cursor 100, gap to buffered 104) whose history answers
/// `first` once and `rest` afterwards; return the first bounded episode's
/// outcome: Ok(detail) when it backs off as transient, Err(detail) when terminal.
async fn spot_first_episode(first: Answer, rest: Answer) -> std::result::Result<String, String> {
    let state = seeded().await;
    let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
    let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let w = tokio::spawn(run_spot_worker_with_fetch(state.clone(), Symbol::Btc, rx, move |_| {
        let answer = if c.fetch_add(1, SeqCst) == 0 { first } else { rest };
        async move {
            match answer {
                Answer::Hole => Ok([100, 101, 103, 104].into_iter().map(t).collect()),
                Answer::Empty => Ok(Vec::new()),
                Answer::Invalid => Err(anyhow::anyhow!("invalid replay trade side")),
                Answer::Transport => Err(reqwest::Client::new().get("http://127.0.0.1:1/").send().await.unwrap_err().into()),
            }
        }
    }));
    enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(104))).await;
    let out = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let d = shard.recovery.read().await.clone();
            let detail = d.detail.clone().unwrap_or_default();
            if d.needs_backfill() {
                break Err(detail);
            }
            assert_ne!(d.state, "live");
            if detail.contains("temporarily unavailable") {
                break Ok(detail);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("first bounded episode did not end");
    assert!(calls.load(SeqCst) <= RECOVERY_ATTEMPTS, "unchanged attempt cap");
    assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("100"));
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
    out
}

/// Only the last attempt's cause decides the episode: later results override
/// earlier ones in both directions, and existing diagnostics are unchanged.
#[tokio::test]
async fn spot_last_attempt_cause_overrides_earlier_attempts() {
    use Answer::*;
    let hole = "spot history has not closed the ID gap: Coinbase spot handoff has a trade ID gap";
    for (first, rest, terminal, needle) in [
        (Hole, Empty, false, "public history returned no trades"),
        (Empty, Hole, true, hole),
        (Hole, Invalid, true, "history response error: invalid replay trade side"),
        (Invalid, Empty, false, "public history returned no trades"),
        (Invalid, Transport, false, "history transport error:"),
        (Transport, Hole, true, hole),
    ] {
        let out = spot_first_episode(first, rest).await;
        let detail = match (&out, terminal) {
            (Err(d), true) => {
                assert!(d.starts_with("spot recovery budget exhausted after"), "{first:?}/{rest:?}: {d}");
                d
            }
            (Ok(d), false) => d,
            _ => panic!("{first:?}/{rest:?}: unexpected {out:?}"),
        };
        assert!(detail.contains(needle), "{first:?}/{rest:?}: {detail}");
        tokio::time::resume();
    }
}

/// A heartbeat observed in any waiting phase (fetch in flight, inter-attempt
/// pause, outage cooldown) advances the known frontier: recovery continues
/// through it before live, without a new trade.
#[tokio::test]
async fn spot_heartbeat_in_each_waiting_phase_extends_required_frontier() {
    for phase in ["in-flight", "pause", "cooldown"] {
        let state = seeded().await;
        let shard = crate::pick_shard(&state, Exchange::Coinbase, Market::Spot, Symbol::Btc);
        let up = Arc::new(Upstream {
            down: AtomicBool::new(true),
            hole: None,
            top: AtomicUsize::new(110),
            calls: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let (tx, rx) = mpsc::channel(RECOVERY_CHANNEL_CAPACITY);
        let w = spawn_worker(state.clone(), up.clone(), rx);
        enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Trade(t(103))).await;
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let detail = shard.recovery.read().await.detail.clone().unwrap_or_default();
                let (calls, inflight) = (up.calls.load(SeqCst), up.inflight.load(SeqCst));
                let ready = match phase {
                    "in-flight" => inflight == 1,
                    "pause" => calls == 1 && inflight == 0 && detail.contains("history transport error"),
                    _ => detail.contains("episode 1 ended"),
                };
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{phase}: phase not reached"));
        enqueue_spot(&state, Symbol::Btc, &tx, SpotInput::Heartbeat(110)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        up.down.store(false, SeqCst);
        tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                let d = shard.recovery.read().await.clone();
                assert!(!d.needs_backfill(), "{phase}: {:?}", d.detail);
                if d.state == "live" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{phase}: never live"));
        assert_eq!(shard.engine.lock().await.last_trade_id.as_deref(), Some("110"), "{phase}: live below heartbeat frontier");
        assert_eq!(up.peak.load(SeqCst), 1);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), w).await.unwrap().unwrap();
        tokio::time::resume();
    }
}

/// The coordinator's producers carry the hole/transient cause as a type, with
/// their existing diagnostic text unchanged.
#[test]
fn spot_coordinator_types_hole_apart_from_transient_retries() {
    let anchor = || RecoveryAnchor { cursor: Some("100".into()), from_ms: 0 };
    let mut c = RecoveryCoordinator::new(anchor(), 8);
    c.set_spot_target(104);
    c.push_ws(t(104)).unwrap();
    match c.observe_rest([100, 101, 103, 104].into_iter().map(t).collect()) {
        RecoveryDecision::Hole(d) => {
            assert_eq!(d, "spot history has not closed the ID gap: Coinbase spot handoff has a trade ID gap")
        }
        other => panic!("{other:?}"),
    }
    let mut c = RecoveryCoordinator::new(anchor(), 8);
    c.set_spot_target(105);
    c.push_ws(t(103)).unwrap();
    match c.observe_rest((100..=103).map(t).collect()) {
        RecoveryDecision::Retry(d) => assert_eq!(d, "spot history has not reached required-through ID 105"),
        other => panic!("{other:?}"),
    }
    let mut c = RecoveryCoordinator::new(anchor(), 8);
    c.push_ws(t(104)).unwrap();
    assert!(matches!(c.observe_rest(Vec::new()), RecoveryDecision::Retry(d) if d == "public history returned no trades"));
    assert!(matches!(c.observe_rest(vec![t(99)]), RecoveryDecision::Retry(d) if d == "public history is still behind the recovery cursor"));
}
