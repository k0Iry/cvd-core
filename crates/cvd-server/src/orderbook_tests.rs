//! Order-book reducer / hub tests. Replay events are SYNTHETIC (labelled);
//! `live_binance_btc_eth_sync` is an opt-in bounded real public run.
use super::*;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

const UB: BookKey = BookKey::binance(Market::Usdm, Symbol::Btc);
const UE: BookKey = BookKey::binance(Market::Usdm, Symbol::Eth);
const SB: BookKey = BookKey::binance(Market::Spot, Symbol::Btc);

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
        prev: Some(prev),
        bids,
        asks,
    }
}
fn spot_ev(first: u64, last: u64, bids: Vec<[String; 2]>) -> DiffEvent {
    DiffEvent { prev: None, ..ev(first, last, 0, bids, vec![]) }
}
fn spot_snap(id: u64, n: usize) -> RestSnapshot {
    RestSnapshot {
        last_update_id: id,
        bids: (0..n).map(|i| lv(&format!("{:.2}", 50_000.0 - i as f64 * 0.01), "1")).collect(),
        asks: (0..n).map(|i| lv(&format!("{:.2}", 50_000.01 + i as f64 * 0.01), "1")).collect(),
    }
}

#[test]
fn spot_bridge_requires_l_plus_one_and_contiguous_ids() {
    // u == L is stale for spot (would be accepted as a bridge by futures).
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    b.on_event(spot_ev(90, 100, vec![])).unwrap();
    b.on_event(spot_ev(101, 105, vec![lv("49999.00", "7")])).unwrap();
    b.on_snapshot(spot_snap(100, 5000)).unwrap();
    assert_eq!(b.last_update_id(), Some(105));
    assert_eq!(b.on_event(spot_ev(106, 110, vec![])), Ok(true));
    // Non-contiguous U is a gap even though u advances.
    assert_eq!(b.on_event(spot_ev(112, 120, vec![])), Err(SyncError::SequenceGap));
    // A futures-shaped event (pu present) is rejected on a spot book.
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    b.on_snapshot(spot_snap(100, 10)).unwrap();
    assert_eq!(b.on_event(ev(101, 102, 100, vec![], vec![])), Err(SyncError::SourceMismatch));
    // And a spot-shaped event is rejected on a futures book.
    assert_eq!(BookSync::new("BTCUSDT").on_event(spot_ev(1, 2, vec![])), Err(SyncError::SourceMismatch));
    // Bridge gap: first event starts after L+1.
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    b.on_snapshot(spot_snap(100, 10)).unwrap();
    assert_eq!(b.on_event(spot_ev(102, 103, vec![])), Err(SyncError::BridgeGap));
}

/// Official spot rule (spot-ws.md:785-793): U <= local+1 <= u is valid even
/// when the range overlaps already-applied ids; u <= local is ignored;
/// U > local+1 is a gap. Counterexample to strict `U == last_u + 1`.
#[test]
fn spot_live_overlap_applies_stale_ignored_gap_detected() {
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    b.on_snapshot(spot_snap(100, 10)).unwrap();
    assert_eq!(b.on_event(spot_ev(101, 107, vec![])), Ok(true)); // local = 107
                                                                 // Overlap U105/u110 with an absolute quantity: applied, not a gap.
    assert_eq!(b.on_event(spot_ev(105, 110, vec![lv("49999.95", "3.25")])), Ok(true));
    assert_eq!(b.last_update_id(), Some(110));
    assert!(b.top(5000).0.iter().any(|l| l[0] == 49999.95 && l[1] == 3.25));
    // Stale/duplicate (u <= 110): ignored, book unchanged.
    assert_eq!(b.on_event(spot_ev(108, 110, vec![lv("49999.95", "9")])), Ok(false));
    assert_eq!(b.on_event(spot_ev(100, 104, vec![lv("49999.95", "9")])), Ok(false));
    assert!(b.top(5000).0.iter().any(|l| l[0] == 49999.95 && l[1] == 3.25));
    // Exact successor still valid; real gap still detected.
    assert_eq!(b.on_event(spot_ev(111, 111, vec![])), Ok(true));
    assert_eq!(b.on_event(spot_ev(113, 115, vec![])), Err(SyncError::SequenceGap));
    // Near u64::MAX no overflow panic.
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    b.on_snapshot(spot_snap(u64::MAX - 1, 10)).unwrap();
    assert_eq!(b.on_event(spot_ev(u64::MAX - 1, u64::MAX, vec![])), Ok(true));
    assert_eq!(b.on_event(spot_ev(u64::MAX, u64::MAX, vec![])), Ok(false));
}

/// A spot frame publishes all 5000 levels/side with truthful source identity.
#[test]
fn spot_frame_publishes_5000_with_spot_identity() {
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    b.on_snapshot(spot_snap(100, 5000)).unwrap();
    b.on_event(spot_ev(101, 101, vec![])).unwrap();
    let v: serde_json::Value = serde_json::from_str(&frame(SB, BookStatus::Live, 1, Some(&b), None).json).unwrap();
    assert_eq!(v["source"]["market"], "spot");
    assert_eq!(v["depth_limit"], 5000);
    assert_eq!(v["bids"].as_array().unwrap().len(), 5000);
    assert_eq!(v["asks"].as_array().unwrap().len(), 5000);
    let uf = serde_json::from_str::<serde_json::Value>(&frame(UB, BookStatus::Syncing, 1, None, None).json).unwrap();
    assert_eq!((uf["source"]["market"].as_str(), uf["depth_limit"].as_u64()), (Some("usdm"), Some(1000)));
}

