//! Single runner process client: spawn with minimal environment, strict frame
//! I/O, per-request deadline, kill on timeout/crash/protocol violation.

use cvd_indicator_sdk::model::{codes, Descriptor, HistoryRequest, LiveInit, LiveUpdate, Output, PluginError};
use cvd_indicator_sdk::protocol::{FrameHeader, FrameKind, Limits, HEADER_LEN};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Operator-supplied runner settings. Paths come only from host configuration.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub runner_path: PathBuf,
    pub library_path: PathBuf,
    pub limits: Limits,
    pub startup_timeout: Duration,
    pub history_timeout: Duration,
    pub live_timeout: Duration,
    /// Bytes of runner stderr retained for local diagnostics.
    pub stderr_capture_bytes: usize,
}

impl RunnerConfig {
    pub fn new(runner_path: impl Into<PathBuf>, library_path: impl Into<PathBuf>) -> Self {
        Self {
            runner_path: runner_path.into(),
            library_path: library_path.into(),
            limits: Limits::default(),
            startup_timeout: Duration::from_secs(5),
            history_timeout: Duration::from_secs(10),
            live_timeout: Duration::from_secs(2),
            stderr_capture_bytes: 16 * 1024,
        }
    }
}

/// Failure classes. Only [`HostError::public`] may leave the host.
#[derive(Debug)]
pub enum HostError {
    /// Library returned a well-formed error; runner remains usable.
    Plugin(PluginError),
    /// Runner exceeded its deadline and was killed.
    Timeout,
    /// Runner exited / pipe closed; `detail` is local-only.
    Crashed { detail: String },
    /// Framing or identity violation; runner was killed. `detail` local-only.
    Protocol { detail: String },
    /// Runner could not be started or handshake failed.
    Spawn { public: PluginError, detail: String },
}

impl HostError {
    /// Whether the runner process is no longer usable.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, HostError::Plugin(_))
    }
    pub fn public(&self) -> PluginError {
        match self {
            HostError::Plugin(e) => e.clone(),
            HostError::Timeout => PluginError::new(codes::TIMEOUT, "indicator timed out"),
            HostError::Crashed { .. } => PluginError::new(codes::CRASHED, "indicator process exited"),
            HostError::Protocol { .. } => PluginError::new(codes::PROTOCOL, "indicator protocol violation"),
            HostError::Spawn { public, .. } => public.clone(),
        }
    }
    /// Local diagnostic text (never sent to clients).
    pub fn local_detail(&self) -> String {
        match self {
            HostError::Plugin(e) => e.to_string(),
            HostError::Timeout => "timeout".into(),
            HostError::Crashed { detail } | HostError::Protocol { detail } => detail.clone(),
            HostError::Spawn { detail, .. } => detail.clone(),
        }
    }
}

/// Bounded tail of runner stderr for local logs.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<Vec<u8>>>);

