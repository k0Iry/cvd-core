//! Order-book reducer / hub tests. Replay events are SYNTHETIC (labelled);
//! `live_binance_btc_eth_sync` is an opt-in bounded real public run.
use super::*;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

fn lv(p: &str, q: &str) -> [String; 2] {
    [p.to_string(), q.to_string()]
}
fn ev(first: u64, last: u64, prev: u64, bids: Vec<[String; 2]>, asks: Vec<[String; 2]>) -> DiffEvent {
    DiffEvent {
        event: "depthUpdate".into(),
        event_time: 1_700_000_000_000 + last as i64,
        symbol: "BTCUSDT".into(),
        first,
        last,
        prev,
        bids,
        asks,
    }
}
fn snap(id: u64) -> RestSnapshot {
    // 60 bid levels 100.00..99.41, 60 ask levels 100.10..100.69
    let bids = (0..60).map(|i| lv(&format!("{:.2}", 100.0 - i as f64 * 0.01), "1.0")).collect();
    let asks = (0..60).map(|i| lv(&format!("{:.2}", 100.10 + i as f64 * 0.01), "2.0")).collect();
    RestSnapshot { last_update_id: id, bids, asks }
}
fn live_book() -> BookSync {
    let mut b = BookSync::new("BTCUSDT");
    b.on_event(ev(90, 95, 89, vec![], vec![])).unwrap(); // dropped later: u < L
    b.on_event(ev(96, 105, 95, vec![lv("100.00", "5")], vec![])).unwrap(); // bridge U<=100<=u
    b.on_snapshot(snap(100)).unwrap();
    assert!(b.is_live());
    b
}

#[test]
fn price_keys_are_exact_decimals() {
    assert_eq!(parse_price("100.10"), parse_price("100.1"));
    assert_eq!(parse_price("67000.1"), Some(6_700_010_000_000));
    assert_eq!(parse_price("1.000000001"), None);
    assert_eq!(parse_price("-1"), None);
    assert_eq!(parse_price("1e3"), None);
    assert_eq!(parse_price("0"), None);
}

#[test]
fn snapshot_bridge_drops_old_and_applies_bridging_event() {
    let b = live_book();
    assert_eq!(b.last_update_id(), Some(105));
    let (bids, asks) = b.top(3);
    assert_eq!(bids[0], [100.0, 5.0]); // absolute replacement, not addition
    assert_eq!(asks[0], [100.1, 2.0]);
}

#[test]
fn bridge_rule_rejects_event_starting_after_snapshot() {
    let mut b = BookSync::new("BTCUSDT");
    b.on_event(ev(101, 110, 100, vec![], vec![])).unwrap();
    assert_eq!(b.on_snapshot(snap(100)), Err(SyncError::BridgeGap));
}

#[test]
fn initial_pu_is_not_compared_to_snapshot_id() {
    // pu (95) != L (100) is legal for the bridging event (USD-M rule).
    let mut b = BookSync::new("BTCUSDT");
    b.on_snapshot(snap(100)).unwrap();
    assert!(!b.is_live(), "no live book before the bridging event");
    assert_eq!(b.on_event(ev(97, 103, 95, vec![], vec![])), Ok(true));
    assert!(b.is_live());
}

#[test]
fn pu_continuity_required_and_spot_rule_not_used() {
    let mut b = live_book();
    // U jumps (106 -> 120) but pu == previous u: valid on USD-M.
    assert_eq!(b.on_event(ev(120, 130, 105, vec![], vec![])), Ok(true));
    // Negative control: pu differs from the processed u => gap.
    assert_eq!(b.on_event(ev(131, 140, 129, vec![], vec![])), Err(SyncError::SequenceGap));
}

#[test]
fn duplicate_event_is_a_gap_not_reapplied() {
    let mut b = live_book();
    let e = ev(106, 110, 105, vec![lv("99.99", "7")], vec![]);
    b.on_event(e.clone()).unwrap();
    assert_eq!(b.on_event(e), Err(SyncError::SequenceGap));
}