#[test]
fn spot_snapshot_accepts_5000_rejects_5001_futures_keeps_1000() {
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    assert!(b.on_snapshot(spot_snap(1, 5000)).is_ok());
    let mut b = BookSync::with_venue("BTCUSDT", Venue::Spot);
    assert_eq!(b.on_snapshot(spot_snap(1, 5001)), Err(SyncError::BadSnapshot));
    let mut b = BookSync::new("BTCUSDT");
    assert_eq!(b.on_snapshot(spot_snap(1, 1001)), Err(SyncError::BadSnapshot));
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
    assert_eq!(supported("binance", "usdm", "BTC"), Some(UB));
    assert_eq!(supported("binance", "usdm", "ETH"), Some(UE));
    assert_eq!(supported("binance", "spot", "BTC"), Some(SB));
    // M2: all ten Binance keys supported (see m2_*); Coinbase stays deferred.
    for (e, m, s) in [
        ("coinbase", "usdm", "BTC"),
        ("coinbase", "usdc", "SOL"),
        ("coinbase", "drb", "BTC"),
        ("binance", "usdm", "SOL"),
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
    fn connect(
        &self,
        _v: BookKey,
        _n: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>> {
        let rx = self.streams.lock().unwrap().pop();
        Box::pin(async move {
            let rx = rx.ok_or_else(|| anyhow!("no stream"))?;
            let s = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) });
            Ok(Box::pin(s) as TextStream)
        })
    }
    fn snapshot(&self, _v: BookKey, _n: &str) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
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
    let mut a = hub.subscribe(UB);
    let b = hub.subscribe(UB); // shared: one task
    assert_eq!(hub.active(), vec![(UB, 2)]);
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
    assert_eq!(hub.active(), vec![(UB, 1)]);
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
    for sym in [UB, UE] {
        let mut s = hub.subscribe(sym);
        let mut seqs = Vec::new();
        let mut last = None;
        while seqs.len() < 20 {
            let f = wait_status(&mut s.rx, BookStatus::Live).await;
            let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
            assert_eq!(v["source"]["symbol"], sym.symbol.as_str());
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
            sym.symbol.as_str().into(),
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
    fn connect(
        &self,
        _v: BookKey,
        n: &str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<TextStream>> + Send + '_>> {
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
    fn snapshot(&self, _v: BookKey, n: &str) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
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
    let f = frame(UB, BookStatus::Live, 1, Some(&b), None);
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    let pb = v["bids"].as_array().unwrap();
    let pa = v["asks"].as_array().unwrap();
    // M2: futures publication is now the full 1000 cap, so 900 is untruncated;
    // the exact >1000 truncation oracle is m2_futures_publish_cap_1000_exact_truncation_boundary.
    assert_eq!((pb.len(), pa.len()), (900, 900));
    assert!(PUBLISH_LEVELS == 1000);
    assert_eq!(v["coverage"]["bid_floor"], pb.last().unwrap()[0], "bid coverage == lowest transmitted bid");
    assert_eq!(v["coverage"]["ask_ceiling"], pa.last().unwrap()[0], "ask coverage == highest transmitted ask");
    // Maintained range stays distinct (wider) and is reported separately.
    assert_eq!(
        v["maintained_coverage"]["bid_floor"].as_f64(),
        Some(10_000.0 - 899.0 * 0.1).map(|x| (x * 10.0f64).round() / 10.0)
    );
    // Untruncated side keeps the full maintained range.
    let small = live_book();
    let v: serde_json::Value = serde_json::from_str(&frame(UB, BookStatus::Live, 1, Some(&small), None).json).unwrap();
    assert_eq!(v["coverage"]["bid_floor"].as_f64(), Some(99.41));
}

/// B1: valid diffs during a never-finishing snapshot must not restart the deadline.
#[tokio::test(start_paused = true)]
async fn snapshot_deadline_is_absolute() {
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(UB);
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
    let mut s = hub.subscribe(UB);
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
    let mut s = hub.subscribe(UB);
    tx.send(Ok(big)).unwrap();
    assert_eq!(reason(&*wait_status(&mut s.rx, BookStatus::Stale).await), "oversized_input");
    drop(s);

    // Many valid-but-large buffered events exceed the byte (not count) budget.
    let pad: Vec<[String; 2]> = (0..2000).map(|i| [format!("{}.5", 50 + i), "0".into()]).collect();
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(UB);
    for i in 0..400u64 {
        let e =
            serde_json::json!({"e":"depthUpdate","E":1,"s":"BTCUSDT","U":i,"u":i,"pu":i,"b":pad,"a":[]}).to_string();
        assert!(e.len() < MAX_EVENT_BYTES && e.len() * 400 > MAX_BUFFERED_BYTES);
        let _ = tx.send(Ok(e));
    }
    assert_eq!(reason(&*wait_status(&mut s.rx, BookStatus::Stale).await), "bufferoverflow");
    drop(s);

    // Oversized snapshot body.
    let (tx, rx) = mpsc::unbounded_channel();
    let feed = TimedFeed::new(vec![rx], vec![(Duration::ZERO, "y".repeat(MAX_SNAPSHOT_BYTES + 1))]);
    let hub = BookHub::new(feed);
    let mut s = hub.subscribe(UB);
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
    let mut s = hub.subscribe(UB);
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
        let s = hub.subscribe(UB);
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
    let mut s = hub.subscribe(UB);
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
        let mut s = hub.subscribe(UB);
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
    let btc = hub.subscribe(UB);
    let eth = hub.subscribe(UE);
    tokio::time::sleep(Duration::from_secs(14)).await;
    // Demand churn: drop and resubscribe both symbols repeatedly.
    drop((btc, eth));
    for _ in 0..4 {
        let b = hub.subscribe(UB);
        let e = hub.subscribe(UE);
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop((b, e));
    }
    let b = hub.subscribe(UB);
    let e = hub.subscribe(UE);
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
        (
            "missing length, oversized",
            serve("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", big.clone(), false).await,
            Some("oversized_input"),
        ),
        (
            "chunked, oversized",
            serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", big.clone(), true).await,
            Some("oversized_input"),
        ),
        (
            "chunked, small",
            serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", b"{\"ok\":1}".to_vec(), true).await,
            None,
        ),
        (
            "http error",
            serve("HTTP/1.1 429 Too Many\r\nContent-Length: 0\r\n\r\n", vec![], false).await,
            Some("snapshot http 429"),
        ),
    ];
    for (name, url, want) in cases {
        let r = tokio::time::timeout(Duration::from_secs(10), async {
            read_snapshot_body(http.get(url).send().await?).await
        })
        .await
        .expect("bounded");
        match want {
            Some(e) => assert_eq!(r.unwrap_err().to_string(), e, "{name}"),
            None => assert_eq!(r.unwrap(), "{\"ok\":1}", "{name}"),
        }
    }
}

// ------------------------------------------------------------------ M2: ten Binance keys

const TEN: [(&str, &str, &str, &str, usize); 10] = [
    ("spot", "BTC", "BTCUSDT", "USDT", 5000),
    ("spot", "ETH", "ETHUSDT", "USDT", 5000),
    ("spot_usdc", "BTC", "BTCUSDC", "USDC", 5000),
    ("spot_usdc", "ETH", "ETHUSDC", "USDC", 5000),
    ("usdm", "BTC", "BTCUSDT", "USDT", 1000),
    ("usdm", "ETH", "ETHUSDT", "USDT", 1000),
    ("usdm_usdc", "BTC", "BTCUSDC", "USDC", 1000),
    ("usdm_usdc", "ETH", "ETHUSDC", "USDC", 1000),
    ("coinm", "BTC", "BTCUSD_PERP", "USD", 1000),
    ("coinm", "ETH", "ETHUSD_PERP", "USD", 1000),
];

#[test]
fn m2_ten_exact_keys_distinct_and_registered() {
    let mut keys = std::collections::HashSet::new();
    for (m, s, native, quote, cap) in TEN {
        let k = supported("binance", m, s).unwrap_or_else(|| panic!("{m}/{s}"));
        let inst = instrument(k).unwrap();
        assert_eq!((inst.native_symbol, inst.quote_asset), (native, quote), "{m}/{s}");
        assert_eq!(k.venue().snapshot_limit(), cap);
        assert_eq!(k.venue().publish_levels(), cap);
        assert_eq!(k.label(), format!("binance/{m}/{s}"));
        assert!(keys.insert(k));
    }
    assert_eq!(keys.len(), 10);
    // Same native string on spot and USD-M are distinct keys.
    assert_ne!(supported("binance", "spot", "BTC"), supported("binance", "usdm", "BTC"));
    for (e, m, s) in [("coinbase", "usdc", "SOL"), ("coinbase", "SPOT", "BTC"), ("coinbase", "spot", "SOL"), ("binance", "usdc", "BTC"), ("binance", "usdm", "SOL"), ("binance", "SPOT", "BTC"), ("binance", "coinm", "btc")] {
        assert_eq!(supported(e, m, s), None, "{e}/{m}/{s}");
    }
}

#[test]
fn m2_coinm_contract_size_matches_recorded_exchange_info() {
    // Checked-in pinned excerpt of a recorded PUBLIC exchangeInfo response
    // (provenance inside); always exercised, never a runtime fetch.
    let t = include_str!("../tests/fixtures/binance-coinm-exchangeinfo-excerpt.json");
    let v: serde_json::Value = serde_json::from_str(t).expect("fixture json");
    assert!(v["provenance"]["recorded_file_sha256"].as_str().is_some_and(|h| h.len() == 64), "fixture provenance");
    assert_eq!(v["symbols"].as_array().expect("symbols").len(), 2);
    let mut checked = 0;
    for (s, native) in [(Symbol::Btc, "BTCUSD_PERP"), (Symbol::Eth, "ETHUSD_PERP")] {
        let e = v["symbols"].as_array().unwrap().iter().find(|x| x["symbol"] == native).unwrap();
        assert_eq!(e["quoteAsset"], "USD");
        assert_eq!(e["marginAsset"], s.as_str());
        assert_eq!(e["contractType"], "PERPETUAL");
        let k = BookKey::binance(Market::Coinm, s);
        assert_eq!(k.contract_size(), e["contractSize"].as_f64());
        assert!(k.contract_size().is_some());
        checked += 1;
    }
    assert_eq!(checked, 2);
}

#[test]
fn m2_inverse_conversion_per_level_price() {
    let k = BookKey::binance(Market::Coinm, Symbol::Btc);
    // 10 contracts * $100 at 50,000 => 0.02 BTC; at 100,000 => 0.01 BTC.
    assert_eq!(base_qty(k, 50_000.0, 10.0), 0.02);
    assert_eq!(base_qty(k, 100_000.0, 10.0), 0.01);
    let e = BookKey::binance(Market::Coinm, Symbol::Eth);
    assert_eq!(base_qty(e, 2_000.0, 4.0), 0.02);
    assert_eq!(base_qty(e, 4_000.0, 4.0), 0.01);
    // Linear books pass base through.
    assert_eq!(base_qty(BookKey::binance(Market::UsdmUsdc, Symbol::Btc), 50_000.0, 1.5), 1.5);
}

fn ladder(mid: f64, n: usize, tick: f64, bid: bool) -> Vec<[String; 2]> {
    (1..=n).map(|i| {
        let p = if bid { mid - tick * i as f64 } else { mid + tick * i as f64 };
        [format!("{p:.1}"), "10".to_string()]
    }).collect()
}

#[test]
fn m2_coinm_frame_converts_each_level_at_its_price() {
    let k = BookKey::binance(Market::Coinm, Symbol::Btc);
    let mut b = BookSync::with_venue("BTCUSD_PERP", k.venue());
    b.on_snapshot(RestSnapshot { last_update_id: 10, bids: ladder(50_000.0, 60, 100.0, true), asks: ladder(50_000.0, 60, 100.0, false) }).unwrap();
    b.on_event(DiffEvent { event: "depthUpdate".into(), event_time: 1, symbol: "BTCUSD_PERP".into(), first: 9, last: 11, prev: Some(8), bids: vec![], asks: vec![] }).unwrap();
    assert!(b.is_live());
    let f = frame(k, BookStatus::Live, 1, Some(&b), None);
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    assert_eq!(v["native_symbol"], "BTCUSD_PERP");
    assert_eq!(v["quote"], "USD");
    assert_eq!(v["raw_quantity"], "inverse_contracts");
    assert_eq!(v["publish_limit"], 1000);
    let b0 = &v["bids"][0];
    assert_eq!(b0[0].as_f64(), Some(49_900.0));
    assert_eq!(b0[1].as_f64(), Some(10.0 * 100.0 / 49_900.0));
    let b59 = &v["bids"][59];
    assert_eq!(b59[1].as_f64(), Some(10.0 * 100.0 / 44_000.0));
    let a0 = &v["asks"][0];
    assert_eq!(a0[1].as_f64(), Some(10.0 * 100.0 / 50_100.0));
}

#[test]
fn m2_futures_publish_cap_1000_exact_truncation_boundary() {
    // LEGAL 1000-level snapshot, then inside-coverage inserts grow maintained
    // state past 1000; transmission stays 1000 with exact coverage boundary.
    let k = BookKey::binance(Market::UsdmUsdc, Symbol::Eth);
    let mut b = BookSync::with_venue("ETHUSDC", k.venue());
    b.on_snapshot(RestSnapshot { last_update_id: 100, bids: ladder(2000.0, 1000, 0.2, true), asks: ladder(2000.0, 1000, 0.2, false) }).unwrap();
    let ins = |n: usize, bid: bool| (0..n).map(|i| {
        let p = if bid { 2000.0 - 0.1 - 0.2 * i as f64 } else { 2000.0 + 0.1 + 0.2 * i as f64 };
        [format!("{p:.1}"), "1".to_string()]
    }).collect::<Vec<_>>();
    b.on_event(DiffEvent { event: "depthUpdate".into(), event_time: 1, symbol: "ETHUSDC".into(), first: 99, last: 101, prev: Some(98), bids: ins(10, true), asks: ins(10, false) }).unwrap();
    assert!(b.is_live());
    let f = frame(k, BookStatus::Live, 1, Some(&b), None);
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    assert_eq!(v["bids"].as_array().unwrap().len(), 1000);
    assert_eq!(v["asks"].as_array().unwrap().len(), 1000);
    // 1010 maintained bids; 1000th best is snapshot level 995 => 2000-0.2*995 = 1801.0
    assert_eq!(v["coverage"]["bid_floor"].as_f64(), Some(1802.0));
    assert_eq!(v["coverage"]["ask_ceiling"].as_f64(), Some(2198.0));
    assert_eq!(v["maintained_coverage"]["bid_floor"].as_f64(), Some(1800.0));
    assert_eq!(v["bids"][999][0].as_f64(), Some(1802.0));
}

/// Bounded live smoke: ONE snapshot + live diff sync per exact key (no
/// repeated max-depth polling). Run with `--ignored m2_live_ten`.
#[tokio::test]
#[ignore]
async fn m2_live_ten_sources() {
    let feed = BinanceFeed { http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().unwrap() };
    let mut out = Vec::new();
    for (m, s, native, _q, cap) in TEN {
        let k = supported("binance", m, s).unwrap();
        let mut row = serde_json::json!({"key": format!("binance/{m}/{s}"), "native": native, "cap": cap});
        let r: Result<(), String> = async {
            let mut ws = tokio::time::timeout(CONNECT_DEADLINE, feed.connect(k, native)).await.map_err(|_| "connect_timeout")?.map_err(|e| format!("connect {e}"))?;
            let mut book = BookSync::with_venue(native, k.venue());
            let snap = feed.snapshot(k, native).await.map_err(|e| format!("snap {e}"))?;
            let snap: RestSnapshot = serde_json::from_str(&snap).map_err(|e| format!("parse {e}"))?;
            row["snapshot_bids"] = snap.bids.len().into();
            row["snapshot_asks"] = snap.asks.len().into();
            book.on_snapshot(snap).map_err(|e| format!("{e:?}"))?;
            let dl = Instant::now() + SYNC_DEADLINE;
            while !book.is_live() {
                let m = tokio::time::timeout_at(dl, ws.next()).await.map_err(|_| "sync_timeout")?;
                read_event(m, &mut book)?;
            }
            let f = frame(k, BookStatus::Live, 1, Some(&book), None);
            let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
            row["live"] = true.into();
            row["published_bids"] = v["bids"].as_array().unwrap().len().into();
            row["published_asks"] = v["asks"].as_array().unwrap().len().into();
            row["quote"] = v["quote"].clone();
            row["raw_quantity"] = v["raw_quantity"].clone();
            row["best_bid"] = v["bids"][0].clone();
            row["coverage"] = v["coverage"].clone();
            Ok(())
        }.await;
        if let Err(e) = r { row["error"] = e.into(); }
        println!("M2LIVE {row}");
        out.push(row);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(out.iter().all(|r| r["live"] == true), "some sources failed");
}

// ------------------------------------------------- M3A Coinbase Exchange spot
// SYNTHETIC recorded-shape level2_batch fixtures (generated, not market dumps).

const CB: BookKey = BookKey { exchange: crate::types::Exchange::Coinbase, market: Market::Spot, symbol: Symbol::Btc };

fn cb_snap(product: &str, mid: f64, nb: usize, na: usize) -> String {
    let bids: Vec<_> = (0..nb).map(|i| [format!("{:.2}", mid - 0.01 * (i as f64 + 1.0)), format!("{:.8}", 0.5)]).collect();
    let asks: Vec<_> = (0..na).map(|i| [format!("{:.2}", mid + 0.01 * (i as f64 + 1.0)), format!("{:.8}", 0.25)]).collect();
    serde_json::json!({"type": "snapshot", "product_id": product, "bids": bids, "asks": asks}).to_string()
}
fn cb_up(product: &str, changes: serde_json::Value) -> String {
    serde_json::json!({"type": "l2update", "product_id": product, "changes": changes, "time": "2026-11-03T20:56:33.841784Z"}).to_string()
}
fn feed_cb(book: &mut BookSync, t: &str) -> Result<bool, String> {
    read_event(Some(Ok(t.to_string())), book)
}

#[test]
fn m3a_identity_exchange_inclusive() {
    let b = supported("binance", "spot", "BTC").unwrap();
    let c = supported("coinbase", "spot", "BTC").unwrap();
    assert_eq!((b.market, b.symbol), (c.market, c.symbol));
    assert_ne!(b, c, "same market+symbol, distinct exchange => distinct producer");
    assert_eq!(c.venue(), Venue::Coinbase);
    assert_eq!(instrument(c).unwrap().native_symbol, "BTC-USD");
    assert_eq!(instrument(supported("coinbase", "spot", "ETH").unwrap()).unwrap().native_symbol, "ETH-USD");
    assert_eq!(instrument(b).unwrap().native_symbol, "BTCUSDT");
    // INTX retired, unregistered markets/symbols unsupported (DRB: m3b_*).
    assert!(supported("coinbase", "usdc", "SOL").is_none());
    assert!(supported("coinbase", "usdm", "BTC").is_none());
    assert!(supported("coinbase", "spot", "SOL").is_none());
    assert_eq!(c.label(), "coinbase/spot/BTC");
    assert_eq!(c.contract_size(), None);
    let f: serde_json::Value = serde_json::from_str(&frame(c, BookStatus::Syncing, 1, None, None).json).unwrap();
    assert_eq!(f["source"]["exchange"], "coinbase");
    assert_eq!(f["quote"], "USD");
    assert_eq!(f["seq_kind"], "internal_revision");
}

#[test]
fn m3a_large_snapshot_bounded_window_then_absolute_updates() {
    let mut b = BookSync::with_venue("BTC-USD", CB.venue());
    assert_eq!(feed_cb(&mut b, r#"{"type":"subscriptions","channels":[]}"#), Ok(false));
    // update before snapshot => resync
    let mut early = BookSync::with_venue("BTC-USD", CB.venue());
    assert_eq!(feed_cb(&mut early, &cb_up("BTC-USD", serde_json::json!([["buy", "1.00", "1"]]))), Err("bridgegap".into()));
    let snap = cb_snap("BTC-USD", 70000.0, 21767, 18986);
    assert!(snap.len() > 1_000_000 && snap.len() < COINBASE_MAX_MESSAGE_BYTES);
    assert_eq!(feed_cb(&mut b, &snap), Ok(false));
    assert!(!b.is_live(), "no exchange time on snapshot: stay syncing");
    assert_eq!((b.bids.len(), b.asks.len()), (5000, 5000));
    assert_eq!(b.coverage(), (70000.0 - 50.0, 70000.0 + 50.0));
    // duplicate snapshot in one generation rejected
    let mut d = BookSync::with_venue("BTC-USD", CB.venue());
    feed_cb(&mut d, &cb_snap("BTC-USD", 100.0, 60, 60)).unwrap();
    assert_eq!(feed_cb(&mut d, &cb_snap("BTC-USD", 100.0, 60, 60)), Err("badsnapshot".into()));
    // absolute replacement (not delta), zero deletes, outside window ignored
    let mut b = BookSync::with_venue("BTC-USD", CB.venue());
    feed_cb(&mut b, &snap).unwrap();
    let up = cb_up("BTC-USD", serde_json::json!([["buy", "69999.99", "3.25"], ["sell", "70000.01", "0"], ["buy", "60000.00", "9"]]));
    assert_eq!(feed_cb(&mut b, &up), Ok(true));
    assert!(b.is_live());
    assert_eq!(b.last_update_id(), Some(1), "internal revision, not exchange seq");
    assert_eq!(b.last_event_ms, 1_793_739_393_841);
    let (bids, asks) = b.top(2);
    assert_eq!(bids[0], [69999.99, 3.25]);
    assert_eq!(asks[0][0], 70000.02);
    assert!(!b.bids.contains_key(&parse_price("60000").unwrap()), "outside window stays unknown");
    feed_cb(&mut b, &cb_up("BTC-USD", serde_json::json!([["buy", "69999.99", "1.5"]]))).unwrap();
    assert_eq!(b.top(1).0[0], [69999.99, 1.5], "absolute size replaces");
    assert_eq!(b.last_update_id(), Some(2));
    assert_eq!(b.published_coverage(5000), b.coverage());
}

#[test]
fn m3a_rejects_bad_shapes_and_products() {
    let ok = cb_snap("BTC-USD", 100.0, 60, 60);
    let mut b = BookSync::with_venue("BTC-USD", CB.venue());
    assert_eq!(feed_cb(&mut b, &cb_snap("ETH-USD", 100.0, 60, 60)), Err("sourcemismatch".into()));
    let mut b = BookSync::with_venue("BTC-USD", CB.venue());
    feed_cb(&mut b, &ok).unwrap();
    for bad in [
        cb_up("ETH-USD", serde_json::json!([["buy", "99.99", "1"]])),
        cb_up("BTC-USD", serde_json::json!([["hold", "99.99", "1"]])),
        cb_up("BTC-USD", serde_json::json!([["buy", "-1", "1"]])),
        cb_up("BTC-USD", serde_json::json!([["buy", "99.99", "NaN"]])),
        cb_up("BTC-USD", serde_json::json!([["buy", "100.50", "1"]])), // crosses
        r#"{"type":"l2update","product_id":"BTC-USD","changes":[],"time":"not-a-time"}"#.to_string(),
        r#"{"type":"error","message":"x"}"#.to_string(),
        r#"{"type":"ticker"}"#.to_string(),
        "garbage".to_string(),
    ] {
        let mut c = BookSync::with_venue("BTC-USD", CB.venue());
        feed_cb(&mut c, &ok).unwrap();
        assert!(feed_cb(&mut c, &bad).is_err(), "{bad}");
        assert!(!c.is_live());
    }
    // crossed / one-sided snapshots and oversized message rejected
    let mut c = BookSync::with_venue("BTC-USD", CB.venue());
    assert!(feed_cb(&mut c, &cb_snap("BTC-USD", 100.0, 60, 0)).is_err());
    let mut c = BookSync::with_venue("BTC-USD", CB.venue());
    assert_eq!(feed_cb(&mut c, &"x".repeat(COINBASE_MAX_MESSAGE_BYTES + 1)), Err("oversized_input".into()));
    // Binance-shaped diff never admitted into a Coinbase book
    let mut c = BookSync::with_venue("BTC-USD", CB.venue());
    feed_cb(&mut c, &ok).unwrap();
    assert!(c.on_event(serde_json::from_str(&ev_json("BTC-USD", 1, 1, 0, ("99.99", "1"))).unwrap()).is_err());
}

#[tokio::test(start_paused = true)]
async fn m3a_hub_generation_resync_and_cross_exchange_isolation() {
    let (ctx1, crx1) = mpsc::unbounded_channel();
    let (ctx2, crx2) = mpsc::unbounded_channel();
    let cfeed = Arc::new(ReplayFeed { streams: StdMutex::new(vec![crx2, crx1]), snapshots: StdMutex::new(vec![]) });
    let (btx, brx) = mpsc::unbounded_channel();
    let bfeed = Arc::new(ReplayFeed { streams: StdMutex::new(vec![brx]), snapshots: StdMutex::new(vec![snap_json(10, 50000.0)]) });
    let chub = BookHub::new(cfeed.clone());
    let bhub = BookHub::new(bfeed);
    let mut cs = chub.subscribe(CB);
    let mut bs = bhub.subscribe(SB);
    ctx1.send(Ok(cb_snap("BTC-USD", 70000.0, 6000, 6000))).unwrap();
    ctx1.send(Ok(cb_up("BTC-USD", serde_json::json!([["buy", "69999.99", "7"]])))).unwrap();
    btx.send(Ok(serde_json::json!({"e":"depthUpdate","E":1_700_000_000_011i64,"s":"BTCUSDT","U":11,"u":11,"b":[["49999.95","4"]],"a":[]}).to_string())).unwrap();
    let f = wait_status(&mut cs.rx, BookStatus::Live).await;
    let v: serde_json::Value = serde_json::from_str(&f.json).unwrap();
    assert_eq!(v["source"]["exchange"], "coinbase");
    assert_eq!(v["bids"][0], serde_json::json!([69999.99, 7.0]));
    assert_eq!(v["bids"].as_array().unwrap().len(), 5000);
    assert_eq!(v["seq"], 1);
    assert_eq!(f.generation, 1);
    let g = wait_status(&mut bs.rx, BookStatus::Live).await;
    let w: serde_json::Value = serde_json::from_str(&g.json).unwrap();
    assert_eq!(w["source"]["exchange"], "binance");
    assert_eq!(w["native_symbol"], "BTCUSDT");
    assert!(w["bids"][0][0].as_f64().unwrap() < 50001.0);
    // Malformed Coinbase input resyncs Coinbase only, with a fresh generation.
    ctx1.send(Ok("garbage".into())).unwrap();
    let s = wait_status(&mut cs.rx, BookStatus::Stale).await;
    assert_eq!(s.generation, 1);
    ctx2.send(Ok(cb_snap("BTC-USD", 71000.0, 100, 100))).unwrap();
    ctx2.send(Ok(cb_up("BTC-USD", serde_json::json!([["sell", "71000.01", "2"]])))).unwrap();
    let f2 = wait_status(&mut cs.rx, BookStatus::Live).await;
    assert_eq!(f2.generation, 2);
    let v2: serde_json::Value = serde_json::from_str(&f2.json).unwrap();
    assert_eq!(v2["asks"][0], serde_json::json!([71000.01, 2.0]));
    assert_eq!(v2["seq"], 1, "revision is generation-scoped");
    assert_eq!(bs.rx.borrow().generation, 1, "Binance producer untouched");
    assert_eq!(bs.rx.borrow().status, BookStatus::Live);
    assert_eq!(cfeed.snapshots.lock().unwrap().len(), 0, "no REST snapshot used");
}

/// Opt-in bounded REAL anonymous Coinbase Exchange smoke (both spot sources).
#[tokio::test]
#[ignore = "network: CVD_BOOK_LIVE_COINBASE=1"]
async fn live_coinbase_spot_btc_eth_sync() {
    let feed = BinanceFeed { http: reqwest::Client::new() };
    let mut out = Vec::new();
    for s in ["BTC", "ETH"] {
        let k = supported("coinbase", "spot", s).unwrap();
        let native = instrument(k).unwrap().native_symbol;
        let t0 = std::time::Instant::now();
        let mut ws = tokio::time::timeout(CONNECT_DEADLINE, feed.connect(k, native)).await.unwrap().unwrap();
        let mut book = BookSync::with_venue(native, k.venue());
        let (mut snap_bytes, mut applied) = (0usize, 0u32);
        let r = tokio::time::timeout(Duration::from_secs(20), async {
            while applied < 20 {
                let m = ws.next().await;
                if let Some(Ok(t)) = &m {
                    if t.contains("\"type\":\"snapshot\"") {
                        snap_bytes = t.len();
                    }
                }
                if read_event(m, &mut book)? {
                    applied += 1;
                }
            }
            Ok::<_, String>(())
        })
        .await;
        assert_eq!(r, Ok(Ok(())), "{s}");
        let f: serde_json::Value = serde_json::from_str(&frame(k, BookStatus::Live, 1, Some(&book), None).json).unwrap();
        let row = serde_json::json!({"key": k.label(), "native": native, "snapshot_message_bytes": snap_bytes,
            "maintained_levels": [book.bids.len(), book.asks.len()], "published": [f["bids"].as_array().unwrap().len(), f["asks"].as_array().unwrap().len()],
            "applied_updates": applied, "seq_kind": f["seq_kind"], "seq": f["seq"], "exchange_time_ms": f["exchange_time_ms"],
            "best_bid": f["bids"][0][0], "best_ask": f["asks"][0][0], "coverage": f["coverage"], "quote": f["quote"], "elapsed_ms": t0.elapsed().as_millis() as u64});
        assert!(book.is_live() && f["bids"][0][0].as_f64() < f["asks"][0][0].as_f64());
        println!("LIVE_COINBASE {row}");
        out.push(row);
    }
    if let Ok(p) = std::env::var("CVD_BOOK_LIVE_OUT") {
        std::fs::write(p, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
}

// ------------------------------------------- M3A closure: nonpositive time
// Producer/consumer consistency: the SDK admission (`BookSnapshot::check`)
// rejects exchange_time_ms <= 0, so the reducer must reject such an update
// BEFORE any mutation and never publish a live frame with nonpositive time.

fn cb_up_t(product: &str, changes: serde_json::Value, time: &str) -> String {
    serde_json::json!({"type": "l2update", "product_id": product, "changes": changes, "time": time}).to_string()
}

fn sdk_admits(key: BookKey, b: &BookSync) -> Result<(), String> {
    let f: serde_json::Value = serde_json::from_str(&frame(key, BookStatus::Live, 1, Some(b), None).json).unwrap();
    let s: cvd_indicator_sdk::book::BookSnapshot = serde_json::from_value(f).map_err(|e| e.to_string())?;
    s.check().map_err(|e| format!("{e:?}"))
}

#[test]
fn m3a_closure_nonpositive_or_bad_time_rejected_before_mutation() {
    let snap = cb_snap("BTC-USD", 100.0, 60, 60);
    let ch = serde_json::json!([["buy", "99.99", "7"]]);
    for bad in ["1970-01-01T00:00:00Z", "1969-12-31T23:59:59.000Z", "1900-01-01T00:00:00Z", "not-a-time"] {
        // (a) first update after snapshot: must stay syncing, untouched.
        let mut b = BookSync::with_venue("BTC-USD", CB.venue());
        feed_cb(&mut b, &snap).unwrap();
        let before = (b.bids.clone(), b.asks.clone(), b.last_event_ms, b.last_update_id());
        assert_eq!(feed_cb(&mut b, &cb_up_t("BTC-USD", ch.clone(), bad)), Err("badlevel".into()), "{bad}");
        assert!(!b.is_live(), "{bad}: no live state from nonpositive time");
        assert_eq!((b.bids.clone(), b.asks.clone(), b.last_event_ms, b.last_update_id()), before, "{bad}");
        // (b) after live state: reject without advancing revision/time/levels.
        let mut b = BookSync::with_venue("BTC-USD", CB.venue());
        feed_cb(&mut b, &snap).unwrap();
        feed_cb(&mut b, &cb_up("BTC-USD", serde_json::json!([["buy", "99.99", "2"]]))).unwrap();
        assert!(b.is_live());
        sdk_admits(CB, &b).expect("valid live book admitted by real SDK");
        let before = (b.bids.clone(), b.asks.clone(), b.last_event_ms, b.last_update_id());
        assert_eq!(feed_cb(&mut b, &cb_up_t("BTC-USD", ch.clone(), bad)), Err("badlevel".into()), "{bad}");
        assert_eq!((b.bids.clone(), b.asks.clone(), b.last_event_ms, b.last_update_id()), before, "{bad}");
        assert!(b.last_event_ms > 0);
        sdk_admits(CB, &b).expect("state after rejection still SDK-admissible");
    }
    // Valid positive time: zero-size delete + absolute update still live and admitted.
    let mut b = BookSync::with_venue("BTC-USD", CB.venue());
    feed_cb(&mut b, &snap).unwrap();
    assert_eq!(feed_cb(&mut b, &cb_up_t("BTC-USD", serde_json::json!([["sell", "100.01", "0"], ["buy", "99.99", "3"]]), "1970-01-01T00:00:00.001Z")), Ok(true));
    assert_eq!(b.last_event_ms, 1);
    sdk_admits(CB, &b).expect("positive 1ms admitted");
}

// ------------------------------------------- M3B: Coinbase DRB linear USDC

fn drb_msg(native: &str, data: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "method": "subscription", "params": {"channel": format!("book.{native}.100ms"), "data": data}}).to_string()
}

fn drb_snap(native: &str, mid: f64, nb: usize, na: usize, cid: u64) -> String {
    let bids: Vec<_> = (1..=nb).map(|i| serde_json::json!(["new", mid - i as f64 * 0.5, 1.5])).collect();
    let asks: Vec<_> = (1..=na).map(|i| serde_json::json!(["new", mid + i as f64 * 0.5, 2.5])).collect();
    drb_msg(native, serde_json::json!({"type": "snapshot", "timestamp": 1793739394083i64, "change_id": cid, "instrument_name": native, "bids": bids, "asks": asks}))
}

fn drb_chg(native: &str, prev: u64, cid: u64, bids: serde_json::Value, asks: serde_json::Value) -> String {
    drb_msg(native, serde_json::json!({"type": "change", "timestamp": 1793739394183i64, "prev_change_id": prev, "change_id": cid, "instrument_name": native, "bids": bids, "asks": asks}))
}

#[test]
fn m3b_identity_units_and_frame() {
    for (s, native) in [("BTC", "BTC_USDC-PERPETUAL"), ("ETH", "ETH_USDC-PERPETUAL")] {
        let k = supported("coinbase", "usdc", s).unwrap();
        assert_eq!(k.venue(), Venue::Drb);
        assert_eq!(instrument(k).unwrap().native_symbol, native);
        assert_ne!(Some(k), supported("coinbase", "spot", s), "DRB vs spot distinct producers");
        // Linear base units: metadata contract_size 0.0001 is NOT a multiplier.
        assert_eq!(k.contract_size(), None);
        assert_eq!(base_qty(k, 70000.0, 1.25), 1.25);
        let mut b = BookSync::with_venue(native, k.venue());
        assert_eq!(read_event(Some(Ok(drb_snap(native, 1000.0, 60, 60, 5))), &mut b), Ok(true));
        let f: serde_json::Value = serde_json::from_str(&frame(k, BookStatus::Live, 1, Some(&b), None).json).unwrap();
        assert_eq!(f["quote"], "USDC");
        assert_eq!(f["native_symbol"], native);
        assert_eq!(f["raw_quantity"], "base");
        assert_eq!(f["seq_kind"], "exchange_update_id");
        assert_eq!(f["seq"], 5);
        assert_eq!(f["source"]["market"], "usdc");
        assert_eq!(f["bids"][0], serde_json::json!([999.5, 1.5]));
        assert_eq!(f["asks"][0], serde_json::json!([1000.5, 2.5]));
        sdk_admits(k, &b).expect("DRB frame admitted by real SDK");
    }
    assert!(supported("coinbase", "usdc", "SOL").is_none());
    assert!(supported("coinbase", "usdm", "BTC").is_none());
}

#[test]
fn m3b_change_chain_jump_gap_and_actions() {
    let n = "BTC_USDC-PERPETUAL";
    let mut b = BookSync::with_venue(n, Venue::Drb);
    // ack (exact channel) is control
    assert_eq!(read_event(Some(Ok(format!(r#"{{"jsonrpc":"2.0","id":1,"result":["book.{n}.100ms"]}}"#))), &mut b), Ok(false));
    // change before snapshot
    assert_eq!(read_event(Some(Ok(drb_chg(n, 1, 2, serde_json::json!([]), serde_json::json!([])))), &mut b), Err("bridgegap".into()));
    let mut b = BookSync::with_venue(n, Venue::Drb);
    read_event(Some(Ok(drb_snap(n, 1000.0, 60, 60, 100))), &mut b).unwrap();
    // jump in change_id with correct prev accepted; absolute overwrite + delete
    let c = drb_chg(n, 100, 7777, serde_json::json!([["change", 999.5, 9.0], ["delete", 999.0, 0.0]]), serde_json::json!([["new", 1000.25, 0.1]]));
    assert_eq!(read_event(Some(Ok(c)), &mut b), Ok(true));
    assert_eq!(b.last_update_id(), Some(7777));
    assert_eq!(b.bids.get(&parse_price("999.5").unwrap()), Some(&9.0));
    assert!(!b.bids.contains_key(&parse_price("999").unwrap()));
    assert_eq!(b.top(1).1[0], [1000.25, 0.1]);
    // wrong prev => gap; state untouched
    let before = (b.bids.clone(), b.asks.clone(), b.last_update_id());
    assert_eq!(read_event(Some(Ok(drb_chg(n, 7776, 7778, serde_json::json!([["change", 999.5, 1.0]]), serde_json::json!([])))), &mut b), Err("sequencegap".into()));
    assert_eq!((b.bids.clone(), b.asks.clone(), b.last_update_id()), before);
    // malformed: bad action, zero new, negative, wrong instrument/channel, bad time, second snapshot
    for bad in [
        drb_chg(n, 7777, 7778, serde_json::json!([["upsert", 999.5, 1.0]]), serde_json::json!([])),
        drb_chg(n, 7777, 7778, serde_json::json!([["new", 999.5, 0.0]]), serde_json::json!([])),
        drb_chg(n, 7777, 7778, serde_json::json!([["change", 999.5, -1.0]]), serde_json::json!([])),
        drb_chg("ETH_USDC-PERPETUAL", 7777, 7778, serde_json::json!([]), serde_json::json!([])),
        drb_msg(n, serde_json::json!({"type": "change", "timestamp": 0, "prev_change_id": 7777, "change_id": 7778, "instrument_name": n, "bids": [], "asks": []})),
        drb_snap(n, 1000.0, 60, 60, 9000),
        serde_json::json!({"jsonrpc": "2.0", "method": "subscription", "params": {"channel": format!("book.{n}.none.20.100ms"), "data": {}}}).to_string(),
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":1}}"#.to_string(),
    ] {
        let mut c = BookSync::with_venue(n, Venue::Drb);
        read_event(Some(Ok(drb_snap(n, 1000.0, 60, 60, 7777))), &mut c).unwrap();
        let before = (c.bids.clone(), c.asks.clone(), c.last_update_id(), c.last_event_ms);
        assert!(read_event(Some(Ok(bad.clone())), &mut c).is_err(), "{bad}");
        assert_eq!((c.bids.clone(), c.asks.clone(), c.last_update_id(), c.last_event_ms), before);
    }
    // nonpositive snapshot time never goes live
    let mut c = BookSync::with_venue(n, Venue::Drb);
    let s = drb_snap(n, 1000.0, 60, 60, 1).replace("1793739394083", "0");
    assert!(read_event(Some(Ok(s)), &mut c).is_err());
    assert!(!c.is_live());
}

#[test]
fn m3b_bounded_window_deep_and_outside() {
    let n = "ETH_USDC-PERPETUAL";
    let mut b = BookSync::with_venue(n, Venue::Drb);
    read_event(Some(Ok(drb_snap(n, 10000.0, 6000, 5200, 1))), &mut b).unwrap();
    assert_eq!((b.bids.len(), b.asks.len()), (5000, 5000));
    let k = supported("coinbase", "usdc", "ETH").unwrap();
    let f: serde_json::Value = serde_json::from_str(&frame(k, BookStatus::Live, 1, Some(&b), None).json).unwrap();
    assert_eq!(f["bids"].as_array().unwrap().len(), 5000);
    assert_eq!(f["bids"][1200][0], 10000.0 - 1201.0 * 0.5, "deep >1000 level published");
    assert_eq!(f["coverage"]["bid_floor"], 10000.0 - 5000.0 * 0.5);
    // update outside anchored window ignored (unknown territory)
    read_event(Some(Ok(drb_chg(n, 1, 2, serde_json::json!([["new", 10000.0 - 5500.0 * 0.5, 9.0]]), serde_json::json!([])))), &mut b).unwrap();
    assert_eq!(b.bids.len(), 5000);
    assert!(!b.bids.contains_key(&parse_price(&format!("{}", 10000.0 - 5500.0 * 0.5)).unwrap()));
}

/// Opt-in bounded REAL anonymous Coinbase DRB smoke (BTC+ETH USDC perps).
#[tokio::test]
#[ignore = "network: real public Coinbase DRB"]
async fn live_coinbase_drb_btc_eth_sync() {
    let feed = BinanceFeed { http: reqwest::Client::new() };
    let mut out = Vec::new();
    for s in ["BTC", "ETH"] {
        let k = supported("coinbase", "usdc", s).unwrap();
        let native = instrument(k).unwrap().native_symbol;
        let t0 = std::time::Instant::now();
        let mut ws = tokio::time::timeout(CONNECT_DEADLINE, feed.connect(k, native)).await.unwrap().unwrap();
        let mut book = BookSync::with_venue(native, k.venue());
        let (mut snap_bytes, mut applied, mut msgs, mut first_cid) = (0usize, 0u32, 0u32, None);
        let r = tokio::time::timeout(Duration::from_secs(20), async {
            while applied < 20 {
                let m = ws.next().await;
                msgs += 1;
                if let Some(Ok(t)) = &m {
                    if t.contains("\"type\":\"snapshot\"") {
                        snap_bytes = t.len();
                    }
                }
                if read_event(m, &mut book)? {
                    applied += 1;
                    first_cid.get_or_insert(book.last_update_id());
                }
            }
            Ok::<_, String>(())
        })
        .await;
        assert_eq!(r, Ok(Ok(())), "{s}");
        let f: serde_json::Value = serde_json::from_str(&frame(k, BookStatus::Live, 1, Some(&book), None).json).unwrap();
        sdk_admits(k, &book).expect("live DRB admitted");
        let row = serde_json::json!({"key": k.label(), "native": native, "snapshot_message_bytes": snap_bytes, "messages": msgs,
            "maintained_levels": [book.bids.len(), book.asks.len()], "published": [f["bids"].as_array().unwrap().len(), f["asks"].as_array().unwrap().len()],
            "applied": applied, "first_change_id": first_cid, "seq_kind": f["seq_kind"], "seq": f["seq"], "exchange_time_ms": f["exchange_time_ms"],
            "raw_quantity": f["raw_quantity"], "contract_size_quote": f["contract_size_quote"], "quote": f["quote"],
            "best_bid": f["bids"][0], "best_ask": f["asks"][0], "coverage": f["coverage"], "elapsed_ms": t0.elapsed().as_millis() as u64});
        assert!(book.is_live() && f["bids"][0][0].as_f64() < f["asks"][0][0].as_f64());
        println!("LIVE_DRB {row}");
        out.push(row);
    }
    if let Ok(p) = std::env::var("CVD_BOOK_LIVE_OUT") {
        std::fs::write(p, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    }
}

#[test]
fn m4_drb_jsonrpc_envelope_and_ack_id() {
    let n = "BTC_USDC-PERPETUAL";
    let ch = format!("book.{n}.100ms");
    // correct ack id 1 is control
    let mut b = BookSync::with_venue(n, Venue::Drb);
    assert_eq!(read_event(Some(Ok(format!(r#"{{"jsonrpc":"2.0","id":1,"result":["{ch}"]}}"#))), &mut b), Ok(false));
    // documented extra fields on id-less notification are accepted
    let mut ok = serde_json::from_str::<serde_json::Value>(&drb_snap(n, 1000.0, 60, 60, 7777)).unwrap();
    ok["usIn"] = 1.into();
    ok["usOut"] = 2.into();
    ok["testnet"] = false.into();
    assert_eq!(read_event(Some(Ok(ok.to_string())), &mut b), Ok(true));
    let snap = drb_snap(n, 1000.0, 60, 60, 7777);
    let chg = drb_chg(n, 7777, 7778, serde_json::json!([["change", 999.5, 1.0]]), serde_json::json!([]));
    let mut bad: Vec<String> = vec![
        format!(r#"{{"id":1,"result":["{ch}"]}}"#),
        format!(r#"{{"jsonrpc":"1.0","id":1,"result":["{ch}"]}}"#),
        format!(r#"{{"jsonrpc":"2.0","result":["{ch}"]}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":2,"result":["{ch}"]}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":"1","result":["{ch}"]}}"#),
    ];
    for v in [None, Some("1.0"), Some("2")] {
        let mut m = serde_json::from_str::<serde_json::Value>(&chg).unwrap();
        match v {
            None => { m.as_object_mut().unwrap().remove("jsonrpc"); }
            Some(s) => m["jsonrpc"] = s.into(),
        }
        bad.push(m.to_string());
    }
    for x in bad {
        let mut c = BookSync::with_venue(n, Venue::Drb);
        read_event(Some(Ok(snap.clone())), &mut c).unwrap();
        let before = (c.bids.clone(), c.asks.clone(), c.last_update_id(), c.last_event_ms, c.is_live());
        assert!(read_event(Some(Ok(x.clone())), &mut c).is_err(), "{x}");
        assert_eq!((c.bids.clone(), c.asks.clone(), c.last_update_id(), c.last_event_ms, c.is_live()), before);
    }
}
