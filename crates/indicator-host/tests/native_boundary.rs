//! R2 native-boundary hardening: process-group cleanup, protocol-fd CLOEXEC
//! and bounded/sanitised ABI error buffers. Real runner + real cdylib.
//! Only process groups owned by runners spawned in this test are signalled.

use cvd_indicator_host::{HostError, RunnerClient, RunnerConfig};
use cvd_indicator_sdk::*;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn artifacts() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        // Become a subreaper so orphaned fixture descendants are reparented to
        // this test process and can be observed as reaped (not just zombies).
        #[cfg(target_os = "linux")]
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let st = std::process::Command::new(env!("CARGO"))
            .current_dir(&root)
            .args(["build", "--offline", "-p", "cvd-indicator-runner", "-p", "raw-abi-indicator"])
            .status()
            .unwrap();
        assert!(st.success());
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
        (target.join("debug/cvd-indicator-runner"), target.join("debug/libraw_abi_indicator.so"))
    })
}

fn cfg() -> RunnerConfig {
    let (r, l) = artifacts();
    let mut c = RunnerConfig::new(r, l);
    c.history_timeout = Duration::from_millis(1500);
    c
}

fn bar(i: i64) -> InputBar {
    let p = 100.0 + i as f64;
    InputBar {
        time: 1_700_000_000 + i * 60,
        po: p,
        ph: p,
        pl: p,
        price_close: p,
        apo: p,
        aph: p,
        apl: p,
        apc: p,
        cvd_usdt: 0.0,
        ac: 0.0,
        delta_usdt: 0.0,
        ad: 0.0,
        bvb: 0.0,
        abvb: 0.0,
        svb: 0.0,
        asvb: 0.0,
        buy_volume_usdt: 0.0,
        agb: 0.0,
        sell_volume_usdt: 0.0,
        ags: 0.0,
        trades: 1,
    }
}

fn req() -> HistoryRequest {
    HistoryRequest {
        scope: Scope {
            exchange: "x".into(),
            market: "spot".into(),
            symbol: "BTC".into(),
            tf: "1m".into(),
            mode: Mode::Source,
        },
        revision: 1,
        output_from: 0,
        bars: (1..=3).map(bar).collect(),
    }
}

