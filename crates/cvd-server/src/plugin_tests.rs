//! Host ⇄ runner ⇄ library integration through the real server code paths
//! (history attachment, live hubs, supervision, endpoints). Uses only the
//! public sample library and test fixtures; no network, no PostgreSQL
//! connection (the pool is lazy and never used here).

use super::*;
use crate::indicators::{compute_history, compute_history_guarded, IndicatorManager, ScopeBase, Status};
use cvd_indicator_sdk::{codes, SeriesValues};
use std::future::IntoFuture;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

pub(crate) fn artifacts() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let status = std::process::Command::new(env!("CARGO"))
            .current_dir(&root)
            .args([
                "build",
                "--offline",
                "-p",
                "cvd-indicator-runner",
                "-p",
                "sample-indicator",
                "-p",
                "fault-indicator",
                "-p",
                "bad-abi-indicator",
                "-p",
                "zero-context-indicator",
            ])
            .status()
            .expect("cargo build");
        assert!(status.success());
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
        (target.join("debug/cvd-indicator-runner"), target.join("debug"))
    })
}

pub(crate) fn config(libs: &[(&str, &str, &str, &str)], live_timeout_ms: u64) -> config::Config {
    let (runner, dir) = artifacts();
    let mut text = format!(
        "[indicators]\nrunner = {:?}\nhistory_timeout_ms = 3000\nlive_timeout_ms = {live_timeout_ms}\n",
        runner.to_str().unwrap()
    );
    for (id, lib, params, modes) in libs {
        text += &format!(
            "[[indicators.library]]\nid = {id:?}\npath = {:?}\nparams = {params}\nmodes = {modes}\n",
            dir.join(format!("lib{lib}.so")).to_str().unwrap()
        );
    }
    config::Config::parse(&text).unwrap()
}

pub(crate) async fn wait_status(mgr: &IndicatorManager, id: &str, want: Status, secs: u64) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if mgr.infos().iter().any(|i| i.id == id && i.status == want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{id} did not reach {want:?}: {:?}", mgr.infos());
}

fn price_bar(i: i64) -> Bar {
    let mut b = tests::bar(i * 60 - 1, i as f64 * 3.0);
    let p = 100.0 + (i as f64 / 5.0).sin() * 4.0;
    b.price_open = p - 0.5;
    b.price_high = p + 1.0;
    b.price_low = p - 1.0;
    b.price_close = p;
    b.aggregate_price_close = p + 0.25;
    b.delta_usdt = (i % 9) as f64 - 4.0;
    b.aggregate_delta_usdt = 2.0 * b.delta_usdt;
    b
}

fn values(out: &cvd_indicator_sdk::Output, key: &str) -> Vec<Option<f64>> {
    match &out.series[key] {
        SeriesValues::Line { values } | SeriesValues::Histogram { values } => values.clone(),
        _ => panic!(),
    }
}

#[test]
fn canonical_bar_wire_carries_no_indicator_expansions() {
    let v = serde_json::to_value(price_bar(3)).unwrap();
    for k in ["lt", "lb", "alt", "alb", "levels_top", "levels_bottom"] {
        assert!(v.get(k).is_none(), "{k} must not be on the canonical bar");
    }
    for k in [
        "time",
        "po",
        "ph",
        "pl",
        "price_close",
        "apo",
        "aph",
        "apl",
        "apc",
        "delta_usdt",
        "ad",
        "cvd_usdt",
        "ac",
        "bvb",
        "abvb",
        "svb",
        "asvb",
        "buy_volume_usdt",
        "agb",
        "sell_volume_usdt",
        "ags",
        "trades",
    ] {
        assert!(v.get(k).is_some(), "{k} retained");
    }
}

#[tokio::test]
async fn history_attaches_versioned_outputs_using_declared_warmup() {
    let mgr = IndicatorManager::start(&config(
        &[("sample.channel", "sample_indicator", "{ length = 20 }", "['source', 'aggregate']")],
        2000,
    ));
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    let ids = vec!["sample.channel".to_string()];
    assert_eq!(mgr.max_context(&ids, "1m"), 500, "context from descriptor, not host constants");
    let bars: Vec<Bar> = (1..=900).map(price_bar).collect();
    let keep = history_keep_start(&bars, 300, None);
    assert_eq!(keep, 600);
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    let out = compute_history(&mgr, &ids, &scope, 0, &bars, keep).await;
    let ind = &out["sample.channel"];
    assert_eq!(ind.version.as_deref(), Some("1.0.0"));
    let src = ind.modes["source"].output.as_ref().expect("source output");
    assert_eq!(src.times, bars[600..].iter().map(|b| b.time).collect::<Vec<_>>());
    let expect = bars[581..=600].iter().map(|b| b.price_close).sum::<f64>() / 20.0;
    assert!((values(src, "sma")[0].unwrap() - expect).abs() < 1e-9, "warm-up bars were supplied");
    assert_eq!(values(src, "delta")[0], Some(bars[600].delta_usdt), "projects persisted delta");
    let agg = ind.modes["aggregate"].output.as_ref().unwrap();
    assert_eq!(values(agg, "delta")[0], Some(bars[600].aggregate_delta_usdt));

    // `after` paging keeps only newer bars; context still comes from older rows.
    assert_eq!(history_keep_start(&bars, 300, Some(bars[700].time)), 701);

    // Unknown / path-like ids are never resolved.
    assert_eq!(requested_indicators(&mgr, Some("../../x.so,sample.channel,nope,sample.channel")), ids);
    assert!(requested_indicators(&mgr, Some("")).is_empty());
    assert_eq!(requested_indicators(&mgr, None), ids);
}