#[test]
fn zero_deletes_and_missing_delete_is_normal() {
    let mut b = live_book();
    b.on_event(ev(106, 110, 105, vec![lv("100.00", "0"), lv("99.995", "0")], vec![])).unwrap();
    assert_eq!(b.top(1).0[0][0], 99.99);
}

#[test]
fn update_is_transactional_across_sides() {
    let mut b = live_book();
    // Bid moves to 100.10 while the 100.10 ask is removed in the same event:
    // only the completed update is checked.
    b.on_event(ev(106, 110, 105, vec![lv("100.10", "1")], vec![lv("100.10", "0")])).unwrap();
    assert_eq!(b.top(1).0[0][0], 100.1);
    assert_eq!(b.top(1).1[0][0], 100.11);
    // A completed crossed book is rejected and leaves state untouched.
    let before = b.top(5);
    assert_eq!(b.on_event(ev(111, 112, 110, vec![lv("100.20", "1")], vec![])), Err(SyncError::Crossed));
    assert_eq!(b.top(5), before);
}

#[test]
fn outside_coverage_is_never_reported() {
    let mut b = live_book();
    b.on_event(ev(106, 110, 105, vec![lv("50.00", "9")], vec![lv("500.00", "9")])).unwrap();
    let (bids, asks) = b.top(1000);
    assert!(bids.iter().all(|l| l[0] >= 99.41) && asks.iter().all(|l| l[0] <= 100.69));
    assert_eq!(b.coverage(), (99.41, 100.69));
}

#[test]
fn bad_input_and_source_identity_rejected() {
    let mut b = live_book();
    assert_eq!(b.on_event(ev(106, 110, 105, vec![lv("abc", "1")], vec![])), Err(SyncError::BadLevel));
    assert_eq!(b.on_event(ev(106, 110, 105, vec![lv("99", "-1")], vec![])), Err(SyncError::BadLevel));
    let mut other = ev(106, 110, 105, vec![], vec![]);
    other.symbol = "ETHUSDT".into();
    assert_eq!(b.on_event(other), Err(SyncError::SourceMismatch));
}

#[test]
fn buffer_is_bounded() {
    let mut b = BookSync::new("BTCUSDT");
    for i in 0..MAX_BUFFERED_EVENTS as u64 {
        b.on_event(ev(i, i, i, vec![], vec![])).unwrap();
    }
    assert_eq!(b.on_event(ev(9, 9, 9, vec![], vec![])), Err(SyncError::BufferOverflow));
}

#[test]
fn unsupported_sources() {
    assert_eq!(supported("binance", "usdm", "BTC"), Some(Symbol::Btc));
    assert_eq!(supported("binance", "usdm", "ETH"), Some(Symbol::Eth));
    for (e, m, s) in [
        ("binance", "spot", "BTC"),
        ("coinbase", "usdm", "BTC"),
        ("binance", "coinm", "BTC"),
        ("binance", "usdm", "SOL"),
        ("binance", "usdm_usdc", "BTC"),
    ] {
        assert_eq!(supported(e, m, s), None, "{e}/{m}/{s}");
    }
}

// ---------------------------------------------------------------- replay feed

/// Scripted SYNTHETIC exchange: each connect pops the next stream; snapshot
/// pops the next queued text (vectors are consumed from the end).
pub(crate) struct ReplayFeed {
    pub streams: StdMutex<Vec<mpsc::UnboundedReceiver<Result<String>>>>,
    pub snapshots: StdMutex<Vec<String>>,
}

impl BookFeed for ReplayFeed {
    fn connect(&self, _n: &str) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>> {
        let rx = self.streams.lock().unwrap().pop();
        Box::pin(async move {
            let rx = rx.ok_or_else(|| anyhow!("no stream"))?;
            let s = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) });
            Ok(Box::pin(s) as TextStream)
        })
    }
    fn snapshot(&self, _n: &str) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        let s = self.snapshots.lock().unwrap().pop();
        Box::pin(async move { s.ok_or_else(|| anyhow!("no snapshot")) })
    }
}

