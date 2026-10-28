//! Real runner process + real cdylib tests (no network, no database).

use cvd_indicator_host::{HostError, RunnerClient, RunnerConfig};
use cvd_indicator_sdk::*;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn artifacts() -> &'static (PathBuf, PathBuf) {
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
                "raw-abi-indicator",
            ])
            .status()
            .expect("cargo build");
        assert!(status.success(), "building runner and fixture libraries failed");
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
        (target.join("debug/cvd-indicator-runner"), target.join("debug"))
    })
}

fn lib(name: &str) -> PathBuf {
    artifacts().1.join(format!("lib{name}.so"))
}

fn cfg(name: &str) -> RunnerConfig {
    let mut c = RunnerConfig::new(&artifacts().0, lib(name));
    c.history_timeout = Duration::from_secs(5);
    c.live_timeout = Duration::from_millis(1500);
    c
}

fn bar(i: i64) -> InputBar {
    let p = 100.0 + (i as f64 * 0.7).sin() * 5.0 + i as f64 * 0.01;
    InputBar {
        time: 1_700_000_000 + i * 60,
        po: p - 0.5,
        ph: p + 1.0,
        pl: p - 1.0,
        price_close: p,
        apo: p - 0.4,
        aph: p + 2.0,
        apl: p - 2.0,
        apc: p + 0.1,
        cvd_usdt: i as f64 * 10.0,
        ac: i as f64 * 20.0,
        delta_usdt: (i % 7) as f64 - 3.0,
        ad: ((i % 5) as f64 - 2.0) * 2.0,
        bvb: 1.0,
        abvb: 2.0,
        svb: 1.0,
        asvb: 2.0,
        buy_volume_usdt: p,
        agb: 2.0 * p,
        sell_volume_usdt: p,
        ags: 2.0 * p,
        trades: 5,
    }
}

fn scope(mode: Mode) -> Scope {
    Scope { exchange: "binance".into(), market: "spot".into(), symbol: "BTC".into(), tf: "1m".into(), mode }
}

fn line(o: &Output, k: &str) -> Vec<Option<f64>> {
    match &o.series[k] {
        SeriesValues::Line { values } | SeriesValues::Histogram { values } => values.clone(),
        _ => panic!("not a line"),
    }
}

fn plugin_code(e: &HostError) -> String {
    match e {
        HostError::Plugin(p) => p.code.clone(),
        other => panic!("expected plugin error, got {other:?}"),
    }
}

fn pid_alive(pid: u32) -> bool {
    // Reaped children disappear from /proc.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).map(|s| s.contains(") Z ")).unwrap_or(true)
}

#[tokio::test]
async fn sample_history_matches_direct_computation_and_respects_warmup() {
    let mut c = RunnerClient::spawn(cfg("sample_indicator")).await.expect("spawn");
    assert_eq!(c.descriptor().id, "sample.channel");
    assert_eq!(c.descriptor().outputs.len(), 5);
    let bars: Vec<InputBar> = (1..=600).map(bar).collect();
    let req = HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 100, bars: bars.clone() };
    let out = c.history(br#"{"length":20}"#, &req, 1).await.expect("history");
    assert_eq!(out.times.len(), 500);
    assert_eq!(out.times[0], bars[100].time);
    let sma = line(&out, "sma");
    let direct = bars[81..=100].iter().map(|b| b.price_close).sum::<f64>() / 20.0;
    assert!((sma[0].unwrap() - direct).abs() < 1e-9);
    // Projection of established per-bar delta, not recomputed.
    assert_eq!(line(&out, "delta")[5], Some(bars[105].delta_usdt));
    let total = bars[105].buy_volume_usdt + bars[105].sell_volume_usdt;
    assert_eq!(line(&out, "total")[5], Some(total), "readout-only quote total");
    let ex = c.descriptor().outputs.iter().find(|o| o.key == "extremes").unwrap();
    assert_eq!(ex.widths, vec![2, 1]);
    assert!(c.descriptor().outputs.iter().find(|o| o.key == "total").unwrap().readout_only);

    // Aggregate mode reads aggregate fields.
    let req = HistoryRequest { scope: scope(Mode::Aggregate), ..req };
    let out = c.history(b"", &req, 1).await.expect("aggregate history");
    assert_eq!(line(&out, "delta")[5], Some(bars[105].ad));
    c.shutdown().await;
}