#[tokio::test]
async fn failed_and_incompatible_indicators_are_reported_without_diagnostics() {
    let mgr = IndicatorManager::start(&config(
        &[
            ("test.bad", "bad_abi_indicator", "{}", "['source']"),
            ("wrong.id", "sample_indicator", "{}", "['source']"),
            ("test.fault", "fault_indicator", "{ fault = 'error' }", "['source']"),
        ],
        2000,
    ));
    wait_status(&mgr, "test.bad", Status::Unavailable, 10).await;
    wait_status(&mgr, "wrong.id", Status::Unavailable, 10).await;
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    let state = Arc::new(tests::state_with(mgr.clone()).await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app_router(state)).into_future());
    let client = market_http_client().unwrap();
    let body: serde_json::Value =
        client.get(format!("http://{addr}/indicators")).send().await.unwrap().json().await.unwrap();
    let list = body["indicators"].as_array().unwrap();
    let find = |id: &str| list.iter().find(|i| i["id"] == id).unwrap().clone();
    assert_eq!(find("test.bad")["status"], "unavailable");
    assert_eq!(body["api_version"], 1);
    assert_eq!(find("test.bad")["error"]["code"], codes::INCOMPATIBLE);
    assert_eq!(find("wrong.id")["error"]["code"], codes::INCOMPATIBLE);
    assert_eq!(find("test.fault")["status"], "ready");
    assert_eq!(find("test.fault")["descriptor"]["outputs"][0]["type"], "line");
    let text = body.to_string();
    assert!(
        !text.contains("/target/") && !text.contains(".so") && !text.contains("abi 99"),
        "no local diagnostics: {text}"
    );
    let meta: serde_json::Value = client.get(format!("http://{addr}/meta")).send().await.unwrap().json().await.unwrap();
    assert_eq!(meta["indicators"].as_array().unwrap().len(), 3);
    let health: serde_json::Value =
        client.get(format!("http://{addr}/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["indicators"].as_array().unwrap().len(), 3);
    server.abort();

    // A plugin error marks that output unavailable but the runner stays ready.
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    let bars: Vec<Bar> = (1..=10).map(price_bar).collect();
    let out = compute_history(&mgr, &["test.fault".into(), "test.bad".into()], &scope, 0, &bars, 5).await;
    assert_eq!(out["test.fault"].modes["source"].status, "unavailable");
    assert_eq!(out["test.fault"].modes["source"].error.as_ref().unwrap().code, codes::COMPUTE_FAILED);
    assert_eq!(out["test.bad"].modes["source"].error.as_ref().unwrap().code, codes::UNAVAILABLE);
    assert_eq!(mgr.infos().iter().find(|i| i.id == "test.fault").unwrap().status, Status::Ready);
}

async fn next_msg(
    sub: &mut live_indicators::Subscription,
    pred: impl Fn(&serde_json::Value) -> bool,
    secs: u64,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut skipped = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(msg) = tokio::time::timeout(left, sub.rx.recv()).await else {
            panic!("indicator message timeout; skipped: {skipped:#?}");
        };
        let msg = msg.unwrap();
        if std::env::var_os("CVD_TEST_TRACE").is_some() {
            eprintln!("[trace] {}", msg.chars().take(200).collect::<String>());
        }
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        if pred(&v) {
            return v;
        }
        skipped.push(msg.chars().take(220).collect::<String>());
    }
}

/// Waits for two frames matching `a` and `b` in either order.
async fn both_msgs(
    sub: &mut live_indicators::Subscription,
    a: impl Fn(&serde_json::Value) -> bool,
    b: impl Fn(&serde_json::Value) -> bool,
    secs: u64,
) -> (serde_json::Value, serde_json::Value) {
    let (mut ra, mut rb) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(secs);
    while ra.is_none() || rb.is_none() {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs().max(1);
        let v = next_msg(sub, |v| (ra.is_none() && a(v)) || (rb.is_none() && b(v)), left).await;
        if ra.is_none() && a(&v) {
            ra = Some(v);
        } else {
            rb = Some(v);
        }
    }
    (ra.unwrap(), rb.unwrap())
}

async fn seeded_state(mgr: Arc<IndicatorManager>, n: i64) -> Arc<AppState> {
    let state = Arc::new(tests::state_with(mgr).await);
    let shard = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
    let mut h = shard.hist_1m.write().await;
    for i in 1..=n {
        h.push_back(price_bar(i));
    }
    drop(h);
    state
}

const KEY: live_indicators::HubKey = (Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1);