pub(crate) fn snap_json(id: u64, mid: f64) -> String {
    let bids: Vec<_> = (0..60).map(|i| [format!("{:.2}", mid - 0.05 - i as f64 * 0.01), "1.5".into()]).collect();
    let asks: Vec<_> = (0..60).map(|i| [format!("{:.2}", mid + 0.05 + i as f64 * 0.01), "2.5".into()]).collect();
    serde_json::json!({"lastUpdateId": id, "bids": bids, "asks": asks}).to_string()
}
pub(crate) fn ev_json(sym: &str, first: u64, last: u64, prev: u64, bid: (&str, &str)) -> String {
    serde_json::json!({"e":"depthUpdate","E":1_700_000_000_000i64 + last as i64,"T":0,"s":sym,"U":first,"u":last,"pu":prev,"b":[[bid.0,bid.1]],"a":[]}).to_string()
}

async fn wait_status(rx: &mut watch::Receiver<Arc<BookFrame>>, want: BookStatus) -> Arc<BookFrame> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            {
                let f = rx.borrow_and_update().clone();
                if f.status == want {
                    return f;
                }
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("status reached")
}

#[tokio::test(start_paused = true)]
async fn hub_live_gap_resync_stale_and_demand_release() {
    let (tx1, rx1) = mpsc::unbounded_channel();
    let (tx2, rx2) = mpsc::unbounded_channel();
    let feed = Arc::new(ReplayFeed {
        streams: StdMutex::new(vec![rx2, rx1]),
        snapshots: StdMutex::new(vec![snap_json(500, 200.0), snap_json(100, 100.0)]),
    });
    let hub = BookHub::new(feed);
    let mut a = hub.subscribe(Symbol::Btc);
    let b = hub.subscribe(Symbol::Btc); // shared: one task
    assert_eq!(hub.active(), vec![(Symbol::Btc, 2)]);
    let s0 = a.rx.borrow().clone();
    assert!(s0.status == BookStatus::Syncing && s0.json.contains("\"bids\":[]"), "no fabricated depth while syncing");
    tx1.send(Ok(ev_json("BTCUSDT", 99, 101, 98, ("99.95", "4")))).unwrap();
    let f = wait_status(&mut a.rx, BookStatus::Live).await;
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    assert_eq!(v["generation"], 1);
    assert_eq!(v["bids"][0], serde_json::json!([99.95, 4.0]));
    assert_eq!(v["quote"], "USDT");
    // Gap => withdrawn (stale, empty levels) then resync generation 2.
    tx1.send(Ok(ev_json("BTCUSDT", 110, 111, 109, ("99.95", "1")))).unwrap();
    let s = wait_status(&mut a.rx, BookStatus::Stale).await;
    let v: serde_json::Value = serde_json::from_str(&s.json).unwrap();
    assert_eq!(v["bids"].as_array().unwrap().len(), 0);
    assert_eq!(v["reason"], "sequencegap");
    tx2.send(Ok(ev_json("BTCUSDT", 499, 501, 498, ("199.95", "3")))).unwrap();
    let f = wait_status(&mut a.rx, BookStatus::Live).await;
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    assert_eq!((v["generation"].as_u64(), v["bids"][0][0].as_f64()), (Some(2), Some(199.95)));
    // Silence => stale with withdrawn levels.
    let s = wait_status(&mut a.rx, BookStatus::Stale).await;
    assert!(s.json.contains("\"stale_feed\"") && s.json.contains("\"bids\":[]"), "{}", s.json);
    drop(b);
    assert_eq!(hub.active(), vec![(Symbol::Btc, 1)]);
    drop(a);
    assert!(hub.active().is_empty(), "last subscriber releases the book task");
    drop((tx1, tx2));
}

