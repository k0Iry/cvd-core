//! Indicator runner supervision and generic history computation.
//!
//! One supervisor task per operator-configured library owns a
//! [`RunnerClient`]. Callers talk to it through a bounded queue (backpressure:
//! a full queue yields `unavailable` immediately) and always wait with a
//! deadline, so ingestion and HTTP handlers never block on a hung plugin.
//! Crashed / hung / incompatible runners are restarted with backoff; raw
//! diagnostics are logged locally and only public error codes leave the host.
//!
//! The host is generic: it never inspects an indicator id to decide numerics or
//! field mappings. Everything comes from the library descriptor.

use crate::config::{Config, LibraryConfig};
use crate::models::Bar;
use cvd_indicator_host::{RunnerClient, RunnerConfig};
use cvd_indicator_sdk::protocol::Limits;
use cvd_indicator_sdk::{
    codes, Descriptor, HistoryRequest, InputBar, LiveInit, LiveUpdate, Mode, Output, PluginError, Scope,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Ready,
    /// Failed, incompatible, restarting after a crash, or disabled by the
    /// operator; `error` carries the public reason.
    Unavailable,
}

/// Version of the host's indicator HTTP/WS envelope.
pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone)]
struct EntryState {
    status: Status,
    descriptor: Option<Descriptor>,
    error: Option<PluginError>,
    restarts: u64,
    /// Incremented on every successful runner start; live sessions are bound
    /// to the epoch they were opened in.
    epoch: u64,
}

type Reply = oneshot::Sender<Result<Output, PluginError>>;

enum Job {
    History { body: Vec<u8>, expected: Vec<i64>, reply: Reply },
    LiveOpen { session: u64, generation: u64, init: LiveInit, reply: Reply },
    LiveUpdate { session: u64, generation: u64, upd: LiveUpdate, reply: Reply },
    LiveClose { session: u64 },
}

pub struct Entry {
    pub cfg: LibraryConfig,
    params_json: Vec<u8>,
    state: RwLock<EntryState>,
    tx: Option<mpsc::Sender<Job>>,
}

/// Public description of an installed indicator (safe for `/indicators`).
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorInfo {
    pub id: String,
    pub status: Status,
    pub modes: Vec<String>,
    pub version: Option<String>,
    pub descriptor: Option<Descriptor>,
    pub error: Option<PluginError>,
    pub restarts: u64,
}

pub struct IndicatorManager {
    entries: Vec<Arc<Entry>>,
    history_timeout: Duration,
    live_timeout: Duration,
}

/// Public `unavailable` error. `_why` documents the local reason only; the
/// public message is fixed.
pub fn unavailable(_why: &str) -> PluginError {
    PluginError::new(codes::UNAVAILABLE, "indicator unavailable")
}

/// Replace any library/runner-provided text with a fixed public message for a
/// known code. Library messages may contain private details; they are logged
/// locally (bounded) and never forwarded to clients.
pub fn sanitize(id: &str, e: PluginError) -> PluginError {
    const KNOWN: &[(&str, &str)] = &[
        (codes::INVALID_INPUT, "indicator rejected the input"),
        (codes::INVALID_PARAMS, "indicator parameters are invalid"),
        (codes::COMPUTE_FAILED, "indicator computation failed"),
        (codes::PANIC, "indicator failed internally"),
        (codes::SEQUENCE_GAP, "live sequence gap; session will be reopened"),
        (codes::STALE_GENERATION, "stale live generation"),
        (codes::UNKNOWN_SESSION, "live session not open"),
        (codes::LIMIT_EXCEEDED, "indicator request or output limit exceeded"),
        (codes::INVALID_OUTPUT, "indicator returned invalid output"),
        (codes::PROTOCOL, "indicator protocol violation"),
        (codes::UNAVAILABLE, "indicator unavailable"),
        (codes::TIMEOUT, "indicator timed out"),
        (codes::CRASHED, "indicator process exited"),
        (codes::INCOMPATIBLE, "indicator library is incompatible"),
    ];
    let (code, msg) = KNOWN
        .iter()
        .find(|(c, _)| *c == e.code)
        .copied()
        .unwrap_or((codes::COMPUTE_FAILED, "indicator computation failed"));
    if e.message != msg {
        let detail: String = e.message.chars().take(256).collect();
        eprintln!("[indicator:{id}] {}: {detail}", e.code);
    }
    PluginError::new(code, msg)
}

fn backoff(failures: u32) -> Duration {
    Duration::from_millis((500u64 << failures.min(7)).min(60_000))
}