#[tokio::test]
async fn live_hub_streams_append_and_replace_deltas_shared_by_viewers() {
    let mgr = IndicatorManager::start(&config(
        &[("sample.channel", "sample_indicator", "{ length = 3 }", "['source']")],
        2000,
    ));
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    let state = seeded_state(mgr, 40).await;
    let mut a = state.hubs.subscribe(&state, KEY).unwrap();
    let mut b = state.hubs.subscribe(&state, KEY).unwrap();
    assert_eq!(state.hubs.active(), 1, "viewers share one hub");
    let ok = next_msg(&mut a, |v| v["type"] == "indicator_status", 10).await;
    assert_eq!(ok["status"], "ok");
    let generation = ok["generation"].as_u64().unwrap();

    // Forming bar (append), then two corrections (replace), then the close.
    let mut forming = price_bar(41);
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, forming.clone()).await;
    let m = next_msg(&mut a, |v| v["type"] == "indicator" && v["seq"] == 1, 5).await;
    assert_eq!(m["output"]["times"], serde_json::json!([forming.time]));
    assert_eq!(m["generation"].as_u64().unwrap(), generation);
    for close in [500.0, 800.0] {
        forming.price_close = close;
        broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, forming.clone()).await;
    }
    let m = next_msg(
        &mut a,
        |v| v["type"] == "indicator" && v["output"]["series"]["sma"]["values"][0].as_f64().is_some_and(|x| x > 300.0),
        5,
    )
    .await;
    let h = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).hist_1m.read().await.clone();
    let expect = (h[38].price_close + h[39].price_close + 800.0) / 3.0;
    assert!(
        (m["output"]["series"]["sma"]["values"][0].as_f64().unwrap() - expect).abs() < 1e-9,
        "replace, not accumulate: {m}"
    );
    push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, forming.clone()).await;
    let next = price_bar(42);
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, next.clone()).await;
    // Close (replace) and next bucket (append) may coalesce into one delta whose
    // output starts at the first touched bar.
    let m = next_msg(
        &mut b,
        |v| {
            v["type"] == "indicator"
                && v["output"]["times"].as_array().and_then(|t| t.last()) == Some(&next.time.into())
        },
        5,
    )
    .await;
    assert_eq!(m["id"], "sample.channel");
    assert_eq!(m["mode"], "source");

    drop(a);
    drop(b);
    let t = Instant::now();
    while state.hubs.active() > 0 && t.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state.hubs.active(), 0, "hub stops without viewers");
}

#[tokio::test]
async fn hung_plugin_never_blocks_ingestion_and_is_replaced() {
    // trigger=2: live init (#1) succeeds, first update (#2) hangs.
    let mgr = IndicatorManager::start(&config(
        &[
            ("test.fault", "fault_indicator", "{ fault = 'hang', trigger = 2 }", "['source']"),
            ("sample.channel", "sample_indicator", "{ length = 3 }", "['source']"),
        ],
        400,
    ));
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    let state = seeded_state(mgr.clone(), 20).await;
    let mut base = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).tx_1m.subscribe();
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["id"] == "test.fault" && v["status"] == "ok", 10).await;

    // Ingestion keeps flowing at full speed while the plugin hangs.
    for i in 21..=60 {
        let t = Instant::now();
        broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, price_bar(i)).await;
        push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, price_bar(i)).await;
        assert!(t.elapsed() < Duration::from_millis(200), "ingestion awaited a plugin: {:?}", t.elapsed());
        assert_eq!(base.recv().await.unwrap().bar.time, price_bar(i).time);
        assert_eq!(base.recv().await.unwrap().bar.time, price_bar(i).time);
    }
    // Independent sessions have no relative delivery order. The healthy
    // update can arrive before the fault times out; preserve both receipts
    // instead of discarding the healthy one while waiting for `unavailable`.
    let (down, healthy) = both_msgs(
        &mut sub,
        |v| v["type"] == "indicator_status" && v["id"] == "test.fault" && v["status"] == "unavailable",
        |v| v["type"] == "indicator" && v["id"] == "sample.channel" && v["seq"].as_u64().is_some_and(|n| n > 0),
        10,
    )
    .await;
    assert!(matches!(down["error"]["code"].as_str(), Some(codes::TIMEOUT) | Some(codes::UNAVAILABLE)), "{down}");
    assert!(!healthy["output"]["times"].as_array().unwrap().is_empty(), "healthy session produced a real update");
    // Supervisor restarts the killed runner; the hub replays the window.
    wait_status(&mgr, "test.fault", Status::Ready, 15).await;
    assert!(mgr.infos().iter().find(|i| i.id == "test.fault").unwrap().restarts >= 1);
    next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["id"] == "test.fault" && v["status"] == "ok", 15).await;
}

#[tokio::test]
async fn aborting_plugin_is_contained_and_session_replayed_after_restart() {
    let mgr = IndicatorManager::start(&config(
        &[("test.fault", "fault_indicator", "{ fault = 'abort', trigger = 2 }", "['source']")],
        1000,
    ));
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    let state = seeded_state(mgr.clone(), 10).await;
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["status"] == "ok", 10).await;
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, price_bar(11)).await;
    let down = next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["status"] == "unavailable", 10).await;
    assert_eq!(down["error"]["code"], codes::CRASHED);
    assert!(!down.to_string().contains("stderr"), "raw diagnostics stay local");
    // History for the same indicator is unavailable or recovered, never a host failure.
    wait_status(&mgr, "test.fault", Status::Ready, 15).await;
    let up = next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["status"] == "ok", 15).await;
    assert_eq!(up["id"], "test.fault");
    // Ingestion state is intact.
    assert_eq!(pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).hist_1m.read().await.len(), 10);
}

#[tokio::test]
async fn host_runs_without_any_indicator_configuration() {
    let state = Arc::new(tests::state().await);
    assert!(state.hubs.subscribe(&state, KEY).is_none());
    let bars: Vec<Bar> = (1..=5).map(price_bar).collect();
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    assert!(compute_history(&state.indicators, &[], &scope, 0, &bars, 0).await.is_empty());
    assert_eq!(state.indicators.max_context(&[], "1m"), 0);
}

fn frame(v: &serde_json::Value) -> live_indicators::IndicatorFrame {
    serde_json::from_value(v.clone()).expect("frame matches typed IndicatorFrame schema")
}