/// Opt-in bounded REAL public Binance run (network). Writes a receipt when
/// `CVD_BOOK_LIVE_RECEIPT` is set.
#[tokio::test]
#[ignore = "network: real public Binance USD-M depth"]
async fn live_binance_btc_eth_sync() {
    let hub = BookHub::new(Arc::new(BinanceFeed {
        http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap(),
    }));
    let mut out = serde_json::Map::new();
    for sym in [Symbol::Btc, Symbol::Eth] {
        let mut s = hub.subscribe(sym);
        let mut seqs = Vec::new();
        let mut last = None;
        while seqs.len() < 20 {
            let f = wait_status(&mut s.rx, BookStatus::Live).await;
            let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
            assert_eq!(v["source"]["symbol"], sym.as_str());
            assert!(v["bids"][0][0].as_f64().unwrap() < v["asks"][0][0].as_f64().unwrap());
            assert!(v["bids"].as_array().unwrap().len() >= MIN_COVERED_LEVELS);
            seqs.push((v["generation"].as_u64().unwrap(), v["seq"].as_u64().unwrap()));
            last = Some(v);
            s.rx.changed().await.unwrap();
        }
        assert!(
            seqs.windows(2).all(|w| w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 < w[1].1)),
            "generation/seq must be strictly monotonic: {seqs:?}"
        );
        let cov = &last.as_ref().unwrap()["coverage"];
        let lb = last.as_ref().unwrap()["bids"].as_array().unwrap().last().unwrap()[0].clone();
        assert_eq!(cov["bid_floor"], lb, "published coverage == transmitted subset");
        let v = last.unwrap();
        out.insert(
            sym.as_str().into(),
            serde_json::json!({"live_frames": seqs.len(), "generation": v["generation"],
            "seq_monotonic_within_generation": seqs.windows(2).all(|w| w[0].0 != w[1].0 || w[0].1 < w[1].1),
            "bid_levels": v["bids"].as_array().map(|a| a.len()), "ask_levels": v["asks"].as_array().map(|a| a.len()),
            "coverage": v["coverage"], "best": [v["bids"][0][0].clone(), v["asks"][0][0].clone()]}),
        );
        drop(s);
    }
    assert!(hub.active().is_empty());
    // Upstream termination: aborted tasks release their sockets; a fresh
    // subscription must open a new generation (not reuse an old task).
    if let Ok(p) = std::env::var("CVD_BOOK_LIVE_RECEIPT") {
        std::fs::write(p, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
}

// ------------------------------------------------- corrections B/C (SYNTHETIC)

/// Scripted feed with snapshot delays and recorded start instants.
pub(crate) struct TimedFeed {
    pub streams: StdMutex<Vec<mpsc::UnboundedReceiver<Result<String>>>>,
    /// (delay, body) popped from the end; empty => snapshot never resolves.
    pub snapshots: StdMutex<Vec<(Duration, String)>>,
    pub connects: StdMutex<Vec<Instant>>,
    pub snapshot_starts: StdMutex<Vec<Instant>>,
    /// Per-native-symbol connect delays (popped from the front).
    pub connect_delays: StdMutex<HashMap<String, Vec<Duration>>>,
    /// Actual snapshot initiations with their native symbol.
    pub starts_by: StdMutex<Vec<(String, Instant)>>,
}
impl TimedFeed {
    fn new(streams: Vec<mpsc::UnboundedReceiver<Result<String>>>, snapshots: Vec<(Duration, String)>) -> Arc<Self> {
        Arc::new(Self {
            streams: StdMutex::new(streams),
            snapshots: StdMutex::new(snapshots),
            connects: StdMutex::new(Vec::new()),
            snapshot_starts: StdMutex::new(Vec::new()),
            connect_delays: StdMutex::new(HashMap::new()),
            starts_by: StdMutex::new(Vec::new()),
        })
    }
}
impl BookFeed for TimedFeed {
    fn connect(&self, n: &str) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>> {
        self.connects.lock().unwrap().push(Instant::now());
        let rx = self.streams.lock().unwrap().pop();
        let delay = self.connect_delays.lock().unwrap().get_mut(n).and_then(|v| (!v.is_empty()).then(|| v.remove(0)));
        Box::pin(async move {
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            let rx = rx.ok_or_else(|| anyhow!("no stream"))?;
            let s = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) });
            Ok(Box::pin(s) as TextStream)
        })
    }
    fn snapshot(&self, n: &str) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
        self.snapshot_starts.lock().unwrap().push(Instant::now());
        self.starts_by.lock().unwrap().push((n.to_string(), Instant::now()));
        let s = self.snapshots.lock().unwrap().pop();
        Box::pin(async move {
            match s {
                Some((d, body)) => {
                    tokio::time::sleep(d).await;
                    Ok(body)
                }
                None => std::future::pending().await,
            }
        })
    }
}