#[tokio::test]
async fn sample_live_append_replace_without_double_counting() {
    let mut c = RunnerClient::spawn(cfg("sample_indicator")).await.unwrap();
    let bars: Vec<InputBar> = (1..=50).map(bar).collect();
    let init = LiveInit { scope: scope(Mode::Source), revision: 3, bars: bars.clone() };
    let o = c.live_open(7, 1, br#"{"length":3}"#, &init).await.unwrap();
    assert_eq!(o.times, vec![bars[49].time]);

    // Replace the forming bar twice: result must reflect only the last value.
    let mut forming = bar(50);
    forming.price_close = 1000.0;
    forming.delta_usdt = 4.0;
    let upd = LiveUpdate { revision: 3, seq: 1, ops: vec![LiveOp::Replace { bar: forming.clone() }] };
    let o = c.live_update(7, 1, &upd).await.unwrap();
    assert_eq!(o.times, vec![forming.time]);
    assert_eq!(line(&o, "delta"), vec![Some(4.0)]);
    let expect = (bars[47].price_close + bars[48].price_close + 1000.0) / 3.0;
    assert!((line(&o, "sma")[0].unwrap() - expect).abs() < 1e-9);
    forming.delta_usdt = 6.0;
    let o = c
        .live_update(7, 1, &LiveUpdate { revision: 3, seq: 2, ops: vec![LiveOp::Replace { bar: forming.clone() }] })
        .await
        .unwrap();
    assert_eq!(line(&o, "delta"), vec![Some(6.0)]);
    assert!((line(&o, "sma")[0].unwrap() - expect).abs() < 1e-9, "replace must not accumulate");

    // Append a new bar.
    let next = bar(51);
    let o = c
        .live_update(7, 1, &LiveUpdate { revision: 3, seq: 3, ops: vec![LiveOp::Append { bar: next.clone() }] })
        .await
        .unwrap();
    assert_eq!(o.times, vec![next.time]);

    // Sequence gap and stale generation are rejected; session survives.
    let gap = LiveUpdate { revision: 3, seq: 9, ops: vec![LiveOp::Replace { bar: next.clone() }] };
    assert_eq!(plugin_code(&c.live_update(7, 1, &gap).await.unwrap_err()), codes::SEQUENCE_GAP);
    let ok = LiveUpdate { revision: 3, seq: 4, ops: vec![LiveOp::Replace { bar: next.clone() }] };
    assert_eq!(plugin_code(&c.live_update(7, 2, &ok).await.unwrap_err()), codes::STALE_GENERATION);
    let stale_rev = LiveUpdate { revision: 2, seq: 4, ops: vec![] };
    assert_eq!(plugin_code(&c.live_update(7, 1, &stale_rev).await.unwrap_err()), codes::STALE_GENERATION);
    c.live_update(7, 1, &ok).await.expect("session still usable");

    c.live_close(7).await.unwrap();
    assert_eq!(plugin_code(&c.live_update(7, 1, &ok).await.unwrap_err()), codes::UNKNOWN_SESSION);
    c.shutdown().await;
}

#[tokio::test]
async fn params_are_validated_against_descriptor() {
    let mut c = RunnerClient::spawn(cfg("sample_indicator")).await.unwrap();
    let req =
        HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 0, bars: (1..=5).map(bar).collect() };
    for p in [&br#"{"length":0}"#[..], br#"{"nope":1}"#, br#"[1]"#, br#"{"length":"x"}"#] {
        assert_eq!(plugin_code(&c.history(p, &req, 1).await.unwrap_err()), codes::INVALID_PARAMS);
    }
    // Non-monotonic input is rejected.
    let mut bad = req.clone();
    bad.bars.swap(1, 2);
    assert_eq!(plugin_code(&c.history(b"", &bad, 1).await.unwrap_err()), codes::INVALID_INPUT);
    c.history(b"", &req, 1).await.expect("runner still healthy");
}