#[tokio::test]
async fn frames_are_typed_fenced_and_revision_correlated() {
    let mgr = IndicatorManager::start(&config(
        &[
            ("sample.channel", "sample_indicator", "{ length = 3 }", "['source']"),
            ("test.fault", "fault_indicator", "{ fault = 'error', trigger = 2 }", "['source']"),
        ],
        2000,
    ));
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    let state = seeded_state(mgr, 20).await;
    let mut base = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).tx_1m.subscribe();
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    let st = next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["id"] == "sample.channel", 10).await;
    let live_indicators::IndicatorFrame::IndicatorStatus { api_version, session, .. } = frame(&st) else { panic!() };
    assert_eq!(api_version, 1);
    assert!(session > 0);

    let mut forming = price_bar(21);
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, forming.clone()).await;
    let rev1 = base.recv().await.unwrap().rev;
    forming.price_close = 777.0;
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, forming.clone()).await;
    let rev2 = base.recv().await.unwrap().rev;
    assert!(rev2 > rev1, "base frames carry a monotonic rev");
    // The output computed from the newest bar version reports as_of_rev >= rev2.
    // Frames of independent indicators are not mutually ordered (sessions
    // advance concurrently), so the fault's error status may precede the
    // healthy output: scan once for both instead of two sequential waits.
    let (m, e) = both_msgs(
        &mut sub,
        |v| v["type"] == "indicator" && v["id"] == "sample.channel" && v["as_of_rev"].as_u64() >= Some(rev2),
        |v| v["type"] == "indicator_status" && v["id"] == "test.fault" && v["status"] == "unavailable",
        5,
    )
    .await;
    let live_indicators::IndicatorFrame::Indicator { session: s2, seq, output, .. } = frame(&m) else { panic!() };
    assert_eq!(s2, session);
    assert!(seq >= 1);
    assert_eq!(output.times.last(), Some(&forming.time));

    // Library error text is replaced by a fixed public message.
    assert_eq!(e["error"]["code"], codes::COMPUTE_FAILED);
    assert_eq!(e["error"]["message"], "indicator computation failed");
    assert!(!e.to_string().contains("fixture"));
    // Re-opened session gets a new session id (seq restarts at 0).
    let re =
        next_msg(&mut sub, |v| v["type"] == "indicator_status" && v["id"] == "test.fault" && v["status"] == "ok", 6)
            .await;
    let live_indicators::IndicatorFrame::IndicatorStatus { session: s_new, .. } = frame(&re) else { panic!() };
    let first_fault = mgr_session_hint(&e);
    assert!(s_new > first_fault, "new session {s_new} must supersede {first_fault}");
}

fn mgr_session_hint(v: &serde_json::Value) -> u64 {
    v["session"].as_u64().unwrap()
}

/// Writes a REAL serialized envelope (server types + public sample library)
/// for browser tests when `CVD_EXPORT_WEB_FIXTURE=<path>` is set; always
/// validates the shapes.
#[tokio::test]
async fn export_web_envelope_fixture() {
    let mgr = IndicatorManager::start(&config(
        &[
            ("sample.channel", "sample_indicator", "{ length = 3 }", "['source', 'aggregate']"),
            ("test.fault", "fault_indicator", "{ fault = 'error' }", "['source']"),
        ],
        2000,
    ));
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    let ids = mgr.ids();
    let bars: Vec<Bar> = (1..=8).map(price_bar).collect();
    let keep = history_keep_start(&bars, 5, None);
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    let ind = compute_history(&mgr, &ids, &scope, 0, &bars, keep).await;
    let page = models::HistoryPayload {
        series: SPOT_SERIES,
        bars: bars[keep..].to_vec(),
        has_more_before: true,
        has_more_after: false,
        oldest_time: bars.get(keep).map(|b| b.time),
        newest_time: bars.last().map(|b| b.time),
        realtime_enabled: true,
        indicator_api_version: indicators::API_VERSION,
        indicators: ind,
    };
    let history = serde_json::to_value(&page).unwrap();
    assert_eq!(history["indicator_api_version"], 1);
    assert_eq!(
        history["indicators"]["sample.channel"]["modes"]["source"]["output"]["times"].as_array().unwrap().len(),
        5
    );
    assert_eq!(history["indicators"]["test.fault"]["modes"]["source"]["status"], "unavailable");

    let state = seeded_state(mgr.clone(), 8).await;
    let discovery = serde_json::json!({ "api_version": indicators::API_VERSION, "indicators": mgr.infos() });
    let mut base = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).tx_1m.subscribe();
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    let mut ws = Vec::new();
    ws.push(
        next_msg(
            &mut sub,
            |v| v["type"] == "indicator_status" && v["id"] == "sample.channel" && v["mode"] == "source",
            10,
        )
        .await,
    );
    let forming = price_bar(9);
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, forming.clone()).await;
    let base_frame = serde_json::to_value(base.recv().await.unwrap()).unwrap();
    ws.push(
        next_msg(
            &mut sub,
            |v| v["type"] == "indicator" && v["id"] == "sample.channel" && v["mode"] == "source" && v["seq"] == 1,
            5,
        )
        .await,
    );
    ws.push(serde_json::to_value(live_indicators::IndicatorFrame::reset(42)).unwrap());
    for f in &ws {
        frame(f);
    }
    if let Some(path) = std::env::var_os("CVD_EXPORT_WEB_FIXTURE") {
        let doc = serde_json::json!({
            "_comment": "Generated by cvd-server plugin_tests::export_web_envelope_fixture from real server types, the runner and the public sample/fault libraries. generation/session values are run-specific.",
            "indicators_endpoint": discovery,
            "history": history,
            "ws_base_bar": base_frame,
            "ws_indicator_frames": ws,
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }
}

/// Optional local smoke against externally built public libraries
/// (`CVD_PUBLIC_INDICATOR_DIR=<dir with lib*.so>`). Skipped when unset.
#[tokio::test]
async fn external_public_libraries_load_and_compute_when_provided() {
    let Some(dir) = std::env::var_os("CVD_PUBLIC_INDICATOR_DIR") else { return };
    let mut libs = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "so") {
            let c = cvd_indicator_host::RunnerClient::spawn(cvd_indicator_host::RunnerConfig::new(&artifacts().0, &p))
                .await
                .unwrap_or_else(|e| panic!("{}: {}", p.display(), e.local_detail()));
            libs.push((c.descriptor().id.clone(), p.clone()));
            c.shutdown().await;
        }
    }
    assert!(!libs.is_empty());
    let mut text = format!("[indicators]\nrunner = {:?}\n", artifacts().0.to_str().unwrap());
    for (id, p) in &libs {
        text += &format!("[[indicators.library]]\nid = {id:?}\npath = {:?}\n", p.to_str().unwrap());
    }
    let mgr = IndicatorManager::start(&config::Config::parse(&text).unwrap());
    for (id, _) in &libs {
        wait_status(&mgr, id, Status::Ready, 10).await;
    }
    let ids: Vec<String> = libs.iter().map(|l| l.0.clone()).collect();
    let bars: Vec<Bar> = (1..=600).map(price_bar).collect();
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    let out = compute_history(&mgr, &ids, &scope, 0, &bars, 300).await;
    for id in &ids {
        for (mode, r) in &out[id].modes {
            assert_eq!(r.status, "ok", "{id}/{mode}: {:?}", r.error);
            eprintln!(
                "external {id}/{mode}: {} points, series {:?}",
                r.output.as_ref().unwrap().times.len(),
                r.output.as_ref().unwrap().series.keys().collect::<Vec<_>>()
            );
        }
    }
}