fn reason(f: &BookFrame) -> String {
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    v["reason"].as_str().unwrap_or("").to_string()
}

/// C: >500 distinct levels per side; the published coverage must describe the
/// transmitted subset, not the maintained 1000-level snapshot range.
#[test]
fn published_coverage_matches_transmitted_subset() {
    let bids = (0..900).map(|i| lv(&format!("{:.1}", 10_000.0 - i as f64 * 0.1), "1")).collect();
    let asks = (0..900).map(|i| lv(&format!("{:.1}", 10_000.1 + i as f64 * 0.1), "1")).collect();
    let mut b = BookSync::new("BTCUSDT");
    b.on_snapshot(RestSnapshot { last_update_id: 10, bids, asks }).unwrap();
    b.on_event(ev(9, 11, 8, vec![], vec![])).unwrap();
    let f = frame(Symbol::Btc, BookStatus::Live, 1, Some(&b), None);
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    let pb = v["bids"].as_array().unwrap();
    let pa = v["asks"].as_array().unwrap();
    assert_eq!((pb.len(), pa.len()), (PUBLISH_LEVELS, PUBLISH_LEVELS));
    assert_eq!(v["coverage"]["bid_floor"], pb.last().unwrap()[0], "bid coverage == lowest transmitted bid");
    assert_eq!(v["coverage"]["ask_ceiling"], pa.last().unwrap()[0], "ask coverage == highest transmitted ask");
    // Maintained range stays distinct (wider) and is reported separately.
    assert_eq!(v["maintained_coverage"]["bid_floor"].as_f64(), Some(10_000.0 - 899.0 * 0.1).map(|x| (x * 10.0f64).round() / 10.0));
    // Untruncated side keeps the full maintained range.
    let small = live_book();
    let v: serde_json::Value = serde_json::from_str(&frame(Symbol::Btc, BookStatus::Live, 1, Some(&small), None).json).unwrap();
    assert_eq!(v["coverage"]["bid_floor"].as_f64(), Some(99.41));
}