impl IndicatorManager {
    /// Empty manager (no indicators configured).
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            entries: Vec::new(),
            history_timeout: Duration::from_secs(10),
            live_timeout: Duration::from_secs(2),
        })
    }

    /// Start one supervisor per enabled library. Must be called inside a Tokio runtime.
    pub fn start(cfg: &Config) -> Arc<Self> {
        let sec = &cfg.indicators;
        let history_timeout = Duration::from_millis(sec.history_timeout_ms);
        let live_timeout = Duration::from_millis(sec.live_timeout_ms);
        let mut entries = Vec::new();
        for lib in &sec.library {
            let (tx, rx) = if lib.enabled {
                let (tx, rx) = mpsc::channel(sec.queue_depth);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            let entry = Arc::new(Entry {
                cfg: lib.clone(),
                params_json: lib.params_json(),
                state: RwLock::new(EntryState {
                    status: if lib.enabled { Status::Starting } else { Status::Unavailable },
                    descriptor: None,
                    error: (!lib.enabled).then(|| unavailable("indicator disabled")),
                    restarts: 0,
                    epoch: 0,
                }),
                tx,
            });
            if let (Some(rx), Some(runner)) = (rx, sec.runner.clone()) {
                let mut rc = RunnerConfig::new(runner, lib.path.clone());
                rc.history_timeout = history_timeout;
                rc.live_timeout = live_timeout;
                rc.limits = Limits::default();
                tokio::spawn(supervise(entry.clone(), rc, rx));
            }
            entries.push(entry);
        }
        Arc::new(Self { entries, history_timeout, live_timeout })
    }

    pub fn infos(&self) -> Vec<IndicatorInfo> {
        self.entries
            .iter()
            .map(|e| {
                let s = e.state.read().unwrap();
                IndicatorInfo {
                    id: e.cfg.id.clone(),
                    status: s.status,
                    modes: e.cfg.modes.clone(),
                    version: s.descriptor.as_ref().map(|d| d.version.clone()),
                    descriptor: s.descriptor.clone(),
                    error: s.error.clone(),
                    restarts: s.restarts,
                }
            })
            .collect()
    }

    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.cfg.id.clone()).collect()
    }

    fn entry(&self, id: &str) -> Option<&Arc<Entry>> {
        self.entries.iter().find(|e| e.cfg.id == id)
    }

    /// Ready descriptor and runner epoch for an id.
    pub fn ready(&self, id: &str) -> Option<(Descriptor, u64)> {
        let e = self.entry(id)?;
        let s = e.state.read().unwrap();
        (s.status == Status::Ready).then(|| (s.descriptor.clone().unwrap(), s.epoch))
    }

    pub fn modes(&self, id: &str) -> Vec<Mode> {
        self.entry(id)
            .map(|e| e.cfg.modes.iter().map(|m| if m == "source" { Mode::Source } else { Mode::Aggregate }).collect())
            .unwrap_or_default()
    }

    /// Largest declared warm-up among the given ready indicators for `tf`.
    pub fn max_context(&self, ids: &[String], tf: &str) -> usize {
        ids.iter().filter_map(|id| self.ready(id)).map(|(d, _)| d.context.bars_for(tf) as usize).max().unwrap_or(0)
    }

    async fn submit(
        &self,
        id: &str,
        job: impl FnOnce(Reply) -> Job,
        deadline: Duration,
    ) -> Result<Output, PluginError> {
        self.submit_raw(id, job, deadline).await.map_err(|e| sanitize(id, e))
    }

    async fn submit_raw(
        &self,
        id: &str,
        job: impl FnOnce(Reply) -> Job,
        deadline: Duration,
    ) -> Result<Output, PluginError> {
        let e = self.entry(id).ok_or_else(|| unavailable("indicator not installed"))?;
        let tx = e.tx.as_ref().ok_or_else(|| unavailable("indicator disabled"))?;
        let (rtx, rrx) = oneshot::channel();
        tx.try_send(job(rtx)).map_err(|_| unavailable("indicator busy or restarting"))?;
        match tokio::time::timeout(deadline, rrx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(unavailable("indicator restarting")),
            Err(_) => Err(PluginError::new(codes::TIMEOUT, "indicator timed out")),
        }
    }

    pub async fn history(&self, id: &str, req: &HistoryRequest) -> Result<Output, PluginError> {
        let body = serde_json::to_vec(req).map_err(|_| PluginError::invalid_input("unencodable input"))?;
        let expected: Vec<i64> = req.bars[req.output_from..].iter().map(|b| b.time).collect();
        // Queue wait + runner deadline + kill margin.
        let deadline = self.history_timeout * 2 + Duration::from_secs(3);
        self.submit(id, |reply| Job::History { body, expected, reply }, deadline).await
    }

    pub async fn live_open(
        &self,
        id: &str,
        session: u64,
        generation: u64,
        init: LiveInit,
    ) -> Result<Output, PluginError> {
        let deadline = self.history_timeout + self.live_timeout + Duration::from_secs(3);
        self.submit(id, |reply| Job::LiveOpen { session, generation, init, reply }, deadline).await
    }

    pub async fn live_update(
        &self,
        id: &str,
        session: u64,
        generation: u64,
        upd: LiveUpdate,
    ) -> Result<Output, PluginError> {
        let deadline = self.live_timeout * 2 + Duration::from_secs(3);
        self.submit(id, |reply| Job::LiveUpdate { session, generation, upd, reply }, deadline).await
    }

    pub fn live_close(&self, id: &str, session: u64) {
        if let Some(tx) = self.entry(id).and_then(|e| e.tx.as_ref()) {
            let _ = tx.try_send(Job::LiveClose { session });
        }
    }
}