async fn fault(f: &str) -> (RunnerClient, Result<Output, HostError>) {
    let mut c = RunnerClient::spawn(cfg("fault_indicator")).await.expect("spawn fault fixture");
    let req =
        HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 0, bars: (1..=10).map(bar).collect() };
    let params = format!(r#"{{"fault":"{f}"}}"#);
    let r = c.history(params.as_bytes(), &req, 1).await;
    (c, r)
}

#[tokio::test]
async fn plugin_error_and_panic_keep_runner_alive() {
    for (f, code) in [("error", codes::COMPUTE_FAILED), ("panic", codes::PANIC), ("bad_output", codes::INVALID_OUTPUT)]
    {
        let (mut c, r) = fault(f).await;
        let e = r.unwrap_err();
        assert_eq!(plugin_code(&e), code, "{f}");
        assert!(!e.public().message.contains("fixture panic"), "panic payload must not leak");
        let req = HistoryRequest {
            scope: scope(Mode::Source),
            revision: 1,
            output_from: 0,
            bars: (1..=3).map(bar).collect(),
        };
        c.history(b"", &req, 1).await.unwrap_or_else(|e| panic!("runner died after {f}: {e:?}"));
        c.shutdown().await;
    }
    let mut c = RunnerClient::spawn(cfg("fault_indicator")).await.unwrap();
    let req =
        HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 0, bars: (1..=3).map(bar).collect() };
    assert_eq!(plugin_code(&c.history(br#"{"create_fail":true}"#, &req, 1).await.unwrap_err()), codes::INVALID_PARAMS);
}

#[tokio::test]
async fn stdout_noise_from_library_cannot_corrupt_protocol() {
    let (mut c, r) = fault("stdout_noise").await;
    assert_eq!(r.expect("noise is redirected to stderr").times.len(), 10);
    // stderr is drained asynchronously; allow the reader task to catch up.
    let t = Instant::now();
    while !c.stderr_tail().contains("garbage on stdout") && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(c.stderr_tail().contains("garbage on stdout"));
    let req =
        HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 0, bars: (1..=3).map(bar).collect() };
    c.history(b"", &req, 1).await.unwrap();
}

#[tokio::test]
async fn abort_and_segfault_are_contained_and_host_survives() {
    for f in ["abort", "segfault"] {
        let (c, r) = fault(f).await;
        let e = r.unwrap_err();
        assert!(matches!(e, HostError::Crashed { .. }), "{f}: {e:?}");
        assert_eq!(e.public().code, codes::CRASHED);
        assert!(!e.public().message.contains('/'), "no local paths in public error");
        assert!(c.is_dead());
        // A fresh runner works: the host process (this test) is unaffected.
        let (_c2, r2) = fault("none").await;
        r2.expect("respawned runner works");
    }
}

#[tokio::test]
async fn hang_is_killed_within_deadline() {
    let mut cfgh = cfg("fault_indicator");
    cfgh.history_timeout = Duration::from_millis(400);
    let mut c = RunnerClient::spawn(cfgh).await.unwrap();
    let pid = c.pid().unwrap();
    let req =
        HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 0, bars: (1..=3).map(bar).collect() };
    let t = Instant::now();
    let e = c.history(br#"{"fault":"hang"}"#, &req, 1).await.unwrap_err();
    assert!(matches!(e, HostError::Timeout), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(4), "bounded kill: {:?}", t.elapsed());
    assert!(!pid_alive(pid), "hung runner must be killed and reaped");
    assert!(c.is_dead());
}

#[tokio::test]
async fn live_abort_on_nth_update_is_contained() {
    let mut c = RunnerClient::spawn(cfg("fault_indicator")).await.unwrap();
    let init = LiveInit { scope: scope(Mode::Source), revision: 1, bars: (1..=5).map(bar).collect() };
    // compute call #1 = init, #2 = first update, #3 aborts.
    c.live_open(1, 1, br#"{"fault":"abort","trigger":3}"#, &init).await.unwrap();
    let u1 = LiveUpdate { revision: 1, seq: 1, ops: vec![LiveOp::Append { bar: bar(6) }] };
    c.live_update(1, 1, &u1).await.unwrap();
    let u2 = LiveUpdate { revision: 1, seq: 2, ops: vec![LiveOp::Append { bar: bar(7) }] };
    assert!(matches!(c.live_update(1, 1, &u2).await.unwrap_err(), HostError::Crashed { .. }));
}

#[tokio::test]
async fn live_panic_destroys_session() {
    let mut c = RunnerClient::spawn(cfg("fault_indicator")).await.unwrap();
    let init = LiveInit { scope: scope(Mode::Source), revision: 1, bars: (1..=5).map(bar).collect() };
    c.live_open(1, 1, br#"{"fault":"panic","trigger":2}"#, &init).await.unwrap();
    let u1 = LiveUpdate { revision: 1, seq: 1, ops: vec![LiveOp::Append { bar: bar(6) }] };
    assert_eq!(plugin_code(&c.live_update(1, 1, &u1).await.unwrap_err()), codes::PANIC);
    assert_eq!(plugin_code(&c.live_update(1, 1, &u1).await.unwrap_err()), codes::UNKNOWN_SESSION);
}

#[tokio::test]
async fn incompatible_libraries_are_rejected_at_handshake() {
    let e = RunnerClient::spawn(cfg("bad_abi_indicator")).await.err().expect("bad abi rejected");
    assert!(matches!(e, HostError::Spawn { .. }));
    assert_eq!(e.public().code, codes::INCOMPATIBLE);
    assert!(e.local_detail().contains("abi 99"), "local detail keeps diagnostics: {}", e.local_detail());
    assert!(!e.public().message.contains("abi 99"));

    let mut missing = cfg("sample_indicator");
    missing.library_path = PathBuf::from("/nonexistent/libnothing.so");
    let e = RunnerClient::spawn(missing).await.err().unwrap();
    assert_eq!(e.public().code, codes::INCOMPATIBLE);
    assert!(!e.public().message.contains("nonexistent"));

    // A real shared object without the entry symbol: the runner's own SDK rlib is
    // not a .so, so use the bad-abi build dir's sibling C runtime instead.
    for candidate in
        ["/lib/x86_64-linux-gnu/libm.so.6", "/usr/lib/x86_64-linux-gnu/libm.so.6", "/lib/aarch64-linux-gnu/libm.so.6"]
    {
        if std::path::Path::new(candidate).exists() {
            let mut c = cfg("sample_indicator");
            c.library_path = candidate.into();
            let e = RunnerClient::spawn(c).await.err().unwrap();
            assert!(e.local_detail().contains("entry symbol"), "{}", e.local_detail());
            break;
        }
    }

    let mut bad_runner = cfg("sample_indicator");
    bad_runner.runner_path = PathBuf::from("/nonexistent/runner");
    let e = RunnerClient::spawn(bad_runner).await.err().unwrap();
    assert_eq!(e.public().code, codes::UNAVAILABLE);
}

#[tokio::test]
async fn request_limits_are_enforced_before_sending() {
    let mut c = cfg("sample_indicator");
    c.limits.max_bars_per_request = 10;
    let mut c = RunnerClient::spawn(c).await.unwrap();
    let req =
        HistoryRequest { scope: scope(Mode::Source), revision: 1, output_from: 0, bars: (1..=11).map(bar).collect() };
    assert_eq!(plugin_code(&c.history(b"", &req, 1).await.unwrap_err()), codes::LIMIT_EXCEEDED);
}

#[test]
fn canonical_bar_json_size_supports_default_limits() {
    let b = serde_json::to_vec(&bar(123456)).unwrap();
    let limits = protocol::Limits::default();
    let worst = (1600 + limits.max_context_bars as usize) * b.len();
    assert!(worst * 2 <= limits.max_request_bytes + limits.max_request_bytes / 2, "bar={}B worst={worst}", b.len());
}