/// B1: valid diffs during a never-finishing snapshot must not restart the deadline.
#[tokio::test(start_paused = true)]
async fn snapshot_deadline_is_absolute() {
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(Symbol::Btc);
    let t0 = Instant::now();
    let pump = tokio::spawn(async move {
        for i in 0..100u64 {
            if tx.send(Ok(ev_json("BTCUSDT", 10 + i, 10 + i, 9 + i, ("99.95", "1")))).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let f = wait_status(&mut s.rx, BookStatus::Stale).await;
    assert_eq!(reason(&f), "snapshot_timeout");
    assert!(t0.elapsed() <= SYNC_DEADLINE + Duration::from_millis(50), "{:?}", t0.elapsed());
    pump.abort();
}

/// B2: a bridge received long before a delayed snapshot is not fresh live data.
#[tokio::test(start_paused = true)]
async fn aged_buffered_bridge_is_never_published_live() {
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![(Duration::from_secs(7), snap_json(100, 100.0))]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(Symbol::Btc);
    tx.send(Ok(ev_json("BTCUSDT", 99, 101, 98, ("99.95", "4")))).unwrap();
    // Quiet stream afterwards. Collect every frame until Stale.
    let mut statuses = Vec::new();
    let f = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let f = s.rx.borrow_and_update().clone();
            statuses.push(f.status);
            if f.status == BookStatus::Stale {
                return f;
            }
            s.rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(!statuses.contains(&BookStatus::Live), "expired buffered bridge published live: {statuses:?}");
    assert_eq!(reason(&f), "stale_feed");
    drop(tx);
}

/// B4: oversized single input and many buffered inputs fail with fixed reasons.
#[tokio::test(start_paused = true)]
async fn input_byte_budgets() {
    let big = "x".repeat(MAX_EVENT_BYTES + 1);
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(Symbol::Btc);
    tx.send(Ok(big)).unwrap();
    assert_eq!(reason(&*wait_status(&mut s.rx, BookStatus::Stale).await), "oversized_input");
    drop(s);

    // Many valid-but-large buffered events exceed the byte (not count) budget.
    let pad: Vec<[String; 2]> = (0..2000).map(|i| [format!("{}.5", 50 + i), "0".into()]).collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(Symbol::Btc);
    for i in 0..400u64 {
        let e = serde_json::json!({"e":"depthUpdate","E":1,"s":"BTCUSDT","U":i,"u":i,"pu":i,"b":pad,"a":[]}).to_string();
        assert!(e.len() < MAX_EVENT_BYTES && e.len() * 400 > MAX_BUFFERED_BYTES);
        let _ = tx.send(Ok(e));
    }
    assert_eq!(reason(&*wait_status(&mut s.rx, BookStatus::Stale).await), "bufferoverflow");
    drop(s);

    // Oversized snapshot body.
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![(Duration::ZERO, "y".repeat(MAX_SNAPSHOT_BYTES + 1))]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(Symbol::Btc);
    assert_eq!(reason(&*wait_status(&mut s.rx, BookStatus::Stale).await), "oversized_input");
    drop((s, tx));
}

/// B3: short live-then-disconnect loops keep backoff history; demand churn
/// cannot start snapshots faster than the shared per-symbol budget.
#[tokio::test(start_paused = true)]
async fn reconnect_backoff_and_snapshot_budget() {
    let mut streams = Vec::new();
    let mut senders = Vec::new();
    for _ in 0..5 {
        let (tx, rx) = mpsc::unbounded_channel();
        streams.push(rx);
        senders.push(tx);
    }
    let snaps = (0..5).map(|_| (Duration::ZERO, snap_json(100, 100.0))).collect();
    let feed = TimedFeed::new(streams, snaps);
    let hub = BookHub::new(feed.clone());
    let mut s = hub.subscribe(Symbol::Btc);
    // Each generation goes live briefly then disconnects.
    for tx in senders.into_iter().rev() {
        tx.send(Ok(ev_json("BTCUSDT", 99, 101, 98, ("99.95", "4")))).unwrap();
        wait_status(&mut s.rx, BookStatus::Live).await;
        drop(tx);
        wait_status(&mut s.rx, BookStatus::Stale).await;
    }
    let c = feed.connects.lock().unwrap().clone();
    let gaps: Vec<_> = c.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.windows(2).all(|w| w[1] >= w[0]) && gaps.last() > gaps.first(),
        "backoff must grow across short live loops (not reset by brief live): {gaps:?}"
    );
    drop(s);

    // Demand churn: subscribe/drop repeatedly; snapshot starts stay spaced.
    let streams = (0..6).map(|_| mpsc::unbounded_channel::<Result<String>>().1).collect();
    let feed = TimedFeed::new(streams, vec![]);
    let hub = BookHub::new(feed.clone());
    for _ in 0..6 {
        let s = hub.subscribe(Symbol::Btc);
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(s);
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let starts = feed.snapshot_starts.lock().unwrap().clone();
    assert!(starts.windows(2).all(|w| w[1] - w[0] >= SNAPSHOT_MIN_GAP), "{} starts too close", starts.len());
}

/// B5: dropping the last subscriber terminates the upstream socket/stream.
#[tokio::test(start_paused = true)]
async fn last_demand_closes_upstream_stream() {
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![(Duration::ZERO, snap_json(100, 100.0))]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(Symbol::Btc);
    tx.send(Ok(ev_json("BTCUSDT", 99, 101, 98, ("99.95", "4")))).unwrap();
    wait_status(&mut s.rx, BookStatus::Live).await;
    assert!(!tx.is_closed());
    drop(s);
    tokio::time::timeout(Duration::from_secs(1), tx.closed()).await.expect("upstream receiver dropped");
    assert!(hub.active().is_empty());
}

// ------------------------------------------- final closure §1 (SYNTHETIC)

/// §1 B-1: beyond the bridge, a `pu`-chained but non-increasing `u` (repeat or
/// backwards) and a malformed `U > u` fail closed without touching the book.
#[test]
fn live_u_must_strictly_increase_and_range_well_formed() {
    // Bridge where u == snapshot id is legal.
    let mut b = BookSync::new("BTCUSDT");
    b.on_snapshot(snap(100)).unwrap();
    assert_eq!(b.on_event(ev(99, 100, 98, vec![], vec![])), Ok(true));
    assert_eq!(b.last_update_id(), Some(100));

    for bad in [
        ev(100, 100, 100, vec![lv("99.99", "9")], vec![]), // repeat u, pu chained
        ev(99, 99, 100, vec![lv("99.99", "9")], vec![]),   // backwards u, pu chained
        ev(105, 101, 100, vec![lv("99.99", "9")], vec![]), // malformed U > u
    ] {
        let mut b2 = BookSync::new("BTCUSDT");
        b2.on_snapshot(snap(100)).unwrap();
        b2.on_event(ev(99, 100, 98, vec![], vec![])).unwrap();
        let before = b2.top(60);
        assert_eq!(b2.on_event(bad), Err(SyncError::SequenceGap));
        assert_eq!(b2.top(60), before, "book untouched on rejection");
        assert_eq!(b2.last_update_id(), Some(100));
    }
    // Malformed range is rejected while buffering too.
    let mut b3 = BookSync::new("BTCUSDT");
    assert_eq!(b3.on_event(ev(10, 9, 8, vec![], vec![])), Err(SyncError::SequenceGap));
    // Positive control: futures ids skip (U far above last_u + 1) is still valid.
    assert_eq!(b.on_event(ev(150, 160, 100, vec![], vec![])), Ok(true));
}

/// §1 B-1 at the hub: a repeated/decreasing u never produces a repeated or
/// decreasing published seq; the generation ends with a fixed reason.
#[tokio::test(start_paused = true)]
async fn hub_never_publishes_non_increasing_seq() {
    for (u, pu) in [(101u64, 101u64), (100, 101)] {
        let (tx, rx) = mpsc::unbounded_channel();
        let feed = TimedFeed::new(vec![rx], vec![(Duration::ZERO, snap_json(100, 100.0))]);
        let hub = BookHub::new(feed);
        let mut s = hub.subscribe(Symbol::Btc);
        tx.send(Ok(ev_json("BTCUSDT", 99, 101, 98, ("99.95", "4")))).unwrap();
        wait_status(&mut s.rx, BookStatus::Live).await;
        tokio::time::sleep(PUBLISH_INTERVAL * 2).await;
        tx.send(Ok(ev_json("BTCUSDT", u.min(101), u, pu, ("99.94", "3")))).unwrap();
        let mut seqs = Vec::new();
        let f = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let f = s.rx.borrow_and_update().clone();
                let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
                if f.status == BookStatus::Live {
                    seqs.push(v["seq"].as_u64().unwrap());
                }
                if f.status == BookStatus::Stale {
                    return f;
                }
                s.rx.changed().await.unwrap();
            }
        })
        .await
        .expect("bad sequence must end the generation");
        assert_eq!(reason(&f), "sequencegap");
        assert!(seqs.iter().all(|&q| q == 101), "only the bridged seq was live: {seqs:?}");
        drop((s, tx));
    }
}