// ---- Fail-closed invalid canonical input (parent decision F) ----

fn invalid_bar(i: i64) -> Bar {
    let mut b = price_bar(i);
    b.price_close = f64::NAN;
    b
}

async fn absent(sub: &mut live_indicators::Subscription, pred: impl Fn(&serde_json::Value) -> bool, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok(msg)) =
        tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), sub.rx.recv()).await
    {
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert!(!pred(&v), "unexpected frame: {v}");
    }
}

#[tokio::test]
async fn history_window_with_invalid_input_fails_closed_and_clean_window_keeps_parity() {
    let mgr = IndicatorManager::start(&config(
        &[
            ("test.fault", "fault_indicator", "{}", "['source', 'aggregate']"),
            ("sample.channel", "sample_indicator", "{ length = 3 }", "['source']"),
        ],
        2000,
    ));
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    let ids = mgr.ids();
    let scope = ScopeBase { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into() };
    // Same production steps as load_history_page: evidence before filtering.
    let mut raw: Vec<Bar> = (1..=40).map(|i| if i == 30 { invalid_bar(i) } else { price_bar(i) }).collect();
    raw[5].delta_usdt = 0.0; // genuine zero stays
    let invalid = invalid_bar_times(&raw);
    assert_eq!(invalid, vec![price_bar(30).time]);
    let mut v = raw.clone();
    v.retain(Bar::is_renderable);
    assert_eq!(v.len(), 39, "only the invalid row is dropped from base data");
    assert!(v.iter().any(|b| b.delta_usdt < 0.0) && v[5].delta_usdt == 0.0);

    // Window (5 + context 2) entirely after the invalid row: bitwise parity.
    let keep = history_keep_start(&v, 5, None);
    let guarded = compute_history_guarded(&mgr, &ids, &scope, 0, &v, keep, &invalid).await;
    let plain = compute_history(&mgr, &ids, &scope, 0, &v, keep).await;
    let f = &guarded["test.fault"];
    for mode in ["source", "aggregate"] {
        assert_eq!(f.modes[mode].status, "ok");
        let (a, b) = (f.modes[mode].output.as_ref().unwrap(), plain["test.fault"].modes[mode].output.as_ref().unwrap());
        assert_eq!(a.times, b.times);
        let bits =
            |o: &cvd_indicator_sdk::Output| values(o, "close").iter().map(|x| x.map(f64::to_bits)).collect::<Vec<_>>();
        assert_eq!(bits(a), bits(b), "clean window is bitwise identical");
    }
    // sample has 500-bar context -> its window reaches the invalid row.
    assert_eq!(guarded["sample.channel"].modes["source"].status, "unavailable");

    // Window covering the invalid row: fixed unavailable, no values, no NaN/zero.
    let keep = history_keep_start(&v, 10, None);
    let out = compute_history_guarded(&mgr, &ids, &scope, 0, &v, keep, &invalid).await;
    for mode in ["source", "aggregate"] {
        let r = &out["test.fault"].modes[mode];
        assert_eq!(r.status, "unavailable");
        assert!(r.output.is_none());
        assert_eq!(r.error.as_ref().unwrap().code, codes::UNAVAILABLE);
        assert_eq!(r.error.as_ref().unwrap().message, "indicator unavailable");
    }
    let text = serde_json::to_string(&out).unwrap();
    assert!(!text.contains("NaN") && !text.contains("inf") && !text.contains("canonical"), "{text}");
    // Window starting at the first loaded row: any invalid row poisons it.
    assert!(indicators::window_poisoned(&invalid, None));
    assert!(!indicators::window_poisoned(&[], None));
}

