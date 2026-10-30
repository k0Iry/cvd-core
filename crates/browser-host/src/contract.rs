//! Worker-side contract enforcement for one plugin instance: checked memory
//! regions, descriptor acceptance, request bookkeeping and output validation.

use crate::HostError;
use cvd_indicator_sdk::abi::{OP_HISTORY, OP_LIVE_INIT, OP_LIVE_UPDATE, STATUS_ERROR, STATUS_OK};
use cvd_indicator_sdk::wasm::{
    MAX_DESCRIPTOR_BYTES, MAX_INPUT_BYTES, MAX_MEMORY_BYTES, MAX_OUTPUT_BYTES, WASM_ABI_VERSION,
};
use cvd_indicator_sdk::author::{check_bars, LIVE_REPLACE_SLACK};
use cvd_indicator_sdk::{codes, Descriptor, HistoryRequest, LiveInit, LiveOp, LiveUpdate, Output, PluginError};

/// Host mirror of the SDK live window (author.rs `LiveState`): same cap
/// `context + 1 + LIVE_REPLACE_SLACK`, fixed revision, exact seq, replace
/// membership and touched-range output axis. Committed only after a
/// validated `STATUS_OK` result; any error leaves it unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMirror {
    pub revision: u64,
    pub seq: u64,
    pub times: Vec<i64>,
    pub cap: usize,
}

#[derive(Debug)]
struct InFlight {
    expected: Vec<i64>,
    next_live: Option<LiveMirror>,
}

fn admit(r: Result<(), PluginError>) -> Result<(), HostError> {
    r.map_err(|e| HostError::new("invalid_input", e.message))
}

/// Validate a plugin-reported `(ptr, len)` against the live memory size.
/// All arithmetic is u64 so wasm32 values cannot overflow.
pub fn check_region(mem_len: u64, ptr: u64, len: u64, max: u64) -> Result<(), HostError> {
    if mem_len > MAX_MEMORY_BYTES {
        return Err(HostError::new("memory_limit", "plugin memory exceeds 512 MiB"));
    }
    if len > max {
        return Err(HostError::new("invalid_output_region", "result exceeds size limit"));
    }
    if len > 0 && (ptr == 0 || ptr.checked_add(len).is_none_or(|end| end > mem_len)) {
        return Err(HostError::new("invalid_output_region", "result pointer/length outside plugin memory"));
    }
    Ok(())
}