/// §1 B-2: the budget binds ACTUAL snapshot starts. Slow first connect then
/// fast reconnect, BTC and ETH concurrently, plus demand churn. Each symbol
/// must record >= 2 starts before spacing is asserted (non-vacuous).
#[tokio::test(start_paused = true)]
async fn snapshot_budget_binds_actual_snapshot_start() {
    let mut keep = Vec::new();
    let mut streams = Vec::new();
    for _ in 0..40 {
        let (tx, rx) = mpsc::unbounded_channel::<Result<String>>();
        keep.push(tx);
        streams.push(rx);
    }
    // Every snapshot resolves immediately with an unparseable body => the
    // generation ends ("snapshot_failed") and retries after backoff (1s first).
    let snaps = (0..40).map(|_| (Duration::ZERO, "not-json".to_string())).collect();
    let feed = TimedFeed::new(streams, snaps);
    {
        let mut d = feed.connect_delays.lock().unwrap();
        d.insert("BTCUSDT".into(), vec![Duration::from_secs(9)]); // slow, then fast
        d.insert("ETHUSDT".into(), vec![Duration::from_millis(9_500)]);
    }
    let hub = BookHub::new(feed.clone());
    let btc = hub.subscribe(Symbol::Btc);
    let eth = hub.subscribe(Symbol::Eth);
    tokio::time::sleep(Duration::from_secs(14)).await;
    // Demand churn: drop and resubscribe both symbols repeatedly.
    drop((btc, eth));
    for _ in 0..4 {
        let b = hub.subscribe(Symbol::Btc);
        let e = hub.subscribe(Symbol::Eth);
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop((b, e));
    }
    let b = hub.subscribe(Symbol::Btc);
    let e = hub.subscribe(Symbol::Eth);
    tokio::time::sleep(Duration::from_secs(8)).await;
    drop((b, e));
    let starts = feed.starts_by.lock().unwrap().clone();
    for sym in ["BTCUSDT", "ETHUSDT"] {
        let t: Vec<Instant> = starts.iter().filter(|(n, _)| n == sym).map(|(_, t)| *t).collect();
        assert!(t.len() >= 3, "{sym}: need >=3 actual snapshot starts, got {}", t.len());
        let gaps: Vec<Duration> = t.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.iter().all(|g| *g >= SNAPSHOT_MIN_GAP), "{sym} actual snapshot starts too close: {gaps:?}");
    }
    drop(keep);
}