fn set_state(e: &Entry, f: impl FnOnce(&mut EntryState)) {
    f(&mut e.state.write().unwrap());
}

async fn supervise(entry: Arc<Entry>, rc: RunnerConfig, mut rx: mpsc::Receiver<Job>) {
    let id = entry.cfg.id.clone();
    let mut failures: u32 = 0;
    loop {
        set_state(&entry, |s| s.status = Status::Starting);
        match RunnerClient::spawn(rc.clone()).await {
            Ok(mut client) if client.descriptor().id == id => {
                failures = 0;
                set_state(&entry, |s| {
                    s.status = Status::Ready;
                    s.descriptor = Some(client.descriptor().clone());
                    s.error = None;
                    s.epoch += 1;
                });
                eprintln!("[indicator:{id}] runner ready version={}", client.descriptor().version);
                loop {
                    let Some(job) = rx.recv().await else {
                        client.shutdown().await;
                        return;
                    };
                    let (reply, res) = match job {
                        Job::History { body, expected, reply } => {
                            (reply, client.history_raw(&entry.params_json, &body, expected, 0).await)
                        }
                        Job::LiveOpen { session, generation, init, reply } => {
                            (reply, client.live_open(session, generation, &entry.params_json, &init).await)
                        }
                        Job::LiveUpdate { session, generation, upd, reply } => {
                            (reply, client.live_update(session, generation, &upd).await)
                        }
                        Job::LiveClose { session } => {
                            let _ = client.live_close(session).await;
                            if client.is_dead() {
                                break;
                            }
                            continue;
                        }
                    };
                    let res = res.map_err(|e| {
                        if e.is_fatal() {
                            eprintln!("[indicator:{id}] runner failure: {}", e.local_detail());
                        }
                        e.public()
                    });
                    if let Err(pe) = &res {
                        if client.is_dead() {
                            set_state(&entry, |s| {
                                s.status = Status::Unavailable;
                                s.error = Some(sanitize(&id, pe.clone()));
                            });
                        }
                    }
                    let _ = reply.send(res);
                    if client.is_dead() {
                        break;
                    }
                }
            }
            Ok(client) => {
                eprintln!("[indicator:{id}] descriptor id {} does not match configuration", client.descriptor().id);
                client.shutdown().await;
                set_state(&entry, |s| {
                    s.status = Status::Unavailable;
                    s.error = Some(PluginError::new(codes::INCOMPATIBLE, "descriptor id does not match configuration"));
                });
            }
            Err(e) => {
                eprintln!("[indicator:{id}] runner start failed: {}", e.local_detail());
                set_state(&entry, |s| {
                    s.status = Status::Unavailable;
                    s.error = Some(sanitize(&id, e.public()));
                });
            }
        }
        failures = failures.saturating_add(1);
        set_state(&entry, |s| s.restarts += 1);
        // During backoff, reject queued work immediately instead of letting it wait.
        let sleep = tokio::time::sleep(backoff(failures));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                job = rx.recv() => match job {
                    None => return,
                    Some(Job::History { reply, .. }) | Some(Job::LiveOpen { reply, .. }) | Some(Job::LiveUpdate { reply, .. }) => {
                        let _ = reply.send(Err(unavailable("indicator restarting")));
                    }
                    Some(Job::LiveClose { .. }) => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------- input mapping

/// Canonical bar → plugin input. Pure field projection; no derived values.
pub fn input_bar(b: &Bar) -> InputBar {
    InputBar {
        time: b.time,
        po: b.price_open,
        ph: b.price_high,
        pl: b.price_low,
        price_close: b.price_close,
        apo: b.aggregate_price_open,
        aph: b.aggregate_price_high,
        apl: b.aggregate_price_low,
        apc: b.aggregate_price_close,
        cvd_usdt: b.cvd_usdt,
        ac: b.aggregate_cvd_usdt,
        delta_usdt: b.delta_usdt,
        ad: b.aggregate_delta_usdt,
        bvb: b.buy_volume_base,
        abvb: b.aggregate_buy_volume_base,
        svb: b.sell_volume_base,
        asvb: b.aggregate_sell_volume_base,
        buy_volume_usdt: b.buy_volume_usdt,
        agb: b.aggregate_buy_volume_usdt,
        sell_volume_usdt: b.sell_volume_usdt,
        ags: b.aggregate_sell_volume_usdt,
        trades: b.trades,
    }
}

pub fn mode_str(m: Mode) -> &'static str {
    match m {
        Mode::Source => "source",
        Mode::Aggregate => "aggregate",
    }
}

/// Per-mode result attached to a history page.
#[derive(Debug, Clone, Serialize)]
pub struct ModeResult {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Output>,
}

/// Versioned per-indicator output alongside the canonical base bars.
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorHistory {
    pub version: Option<String>,
    pub modes: BTreeMap<&'static str, ModeResult>,
}

/// Scope identity without mode.
#[derive(Debug, Clone)]
pub struct ScopeBase {
    pub exchange: String,
    pub market: String,
    pub symbol: String,
    pub tf: String,
}

impl ScopeBase {
    pub fn with_mode(&self, mode: Mode) -> Scope {
        Scope {
            exchange: self.exchange.clone(),
            market: self.market.clone(),
            symbol: self.symbol.clone(),
            tf: self.tf.clone(),
            mode,
        }
    }
}

/// Public status for a computation window that contained invalid canonical
/// input (non-finite bar removed from the base stream). Fail-closed: no
/// values are produced; the code is the existing `unavailable`.
pub fn invalid_input_window() -> PluginError {
    unavailable("computation window contains invalid canonical input")
}

/// True when any invalid (dropped, non-renderable) bar time lies inside the
/// computation window starting at `window_start` (`None` = window starts at
/// the first loaded row, so every earlier invalid row may belong to it).
/// Time-based, so it is a conservative superset of a row-count window.
pub fn window_poisoned(invalid_times: &[i64], window_start: Option<i64>) -> bool {
    match window_start {
        None => !invalid_times.is_empty(),
        Some(t0) => invalid_times.iter().any(|&t| t >= t0),
    }
}

#[cfg(test)]
pub async fn compute_history(
    mgr: &IndicatorManager,
    ids: &[String],
    scope: &ScopeBase,
    revision: u64,
    bars: &[Bar],
    keep_start: usize,
) -> BTreeMap<String, IndicatorHistory> {
    compute_history_guarded(mgr, ids, scope, revision, bars, keep_start, &[]).await
}

/// Compute requested indicators over `bars[keep_start..]`, using up to each
/// indicator's declared context from `bars[..keep_start]` as warm-up.
/// Never fails: unavailable indicators are reported per mode.
/// `invalid_times` are the times of rows removed as non-renderable BEFORE
/// filtering; any mode whose window covers one is `unavailable` (fail-closed).
pub async fn compute_history_guarded(
    mgr: &IndicatorManager,
    ids: &[String],
    scope: &ScopeBase,
    revision: u64,
    bars: &[Bar],
    keep_start: usize,
    invalid_times: &[i64],
) -> BTreeMap<String, IndicatorHistory> {
    let inputs: Arc<Vec<InputBar>> = Arc::new(bars.iter().map(input_bar).collect());
    let mut jobs = Vec::new();
    for id in ids {
        let modes = mgr.modes(id);
        let ready = mgr.ready(id);
        for mode in modes {
            let inputs = inputs.clone();
            let ready = ready.clone();
            let scope = scope.with_mode(mode);
            let id = id.clone();
            jobs.push(async move {
                let res = match ready {
                    None => Err(unavailable("indicator not ready")),
                    Some((d, _)) => {
                        let ctx = d.context.bars_for(&scope.tf) as usize;
                        let from = keep_start.saturating_sub(ctx);
                        // from == len (empty tail, zero context): empty window, nothing to poison.
                        let start = (from > 0).then(|| inputs.get(from).map_or(i64::MAX, |b| b.time));
                        if window_poisoned(invalid_times, start) {
                            return (id, mode, Err(invalid_input_window()));
                        }
                        let req = HistoryRequest {
                            scope,
                            revision,
                            output_from: keep_start - from,
                            bars: inputs[from..].to_vec(),
                        };
                        mgr.history(&id, &req).await
                    }
                };
                (id, mode, res)
            });
        }
    }
    let results = futures::future::join_all(jobs).await;
    let mut out: BTreeMap<String, IndicatorHistory> = BTreeMap::new();
    for id in ids {
        out.insert(
            id.clone(),
            IndicatorHistory { version: mgr.ready(id).map(|(d, _)| d.version), modes: BTreeMap::new() },
        );
    }
    for (id, mode, res) in results {
        let r = match res {
            Ok(o) => ModeResult { status: "ok", error: None, output: Some(o) },
            Err(e) => ModeResult { status: "unavailable", error: Some(e), output: None },
        };
        out.get_mut(&id).unwrap().modes.insert(mode_str(mode), r);
    }
    out
}