async fn run(mode: &str) -> (RunnerClient, Result<Output, HostError>) {
    let mut c = RunnerClient::spawn(cfg()).await.expect("spawn raw fixture");
    let r = c.history(format!(r#"{{"mode":"{mode}"}}"#).as_bytes(), &req(), 1).await;
    (c, r)
}

fn assert_public_clean(e: &PluginError) {
    assert!(e.message.len() <= 256, "public message bounded: {}", e.message.len());
    for bad in ["/opt", "secret", "INTERNAL", "hunter2", "not json", "pretend", "window must"] {
        assert!(!e.message.contains(bad), "public message leaks {bad:?}: {}", e.message);
        assert!(!e.code.contains(bad), "public code leaks {bad:?}: {}", e.code);
    }
    assert!(codes::is_known(&e.code), "unknown public code {}", e.code);
}

// ------------------------------------------------------------ error buffers

#[tokio::test]
async fn hostile_error_buffers_are_bounded_and_sanitised() {
    for (mode, want) in [
        ("create_big_err", codes::COMPUTE_FAILED),
        ("invoke_big_err", codes::COMPUTE_FAILED),
        ("create_malformed_err", codes::COMPUTE_FAILED),
        ("invoke_malformed_err", codes::COMPUTE_FAILED),
        ("unknown_code", codes::COMPUTE_FAILED),
        ("reserved_code", codes::COMPUTE_FAILED),
        ("unicode_err", codes::INVALID_INPUT),
        ("documented_err", codes::INVALID_PARAMS),
    ] {
        let (mut c, r) = run(mode).await;
        let e = r.unwrap_err();
        assert!(matches!(e, HostError::Plugin(_)), "{mode}: non-fatal plugin error, got {e:?}");
        let p = e.public();
        assert_eq!(p.code, want, "{mode}");
        assert_public_clean(&p);
        // Local detail is bounded too (operator diagnostics, not a dump).
        assert!(e.local_detail().len() <= 1024, "{mode}: local detail bounded");
        // Library buffers were freed: the fixture aborts on any outstanding buffer.
        c.history(br#"{"mode":"ok"}"#, &req(), 1).await.unwrap_or_else(|e| panic!("{mode}: runner unusable: {e:?}"));
        c.shutdown().await;
    }
}

#[tokio::test]
async fn oversized_error_buffer_is_rejected_before_decode() {
    // Even when the frame limit is generous, the ABI error cap applies.
    let mut cf = cfg();
    cf.limits.max_response_bytes = 8 * 1024 * 1024;
    let mut c = RunnerClient::spawn(cf).await.unwrap();
    let e = c.history(br#"{"mode":"invoke_big_err"}"#, &req(), 1).await.unwrap_err();
    assert_eq!(e.public().code, codes::COMPUTE_FAILED);
    let t = Instant::now();
    while !c.stderr_tail().contains("oversized") && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let tail = c.stderr_tail();
    assert!(tail.contains("oversized"), "operator diagnostic recorded: {tail}");
    assert!(!tail.contains(&"x".repeat(5000)), "diagnostic must not dump the buffer");
}

// ------------------------------------------------------------ process group

fn pidfile(tag: &str) -> PathBuf {
    let dir = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("cvd-r2-{}-{tag}.pid", std::process::id()))
}

async fn read_pid(p: &PathBuf) -> i32 {
    let t = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(v) = s.trim().parse() {
                let _ = std::fs::remove_file(p);
                return v;
            }
        }
        assert!(t.elapsed() < Duration::from_secs(5), "fixture did not write pidfile");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether `pid` still exists and is not a zombie; reaps it if we are its parent.
fn running(pid: i32) -> bool {
    unsafe {
        let mut st = 0;
        if libc::waitpid(pid, &mut st, libc::WNOHANG) == pid {
            return false;
        }
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(s) => !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
    }
}

async fn gone_within(pid: i32, d: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < d {
        if !running(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn extra_fds(pid: i32) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()).is_some_and(|n| n > 2))
                .map(|e| format!("{:?}->{:?}", e.file_name(), std::fs::read_link(e.path()).ok()))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn pgid(pid: i32) -> i32 {
    unsafe { libc::getpgid(pid) }
}

/// Safety net: kill only the group we own (the runner's own group).
fn owned_cleanup(group: i32, sleeper: i32) {
    // Never signal the test harness's own group (pre-fix runners shared it).
    if group > 1 && group != unsafe { libc::getpgrp() } && pgid(sleeper) == group {
        unsafe { libc::killpg(group, libc::SIGKILL) };
    } else if running(sleeper) {
        unsafe { libc::kill(sleeper, libc::SIGKILL) };
        let _ = running(sleeper);
    }
}

async fn spawn_sleeper(tag: &str, hang: bool) -> (RunnerClient, i32, i32, Result<Output, HostError>) {
    let mut cf = cfg();
    cf.history_timeout = Duration::from_millis(600);
    let mut c = RunnerClient::spawn(cf).await.unwrap();
    let runner = c.pid().unwrap() as i32;
    let pf = pidfile(tag);
    let mode = if hang { "sleeper_hang" } else { "sleeper" };
    let params = format!(r#"{{"mode":"{mode}","pidfile":"{}"}}"#, pf.display());
    let r = c.history(params.as_bytes(), &req(), 1).await;
    let s = read_pid(&pf).await;
    (c, runner, s, r)
}

#[tokio::test]
async fn protocol_fd_is_not_inherited_across_exec() {
    let (c, runner, s, r) = spawn_sleeper("fd", false).await;
    r.expect("sleeper mode computes normally");
    // Give exec a moment to replace the image.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let fds = extra_fds(s);
    let group = pgid(runner);
    assert_eq!(pgid(s), group, "descendant shares the runner's group");
    assert_ne!(group, unsafe { libc::getpgrp() }, "runner must not share the host's process group");
    drop(c);
    let ok = gone_within(s, Duration::from_secs(3)).await;
    owned_cleanup(group, s);
    assert!(fds.is_empty(), "descendant inherited non-stdio fds (protocol fd leak): {fds:?}");
    assert!(ok, "descendant survived drop");
}

#[tokio::test]
async fn timeout_kills_term_ignoring_descendant_group() {
    let (c, runner, s, r) = spawn_sleeper("timeout", true).await;
    let group = pgid(s);
    assert!(matches!(r, Err(HostError::Timeout)), "{r:?}");
    assert!(c.is_dead());
    let ok = gone_within(s, Duration::from_secs(3)).await;
    owned_cleanup(group, s);
    assert!(!running(runner), "runner reaped");
    assert!(ok, "TERM-ignoring descendant holding stdio survived timeout kill");
}

#[tokio::test]
async fn explicit_kill_and_shutdown_clean_the_group() {
    for how in ["kill", "shutdown"] {
        let (mut c, _runner, s, r) = spawn_sleeper(how, false).await;
        r.unwrap();
        let group = pgid(s);
        assert!(running(s));
        let t = Instant::now();
        if how == "kill" {
            c.kill().await;
        } else {
            c.shutdown().await;
        }
        assert!(t.elapsed() < Duration::from_secs(4), "{how}: bounded");
        let ok = gone_within(s, Duration::from_secs(3)).await;
        owned_cleanup(group, s);
        assert!(ok, "{how}: descendant survived");
    }
}

#[tokio::test]
async fn cancelled_kill_still_cleans_group_on_drop() {
    let (mut c, _runner, s, r) = spawn_sleeper("cancel", false).await;
    r.unwrap();
    let group = pgid(s);
    // Cancel the cleanup future immediately, then drop the client.
    let _ = tokio::time::timeout(Duration::from_nanos(1), c.kill()).await;
    drop(c);
    let ok = gone_within(s, Duration::from_secs(3)).await;
    owned_cleanup(group, s);
    assert!(ok, "descendant survived cancelled kill + drop");
}

// ------------------------------------------------- R2 follow-up: handshake

fn fake_runner() -> PathBuf {
    artifacts().0.parent().unwrap().join("fake_runner")
}

#[tokio::test]
async fn handshake_errors_are_bounded_and_sanitised() {
    for mode in ["secret", "unknown_code", "malformed", "huge", "unicode", "plain"] {
        let mut c = RunnerConfig::new(fake_runner(), mode);
        c.startup_timeout = Duration::from_secs(3);
        let e = match RunnerClient::spawn(c).await {
            Ok(_) => panic!("{mode}: fake handshake must fail"),
            Err(e) => e,
        };
        let p = e.public();
        assert_public_clean(&p);
        assert!(!p.message.contains("plain"), "{mode}: runner text crossed boundary: {}", p.message);
        assert!(e.local_detail().len() <= 4096 + 16 * 1024 + 128, "{mode}: detail bounded");
    }
}

#[tokio::test]
async fn bad_library_path_gives_fixed_public_error() {
    let (r, _) = artifacts();
    let c = RunnerConfig::new(r, "/opt/secret/hunter2/missing.so");
    let e = match RunnerClient::spawn(c).await {
        Ok(_) => panic!("missing library must fail"),
        Err(e) => e,
    };
    let p = e.public();
    assert_public_clean(&p);
    assert_eq!(p.code, codes::INCOMPATIBLE);
}

#[tokio::test]
async fn shutdown_is_bounded_after_cancelled_requests_fill_the_pipe() {
    let (c0, runner, s, r) = {
        // Hang the runner inside compute (it stops reading stdin).
        let mut cf = cfg();
        cf.history_timeout = Duration::from_secs(60);
        let mut c = RunnerClient::spawn(cf).await.unwrap();
        let runner = c.pid().unwrap() as i32;
        let pf = pidfile("fullpipe");
        let params = format!(r#"{{"mode":"sleeper_hang","pidfile":"{}"}}"#, pf.display());
        let r = tokio::time::timeout(Duration::from_millis(300), c.history(params.as_bytes(), &req(), 1)).await;
        let s = read_pid(&pf).await;
        (c, runner, s, r)
    };
    assert!(r.is_err(), "first request cancelled by caller");
    let mut c = c0;
    let group = pgid(runner);
    // Second request: large params, cancelled while write_all blocks on a full pipe.
    let pad = format!(r#"{{"mode":"ok","pad":"{}"}}"#, "x".repeat(1 << 20));
    let r2 = tokio::time::timeout(Duration::from_millis(300), c.history(pad.as_bytes(), &req(), 2)).await;
    assert!(r2.is_err(), "second request blocked on full pipe and was cancelled");
    assert!(!c.is_dead());
    let t = Instant::now();
    let done = tokio::time::timeout(Duration::from_secs(6), c.shutdown()).await;
    let el = t.elapsed();
    let ok = gone_within(s, Duration::from_secs(3)).await;
    let runner_gone = !running(runner);
    owned_cleanup(group, s);
    assert!(done.is_ok(), "shutdown hung on full pipe");
    assert!(el < Duration::from_secs(4), "shutdown bounded: {el:?}");
    assert!(runner_gone, "runner reaped");
    assert!(ok, "descendant survived shutdown");
}

/// Owned, deliberately NON-CLOEXEC high descriptor in the host process,
/// emulating inherited CI/jobserver pipes. Closed on drop.
struct InheritableFd(i32);
impl InheritableFd {
    fn new() -> Self {
        let src = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(src >= 0);
        let fd = unsafe { libc::fcntl(src, libc::F_DUPFD, 300) }; // F_DUPFD: no CLOEXEC
        unsafe { libc::close(src) };
        assert!(fd >= 300);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        InheritableFd(fd)
    }
}
impl Drop for InheritableFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

#[tokio::test]
async fn inherited_parent_fd_does_not_reach_runner_or_descendant() {
    let inj = InheritableFd::new();
    let (c, runner, s, r) = spawn_sleeper("inh", false).await;
    let runner_fds = extra_fds(runner);
    r.expect("sleeper mode computes normally");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let fds = extra_fds(s);
    let group = pgid(runner);
    drop(c);
    let ok = gone_within(s, Duration::from_secs(3)).await;
    owned_cleanup(group, s);
    drop(inj);
    assert!(fds.is_empty(), "descendant inherited non-stdio fds: {fds:?}");
    assert!(
        !runner_fds.iter().any(|f| f.starts_with("\"300\"") || f.contains("/dev/null")),
        "runner inherited parent fd: {runner_fds:?}"
    );
    assert!(ok, "descendant survived drop");
}