#[tokio::test]
async fn startup_and_live_invalid_input_poison_until_context_clean_or_validated_reset() {
    let mgr = IndicatorManager::start(&config(&[("test.fault", "fault_indicator", "{}", "['source']")], 2000));
    wait_status(&mgr, "test.fault", Status::Ready, 10).await;
    let state = Arc::new(tests::state_with(mgr).await);
    let sh = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
    // Startup hydration path: invalid row 19 recorded, then filtered.
    let mut loaded: VecDeque<Bar> = (1..=20).map(|i| if i == 19 { invalid_bar(i) } else { price_bar(i) }).collect();
    admit_loaded_history(sh, Tf::M1, &mut loaded);
    assert_eq!(loaded.len(), 19);
    merge_history(&mut *sh.hist_1m.write().await, loaded);
    let mut base = sh.tx_1m.subscribe();
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    let is_status =
        |want: &'static str| move |v: &serde_json::Value| v["type"] == "indicator_status" && v["status"] == want;
    // Context 2: init window {17,18,20} contains 19 -> startup poison.
    let down = next_msg(&mut sub, |v| v["type"] == "indicator_status", 10).await;
    assert_eq!(down["status"], "unavailable", "{down}");
    assert_eq!(down["error"]["code"], codes::UNAVAILABLE);
    let pub_ = |i: i64| push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, price_bar(i));
    pub_(21).await; // window {18,20,21} still covers 19: no recovery on a good tick
    assert_eq!(base.recv().await.unwrap().bar.time, price_bar(21).time, "base data keeps flowing");
    absent(&mut sub, |v| v["type"] == "indicator" || v["status"] == "ok", 3).await;
    pub_(22).await; // window {20,21,22} clean -> validated reopen
    base.recv().await.unwrap();
    next_msg(&mut sub, is_status("ok"), 6).await;

    // Live forming poison, cleared by a validated replacement of the same bucket.
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, invalid_bar(23)).await;
    next_msg(&mut sub, is_status("unavailable"), 6).await;
    assert!(base.try_recv().is_err(), "invalid bar is not transmitted");
    broadcast_live_payload(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, price_bar(23)).await;
    base.recv().await.unwrap();
    next_msg(&mut sub, is_status("ok"), 6).await;
    pub_(23).await;
    base.recv().await.unwrap();

    // Closed invalid bar 24: stays poisoned through unrelated good ticks until
    // the 3-bar context no longer covers it.
    push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, invalid_bar(24)).await;
    next_msg(&mut sub, is_status("unavailable"), 6).await;
    for i in [25, 26] {
        pub_(i).await;
        base.recv().await.unwrap();
    }
    absent(&mut sub, |v| v["type"] == "indicator" || v["status"] == "ok", 3).await;
    pub_(27).await;
    base.recv().await.unwrap();
    next_msg(&mut sub, is_status("ok"), 6).await;
    let m = next_msg(&mut sub, |v| v["type"] == "indicator", 6).await;
    assert_eq!(m["output"]["times"].as_array().unwrap().last().unwrap(), &serde_json::json!(price_bar(27).time));
    assert!(!m.to_string().contains("NaN"));
}

// ---------------------------------------------------------------------------
// A3: per-tf coherent snapshot revisions, base bootstrap and context replay.
// ---------------------------------------------------------------------------

const KEY5: live_indicators::HubKey = (Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M5);

async fn sample_mgr() -> Arc<IndicatorManager> {
    let mgr = IndicatorManager::start(&config(
        &[("sample.channel", "sample_indicator", "{ length = 3 }", "['source']")],
        2000,
    ));
    wait_status(&mgr, "sample.channel", Status::Ready, 10).await;
    mgr
}

fn live(state: &AppState, tf: Tf, bar: Bar) -> impl std::future::Future<Output = ()> + '_ {
    broadcast_live_payload(state, Exchange::Binance, Market::Spot, Symbol::Btc, tf, bar)
}