impl StderrTail {
    pub fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

pub struct RunnerClient {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: u64,
    cfg: RunnerConfig,
    descriptor: Descriptor,
    descriptor_json: Vec<u8>,
    stderr: StderrTail,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    dead: bool,
}

async fn read_frame_async(r: &mut ChildStdout, max: usize) -> Result<(FrameHeader, Vec<u8>), HostError> {
    let mut hb = [0u8; HEADER_LEN];
    r.read_exact(&mut hb).await.map_err(|e| HostError::Crashed { detail: format!("read header: {e}") })?;
    let h = FrameHeader::decode(&hb, max).map_err(|e| HostError::Protocol { detail: e.to_string() })?;
    let mut payload = vec![0u8; h.payload_len()];
    r.read_exact(&mut payload).await.map_err(|e| HostError::Crashed { detail: format!("read payload: {e}") })?;
    Ok((h, payload))
}

impl RunnerClient {
    /// Spawn the runner with an empty environment, perform the handshake and
    /// validate the descriptor.
    pub async fn spawn(cfg: RunnerConfig) -> Result<Self, HostError> {
        let spawn_err = |d: String| HostError::Spawn {
            public: PluginError::new(codes::UNAVAILABLE, "indicator runner failed to start"),
            detail: d,
        };
        let mut cmd = Command::new(&cfg.runner_path);
        cmd.arg("--library")
            .arg(&cfg.library_path)
            .arg("--max-request-bytes")
            .arg(cfg.limits.max_request_bytes.to_string())
            .arg("--max-response-bytes")
            .arg(cfg.limits.max_response_bytes.to_string())
            .arg("--max-sessions")
            .arg(cfg.limits.max_sessions.to_string())
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| spawn_err(format!("spawn {}: {e}", cfg.runner_path.display())))?;
        let stdin = child.stdin.take().ok_or_else(|| spawn_err("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| spawn_err("no stdout".into()))?;
        let stderr = StderrTail::default();
        let mut stderr_task = None;
        if let Some(mut err) = child.stderr.take() {
            let tail = stderr.clone();
            let cap = cfg.stderr_capture_bytes;
            stderr_task = Some(tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.0.lock().unwrap();
                    t.extend_from_slice(&buf[..n]);
                    if t.len() > cap {
                        let cut = t.len() - cap;
                        t.drain(..cut);
                    }
                }
            }));
        }
        let mut me = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            cfg,
            descriptor: Descriptor {
                id: String::new(),
                version: String::new(),
                name: String::new(),
                description: String::new(),
                abi: 0,
                context: Default::default(),
                params: vec![],
                inputs: vec![],
                outputs: vec![],
                live: false,
            },
            descriptor_json: vec![],
            stderr,
            stderr_task,
            dead: false,
        };
        let hello = serde_json::to_vec(&serde_json::json!({"protocol": 1, "abi": cvd_indicator_sdk::abi::ABI_VERSION}))
            .unwrap();
        let timeout = me.cfg.startup_timeout;
        match me.round_trip(FrameKind::Hello, 0, 0, &[], &hello, timeout).await {
            Ok((FrameKind::Ready, body)) => {
                let desc: Descriptor = serde_json::from_slice(&body)
                    .map_err(|e| HostError::Protocol { detail: format!("descriptor json: {e}") })?;
                desc.validate().map_err(|e| HostError::Spawn { public: e.clone(), detail: e.message })?;
                me.descriptor = desc;
                me.descriptor_json = body;
                Ok(me)
            }
            Ok((_, body)) => {
                let e: PluginError = serde_json::from_slice(&body)
                    .unwrap_or_else(|_| PluginError::new(codes::INCOMPATIBLE, "handshake rejected"));
                me.kill().await;
                let detail = format!("{} | stderr: {}", e, me.stderr.snapshot());
                Err(HostError::Spawn { public: PluginError::new(codes::INCOMPATIBLE, e.message), detail })
            }
            Err(e) => {
                me.kill().await;
                Err(spawn_err(format!("{} | stderr: {}", e.local_detail(), me.stderr.snapshot())))
            }
        }
    }

    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    pub fn descriptor_json(&self) -> &[u8] {
        &self.descriptor_json
    }
    pub fn stderr_tail(&self) -> String {
        self.stderr.snapshot()
    }
    pub fn is_dead(&self) -> bool {
        self.dead
    }
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Kill and reap with a bounded wait, then drain remaining stderr
    /// (bounded) so local diagnostics are complete.
    pub async fn kill(&mut self) {
        self.dead = true;
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
        if let Some(t) = self.stderr_task.take() {
            let _ = tokio::time::timeout(Duration::from_millis(500), t).await;
        }
    }

    pub async fn shutdown(mut self) {
        let h = FrameHeader::new(FrameKind::Shutdown, 0, 0, 0);
        let _ = self.stdin.write_all(&h.encode()).await;
        let _ = self.stdin.flush().await;
        if tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await.is_err() {
            self.kill().await;
        }
        self.dead = true;
    }

    async fn round_trip(
        &mut self,
        kind: FrameKind,
        session: u64,
        generation: u64,
        params: &[u8],
        body: &[u8],
        timeout: Duration,
    ) -> Result<(FrameKind, Vec<u8>), HostError> {
        if self.dead {
            return Err(HostError::Crashed { detail: "runner already dead".into() });
        }
        if params.len() + body.len() > self.cfg.limits.max_request_bytes {
            return Err(HostError::Plugin(PluginError::new(codes::LIMIT_EXCEEDED, "request exceeds limit")));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut h = FrameHeader::new(kind, id, session, generation);
        h.params_len = params.len() as u32;
        h.body_len = body.len() as u32;
        let max = self.cfg.limits.max_response_bytes;
        let io = async {
            let mut frame = Vec::with_capacity(HEADER_LEN + params.len() + body.len());
            frame.extend_from_slice(&h.encode());
            frame.extend_from_slice(params);
            frame.extend_from_slice(body);
            self.stdin.write_all(&frame).await.map_err(|e| HostError::Crashed { detail: format!("write: {e}") })?;
            self.stdin.flush().await.map_err(|e| HostError::Crashed { detail: format!("flush: {e}") })?;
            read_frame_async(&mut self.stdout, max).await
        };
        let res = match tokio::time::timeout(timeout, io).await {
            Err(_) => Err(HostError::Timeout),
            Ok(r) => r,
        };
        let (rh, payload) = match res {
            Ok(x) => x,
            Err(e) => {
                self.kill().await;
                if let HostError::Crashed { detail } = e {
                    let status = self.child.try_wait().ok().flatten();
                    return Err(HostError::Crashed {
                        detail: format!("{detail}; exit={status:?}; stderr: {}", self.stderr.snapshot()),
                    });
                }
                return Err(e);
            }
        };
        if rh.request_id != id || rh.session != session || rh.generation != generation || rh.params_len != 0 {
            self.kill().await;
            return Err(HostError::Protocol { detail: format!("response identity mismatch: {rh:?}") });
        }
        if !matches!(rh.kind, FrameKind::Result | FrameKind::Error | FrameKind::Ready) {
            self.kill().await;
            return Err(HostError::Protocol { detail: format!("unexpected response kind {:?}", rh.kind) });
        }
        Ok((rh.kind, payload))
    }

    async fn call(
        &mut self,
        kind: FrameKind,
        session: u64,
        generation: u64,
        params: &[u8],
        body: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, HostError> {
        match self.round_trip(kind, session, generation, params, body, timeout).await? {
            (FrameKind::Result, v) => Ok(v),
            (FrameKind::Error, v) => Err(HostError::Plugin(
                serde_json::from_slice(&v).unwrap_or_else(|_| PluginError::new(codes::PROTOCOL, "unreadable error")),
            )),
            (k, _) => {
                self.kill().await;
                Err(HostError::Protocol { detail: format!("unexpected {k:?}") })
            }
        }
    }

    fn parse_output(&self, bytes: &[u8], expected: Option<&[i64]>) -> Result<Output, HostError> {
        let out: Output = serde_json::from_slice(bytes)
            .map_err(|_| HostError::Plugin(PluginError::new(codes::INVALID_OUTPUT, "output is not valid JSON")))?;
        out.validate(&self.descriptor, expected).map_err(HostError::Plugin)?;
        Ok(out)
    }

    /// Stateless historical window. `params` is raw JSON (already canonical).
    pub async fn history(&mut self, params: &[u8], req: &HistoryRequest, generation: u64) -> Result<Output, HostError> {
        if req.bars.len() > self.cfg.limits.max_bars_per_request {
            return Err(HostError::Plugin(PluginError::new(codes::LIMIT_EXCEEDED, "too many bars")));
        }
        let body = serde_json::to_vec(req).expect("serialize history");
        self.history_raw(
            params,
            &body,
            req.bars[req.output_from.min(req.bars.len())..].iter().map(|b| b.time).collect(),
            generation,
        )
        .await
    }

    /// History with a pre-encoded body (encode once, forward bytes).
    pub async fn history_raw(
        &mut self,
        params: &[u8],
        body: &[u8],
        expected_times: Vec<i64>,
        generation: u64,
    ) -> Result<Output, HostError> {
        let t = self.cfg.history_timeout;
        let v = self.call(FrameKind::History, 0, generation, params, body, t).await?;
        self.parse_output(&v, Some(&expected_times))
    }

    pub async fn live_open(
        &mut self,
        session: u64,
        generation: u64,
        params: &[u8],
        init: &LiveInit,
    ) -> Result<Output, HostError> {
        let body = serde_json::to_vec(init).expect("serialize live init");
        let t = self.cfg.live_timeout.max(self.cfg.history_timeout);
        let v = self.call(FrameKind::LiveOpen, session, generation, params, &body, t).await?;
        let expected: Vec<i64> = init.bars.last().map(|b| vec![b.time]).unwrap_or_default();
        self.parse_output(&v, Some(&expected))
    }

    pub async fn live_update(&mut self, session: u64, generation: u64, upd: &LiveUpdate) -> Result<Output, HostError> {
        let body = serde_json::to_vec(upd).expect("serialize live update");
        let t = self.cfg.live_timeout;
        let v = self.call(FrameKind::LiveUpdate, session, generation, &[], &body, t).await?;
        let out = self.parse_output(&v, None)?;
        let min_touched = upd
            .ops
            .iter()
            .map(|op| match op {
                cvd_indicator_sdk::LiveOp::Append { bar } | cvd_indicator_sdk::LiveOp::Replace { bar } => bar.time,
            })
            .min();
        if let (Some(first), Some(min)) = (out.times.first(), min_touched) {
            if *first != min {
                return Err(HostError::Plugin(PluginError::new(
                    codes::INVALID_OUTPUT,
                    "live output not aligned to delta",
                )));
            }
        }
        Ok(out)
    }

    pub async fn live_close(&mut self, session: u64) -> Result<(), HostError> {
        let t = self.cfg.live_timeout;
        self.call(FrameKind::LiveClose, session, 0, &[], &[], t).await.map(|_| ())
    }
}