/// §1 S4: the production REST body reader against a bounded local HTTP server
/// (no Content-Length / chunked / lying Content-Length). No exchange traffic.
#[tokio::test]
async fn rest_body_reader_bounds_real_http_responses() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn serve(head: &'static str, body: Vec<u8>, chunked: bool) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut c, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = c.read(&mut buf).await;
            let _ = c.write_all(head.as_bytes()).await;
            if chunked {
                for part in body.chunks(64 * 1024) {
                    let _ = c.write_all(format!("{:x}\r\n", part.len()).as_bytes()).await;
                    let _ = c.write_all(part).await;
                    if c.write_all(b"\r\n").await.is_err() {
                        return;
                    }
                }
                let _ = c.write_all(b"0\r\n\r\n").await;
            } else {
                let _ = c.write_all(&body).await;
            }
            let _ = c.shutdown().await;
        });
        format!("http://{addr}/fapi/v1/depth")
    }
    let http = reqwest::Client::new();
    let big = vec![b'a'; MAX_SNAPSHOT_BYTES + 1];
    let cases = [
        ("missing length, oversized", serve("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", big.clone(), false).await, Some("oversized_input")),
        ("chunked, oversized", serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", big.clone(), true).await, Some("oversized_input")),
        ("chunked, small", serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", b"{\"ok\":1}".to_vec(), true).await, None),
        ("http error", serve("HTTP/1.1 429 Too Many\r\nContent-Length: 0\r\n\r\n", vec![], false).await, Some("snapshot http 429")),
    ];
    for (name, url, want) in cases {
        let r = tokio::time::timeout(Duration::from_secs(10), async { read_snapshot_body(http.get(url).send().await?).await })
            .await
            .expect("bounded");
        match want {
            Some(e) => assert_eq!(r.unwrap_err().to_string(), e, "{name}"),
            None => assert_eq!(r.unwrap(), "{\"ok\":1}", "{name}"),
        }
    }
}