/// Old LIVE_INIT stamped the shard-global counter, which a 5m frame published
/// after the newest 1m frame had advanced: the 1m initial result claimed a
/// revision that exists only on the 5m stream. Now: init is stamped with the
/// newest 1m rev, the 1m base bootstrap with that rev precedes it, and every
/// later 1m result carries exactly a 1m base rev, including coalesced
/// multi-bar (close + next) batches interleaved with 5m publications.
#[tokio::test]
async fn init_and_updates_are_stamped_with_this_tf_revision_only() {
    let state = seeded_state(sample_mgr().await, 40).await;
    let mut base1 = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).tx_1m.subscribe();
    let mut base5 = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).tx_5m.subscribe();
    let forming = price_bar(41);
    live(&state, Tf::M1, forming.clone()).await;
    let r1 = base1.recv().await.unwrap().rev;
    let mut b5 = price_bar(8);
    b5.time = forming.time - 1;
    live(&state, Tf::M5, b5.clone()).await;
    let r5 = base5.recv().await.unwrap().rev;
    assert!(r5 > r1);
    let global = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).rev.load(Ordering::Acquire);
    assert_eq!(global, r5, "old stamp source would have been the 5m rev");

    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    // Base bootstrap: the newest 1m bar re-sent with its own 1m rev.
    let boot = base1.recv().await.unwrap();
    assert_eq!((boot.rev, boot.bar.time), (r1, forming.time));
    let init = next_msg(&mut sub, |v| v["type"] == "indicator", 10).await;
    assert_eq!(init["as_of_rev"].as_u64(), Some(r1), "init stamped with this tf's rev, not {r5}: {init}");
    assert_eq!(init["output"]["times"].as_array().unwrap().last().unwrap(), &serde_json::json!(forming.time));

    // Interleave: 1m close + next forming bar with 5m frames between them.
    let mut revs1 = std::collections::BTreeSet::from([r1]);
    let mut revs5 = std::collections::BTreeSet::from([r5]);
    let mut last1 = r1;
    for i in 41..46 {
        push_publish(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, price_bar(i)).await;
        revs1.insert(base1.recv().await.unwrap().rev);
        b5.price_close += 1.0;
        live(&state, Tf::M5, b5.clone()).await;
        revs5.insert(base5.recv().await.unwrap().rev);
        live(&state, Tf::M1, price_bar(i + 1)).await;
        last1 = base1.recv().await.unwrap().rev;
        revs1.insert(last1);
    }
    let want_last = price_bar(46).time;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, sub.rx.recv()).await.expect("final update").unwrap();
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        if v["type"] != "indicator" {
            continue;
        }
        let a = v["as_of_rev"].as_u64().unwrap();
        assert!(revs1.contains(&a), "as_of_rev {a} is not a 1m base rev {revs1:?}");
        assert!(!revs5.contains(&a));
        if v["output"]["times"].as_array().unwrap().last() == Some(&want_last.into()) {
            assert_eq!(a, last1, "result of the newest 1m version carries its exact rev");
            // Content check: sma over the last three 1m bars (41..=46 newest).
            let h = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).hist_1m.read().await.clone();
            let n = h.len();
            let expect = (h[n - 2].price_close + h[n - 1].price_close + price_bar(46).price_close) / 3.0;
            let got = v["output"]["series"]["sma"]["values"].as_array().unwrap().last().unwrap().as_f64().unwrap();
            assert!((got - expect).abs() < 1e-9);
            break;
        }
    }
    // A 5m hub never sees a 1m revision.
    let mut sub5 = state.hubs.subscribe(&state, KEY5).unwrap();
    let st = next_msg(&mut sub5, |v| v["type"] == "indicator_status" || v["type"] == "indicator", 10).await;
    if st["type"] == "indicator" {
        assert!(revs5.contains(&st["as_of_rev"].as_u64().unwrap()));
    }
}

/// Snapshot content and revision are read atomically w.r.t. publication. The
/// old order (read counter, then snapshot) is shown to pair newer content with
/// an older revision; the coherent snapshot never does under concurrency.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_content_and_revision_are_coherent_under_concurrent_publication() {
    let state = Arc::new(tests::state_with(IndicatorManager::start(&config(&[], 1000))).await);
    let sh = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc);
    for i in 1..=10 {
        sh.hist_1m.write().await.push_back(price_bar(i));
    }
    let mut b = price_bar(11);
    b.price_close = 1.0;
    live(&state, Tf::M1, b.clone()).await;
    // Old rule, deterministic interleaving: counter read, then a publication,
    // then the snapshot -> content of rev N+1 stamped as rev N.
    let old_rev = sh.rev.load(Ordering::Acquire);
    b.price_close = 2.0;
    live(&state, Tf::M1, b.clone()).await;
    let (bars, rev) = live_indicators::snapshot(&state, KEY, 4).await;
    assert_eq!(bars.last().unwrap().price_close, 2.0);
    assert!(old_rev < rev, "old stamp {old_rev} would understate content of rev {rev}");

    // Concurrent: every snapshot's tail equals the version published at its rev.
    let mut base = sh.tx_1m.subscribe();
    let writer = {
        let state = state.clone();
        tokio::spawn(async move {
            for k in 0..400u32 {
                let mut b = price_bar(11);
                b.price_close = 10.0 + k as f64;
                live(&state, Tf::M1, b).await;
                if k % 7 == 0 {
                    let mut o = price_bar(3);
                    o.price_close = -(k as f64);
                    live(&state, Tf::M5, o).await; // other tf bumps the global counter
                }
                tokio::task::yield_now().await;
            }
        })
    };
    let mut seen = Vec::new();
    let mut by_rev = HashMap::new();
    let drain = |base: &mut tokio::sync::broadcast::Receiver<WsPayload>, by_rev: &mut HashMap<u64, f64>| loop {
        match base.try_recv() {
            Ok(p) => {
                by_rev.insert(p.rev, p.bar.price_close);
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => panic!("drain lagged"),
            Err(_) => break,
        }
    };
    while !writer.is_finished() {
        let (bars, rev) = live_indicators::snapshot(&state, KEY, 4).await;
        seen.push((rev, bars.last().unwrap().price_close));
        drain(&mut base, &mut by_rev);
        tokio::task::yield_now().await;
    }
    writer.await.unwrap();
    drain(&mut base, &mut by_rev);
    let checked = seen.iter().filter(|(r, _)| by_rev.contains_key(r)).count();
    assert!(checked > 10, "race exercised: {checked}");
    for (rev, close) in seen {
        if let Some(want) = by_rev.get(&rev) {
            assert_eq!(*want, close, "snapshot rev {rev} paired with other content");
        } else {
            assert!(rev <= old_rev + 1, "snapshot rev {rev} is not a 1m rev");
        }
    }
}