/// Region limit kinds used by the FFI.
pub fn region_limit(kind: u32) -> u64 {
    match kind {
        0 => MAX_DESCRIPTOR_BYTES as u64,
        _ => MAX_OUTPUT_BYTES as u64,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Fresh,
    AbiOk,
    Described,
    Created,
    Poisoned,
}

/// One plugin instance's host-side state machine (lives in its Worker).
#[derive(Debug)]
pub struct PluginSession {
    stage: Stage,
    desc: Option<Descriptor>,
    pending: Option<InFlight>,
    live: Option<LiveMirror>,
}

impl Default for PluginSession {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginSession {
    pub const fn new() -> Self {
        Self { stage: Stage::Fresh, desc: None, pending: None, live: None }
    }

    pub fn descriptor(&self) -> Option<&Descriptor> {
        self.desc.as_ref()
    }

    /// Committed live mirror (for tests/diagnostics).
    pub fn live(&self) -> Option<&LiveMirror> {
        self.live.as_ref()
    }

    pub fn is_poisoned(&self) -> bool {
        self.stage == Stage::Poisoned
    }

    fn fail(&mut self, e: HostError) -> HostError {
        self.stage = Stage::Poisoned;
        self.pending = None;
        e
    }

    fn require(&self, s: Stage) -> Result<(), HostError> {
        if self.stage == Stage::Poisoned {
            return Err(HostError::new("poisoned", "plugin instance is no longer usable"));
        }
        if self.stage != s {
            return Err(HostError::new("lifecycle", format!("expected stage {s:?}, at {:?}", self.stage)));
        }
        Ok(())
    }

    /// A trap/abort or deadline happened: no further calls are allowed.
    pub fn poison(&mut self, code: &str) -> HostError {
        self.fail(HostError::new(code, "plugin instance terminated"))
    }

    pub fn accept_abi(&mut self, v: u32) -> Result<(), HostError> {
        self.require(Stage::Fresh)?;
        if v != WASM_ABI_VERSION {
            return Err(
                self.fail(HostError::new("abi_mismatch", format!("module ABI {v}, host expects {WASM_ABI_VERSION}")))
            );
        }
        self.stage = Stage::AbiOk;
        Ok(())
    }

    pub fn accept_descriptor(&mut self, status: i32, bytes: &[u8]) -> Result<&Descriptor, HostError> {
        self.require(Stage::AbiOk)?;
        if status != STATUS_OK || bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(self.fail(HostError::new("bad_descriptor", "describe failed")));
        }
        let d: Descriptor = match serde_json::from_slice(bytes) {
            Ok(d) => d,
            Err(_) => return Err(self.fail(HostError::new("bad_descriptor", "descriptor is not valid JSON"))),
        };
        if let Err(e) = d.validate() {
            return Err(self.fail(HostError::new("bad_descriptor", e.message)));
        }
        self.desc = Some(d);
        self.stage = Stage::Described;
        Ok(self.desc.as_ref().expect("set"))
    }

    pub fn accept_create(&mut self, status: i32, bytes: &[u8]) -> Result<(), HostError> {
        self.require(Stage::Described)?;
        match status {
            STATUS_OK => {
                self.stage = Stage::Created;
                Ok(())
            }
            // Parameter rejection is a normal, non-fatal outcome: instance can be retried.
            STATUS_ERROR => Err(plugin_error(bytes)),
            _ => Err(self.fail(HostError::new("abi_violation", format!("create returned status {status}")))),
        }
    }

    /// Record the expected output axis for a request about to be sent.
    pub fn begin(&mut self, op: u32, body: &[u8]) -> Result<(), HostError> {
        self.require(Stage::Created)?;
        if self.pending.is_some() {
            return Err(HostError::new("lifecycle", "a call is already in flight"));
        }
        if body.len() > MAX_INPUT_BYTES {
            return Err(HostError::new("input_too_large", "request body exceeds 32 MiB"));
        }
        let bad = |w: &str| HostError::new("invalid_input", format!("malformed {w} body"));
        let desc = self.desc.as_ref().expect("described");
        let (expected, next_live) = match op {
            OP_HISTORY => {
                let r: HistoryRequest = serde_json::from_slice(body).map_err(|_| bad("history"))?;
                if r.output_from > r.bars.len() {
                    return Err(HostError::new("invalid_input", "output_from beyond bars"));
                }
                admit(check_bars(&r.bars))?;
                (r.bars[r.output_from..].iter().map(|b| b.time).collect(), None)
            }
            OP_LIVE_INIT => {
                let r: LiveInit = serde_json::from_slice(body).map_err(|_| bad("live init"))?;
                if !desc.live {
                    return Err(HostError::new("invalid_input", "indicator does not support live sessions"));
                }
                admit(check_bars(&r.bars))?;
                let cap = desc.context.bars_for(&r.scope.tf) as usize + 1 + LIVE_REPLACE_SLACK;
                let all: Vec<i64> = r.bars.iter().map(|b| b.time).collect();
                let times = all[all.len().saturating_sub(cap)..].to_vec();
                let from = times.len().saturating_sub(1);
                (times[from..].to_vec(), Some(LiveMirror { revision: r.revision, seq: 0, times, cap }))
            }
            OP_LIVE_UPDATE => {
                let r: LiveUpdate = serde_json::from_slice(body).map_err(|_| bad("live update"))?;
                let st = self.live.as_ref().ok_or_else(|| HostError::new("lifecycle", "live session not initialised"))?;
                if r.revision != st.revision {
                    return Err(HostError::new("stale_revision", "live update revision differs from live init"));
                }
                if r.seq != st.seq + 1 {
                    return Err(HostError::new("sequence_gap", format!("expected seq {}, got {}", st.seq + 1, r.seq)));
                }
                let mut times = st.times.clone();
                let mut touched: Vec<i64> = Vec::new();
                for op in &r.ops {
                    match op {
                        LiveOp::Append { bar } => {
                            admit(check_bars(std::slice::from_ref(bar)))?;
                            if times.last().is_some_and(|l| *l >= bar.time) {
                                return Err(HostError::new("invalid_input", "append must be after last bar"));
                            }
                            times.push(bar.time);
                            touched.push(bar.time);
                        }
                        LiveOp::Replace { bar } => {
                            admit(check_bars(std::slice::from_ref(bar)))?;
                            if !times.contains(&bar.time) {
                                return Err(HostError::new("invalid_input", "replace target not in live window"));
                            }
                            touched.push(bar.time);
                        }
                    }
                }
                if times.len() > st.cap {
                    times.drain(..times.len() - st.cap);
                }
                touched.sort_unstable();
                let from = match touched.first() {
                    None => times.len(),
                    Some(t) => times
                        .iter()
                        .position(|x| x == t)
                        .ok_or_else(|| HostError::new("invalid_input", "delta exceeds live window"))?,
                };
                let next = LiveMirror { revision: st.revision, seq: r.seq, times, cap: st.cap };
                (next.times[from..].to_vec(), Some(next))
            }
            _ => return Err(HostError::new("invalid_input", "unknown operation")),
        };
        self.pending = Some(InFlight { expected, next_live });
        Ok(())
    }

    /// Validate the plugin result for the in-flight call. Returns canonical
    /// validated `Output` JSON.
    pub fn accept_result(&mut self, status: i32, bytes: &[u8]) -> Result<String, HostError> {
        self.require(Stage::Created)?;
        let Some(InFlight { expected, next_live }) = self.pending.take() else {
            return Err(HostError::new("lifecycle", "no call in flight"));
        };
        match status {
            STATUS_OK => {
                let out: Output = match serde_json::from_slice(bytes) {
                    Ok(o) => o,
                    Err(_) => return Err(self.fail(HostError::new("invalid_output", "output is not valid JSON"))),
                };
                let desc = self.desc.as_ref().expect("described");
                if let Err(e) = out.validate(desc, Some(&expected)) {
                    return Err(self.fail(HostError::new("invalid_output", e.message)));
                }
                if next_live.is_some() {
                    self.live = next_live;
                }
                serde_json::to_string(&out).map_err(|_| HostError::new("invalid_output", "serialisation failed"))
            }
            STATUS_ERROR => Err(plugin_error(bytes)),
            _ => Err(self.fail(HostError::new("abi_violation", format!("invoke returned status {status}")))),
        }
    }
}

fn plugin_error(bytes: &[u8]) -> HostError {
    match serde_json::from_slice::<PluginError>(bytes) {
        Ok(e) => {
            let code = codes::library_allowed(&e.code).unwrap_or(codes::COMPUTE_FAILED);
            HostError::new("plugin_error", format!("{code}: {}", codes::public_message(code)))
        }
        Err(_) => HostError::new("plugin_error", "compute_failed: malformed plugin error"),
    }
}