/// A hub opened before history warm-up must replay the warmed context
/// promptly (bounded reset) instead of waiting for `context` new live bars.
#[tokio::test]
async fn live_hub_opened_before_hydration_replays_warmed_context_promptly() {
    let state = Arc::new(tests::state_with(sample_mgr().await).await);
    let mut base = pick_shard(&state, Exchange::Binance, Market::Spot, Symbol::Btc).tx_1m.subscribe();
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    let ok = next_msg(&mut sub, |v| v["type"] == "indicator_status", 10).await;
    let g0 = ok["generation"].as_u64().unwrap();
    let forming = price_bar(41);
    live(&state, Tf::M1, forming.clone()).await;
    let rf = base.recv().await.unwrap().rev;
    let cold = next_msg(&mut sub, |v| v["type"] == "indicator", 10).await;
    assert_eq!(cold["output"]["series"]["sma"]["values"][0], serde_json::Value::Null, "no context yet: {cold}");

    // Warm-up/repair installs actual context (through the hydrate path helper).
    let loaded: VecDeque<Bar> = (1..=40).map(price_bar).collect();
    let t = Instant::now();
    install_history(&state, Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, loaded).await;
    let reset = next_msg(&mut sub, |v| v["type"] == "indicator_reset", 1).await;
    let g1 = reset["generation"].as_u64().unwrap();
    assert!(g1 > g0);
    assert!(t.elapsed() < Duration::from_secs(1), "prompt, not tick/bar driven");
    let boot = base.recv().await.unwrap();
    assert_eq!((boot.rev, boot.bar.time), (rf, forming.time), "base bootstrap after resync");
    let warm = next_msg(&mut sub, |v| v["type"] == "indicator" && v["generation"].as_u64() == Some(g1), 5).await;
    assert_eq!(warm["as_of_rev"].as_u64(), Some(rf));
    let expect = (price_bar(39).price_close + price_bar(40).price_close + forming.price_close) / 3.0;
    let got = warm["output"]["series"]["sma"]["values"].as_array().unwrap().last().unwrap().as_f64().unwrap();
    assert!((got - expect).abs() < 1e-9, "replayed with warmed context: {warm}");
    assert!(base.try_recv().is_err(), "no per-tick history transfer");
}

/// Old `subscribe` spawned the producer with subscribers=0 and the channel's
/// initial receiver discarded, registering the viewer only afterwards: on a
/// multi-thread runtime the hub could emit bootstrap/status into no receiver
/// or start stopping. Deterministic part (current-thread: the spawned hub
/// cannot run before `subscribe` returns): the viewer is counted and owns a
/// receiver at spawn time, so the first status is never lost.
#[tokio::test]
async fn first_subscriber_is_registered_before_hub_producer_runs() {
    let state = seeded_state(sample_mgr().await, 40).await;
    let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
    assert_eq!(sub.hub.counts(), (1, 1), "counted + receiving before the producer is polled");
    let ok = next_msg(&mut sub, |v| v["type"] == "indicator_status", 10).await;
    assert_eq!(ok["status"], "ok");
}

/// Stress counterpart on worker threads: every fresh hub's first status frame
/// reaches its first (only) viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_status_reaches_first_subscriber_on_worker_threads() {
    let state = seeded_state(sample_mgr().await, 40).await;
    for i in 0..30 {
        let mut sub = state.hubs.subscribe(&state, KEY).unwrap();
        let ok = next_msg(&mut sub, |v| v["type"] == "indicator_status", 10).await;
        assert_eq!(ok["status"], "ok", "round {i}");
        let hub = sub.hub.clone();
        drop(sub);
        let t = Instant::now();
        while state.hubs.active() > 0 && t.elapsed() < Duration::from_secs(8) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.hubs.active(), 0, "round {i}: hub retired");
        assert_eq!(hub.counts().0, 0);
    }
}

/// Old stop path decided `count==0` outside the map lock and later skipped
/// unmapping if a joiner had incremented meanwhile -> a mapped dead hub. The
/// stop decision is now atomic with `subscribe` (same lock): a joiner counted
/// first keeps the hub alive; after retirement a joiner gets a fresh hub.
/// Current-thread runtime, no awaits: the hub task never runs, so the
/// interleavings below are exact.
#[tokio::test]
async fn retire_decision_is_atomic_with_subscribe() {
    let state = seeded_state(sample_mgr().await, 40).await;
    let a = state.hubs.subscribe(&state, KEY).unwrap();
    let hub = a.hub.clone();
    drop(a); // producer could now observe count 0 ...
    let b = state.hubs.subscribe(&state, KEY).unwrap(); // ... but a joiner arrives first
    assert!(Arc::ptr_eq(&b.hub, &hub), "joiner joins the still-mapped hub");
    assert!(!state.hubs.try_retire(KEY, &hub), "hub with a joined viewer must not stop");
    assert_eq!(state.hubs.active(), 1);
    drop(b);
    assert!(state.hubs.try_retire(KEY, &hub), "no viewers -> stop");
    assert_eq!(state.hubs.active(), 0, "retired hub unmapped atomically");
    let mut c = state.hubs.subscribe(&state, KEY).unwrap();
    assert!(!Arc::ptr_eq(&c.hub, &hub), "joiner after retirement gets a fresh live hub");
    let ok = next_msg(&mut c, |v| v["type"] == "indicator_status", 10).await;
    assert_eq!(ok["status"], "ok");
}
