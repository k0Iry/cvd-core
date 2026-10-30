//! Main-thread response fencing. Every asynchronous worker response must
//! match an outstanding request issued for the instance's *current*
//! generation, scope and data revision; anything else is dropped.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Fence scope used for the load request (op 0, revision 0).
pub const LOAD_SCOPE: &str = "__load";
/// Fence scope used for a worker-side module validation request (op 0).
pub const VALIDATE_SCOPE: &str = "__validate";
/// Host quota: outstanding requests across all instances of one controller.
pub const MAX_PENDING: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub instance: u64,
    pub generation: u64,
    pub scope_key: String,
    pub revision: u64,
    pub op: u32,
}

/// A response as reported by the bridge (fields copied from the worker message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub instance: u64,
    pub generation: u64,
    /// Worker message type: `loaded` | `result` | `error`.
    pub kind: String,
    /// Required (no `null`/absent bypass): load echoes [`LOAD_SCOPE`]/0.
    pub scope_key: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Accepted { op: u32 },
    Rejected { reason: &'static str },
}

#[derive(Debug, Default)]
pub struct Controller {
    next_id: u64,
    next_generation: u64,
    /// instance -> current live generation (absent = retired / never started)
    live: BTreeMap<u64, u64>,
    pending: BTreeMap<u64, Pending>,
    pub rejected: u64,
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start (or restart) an instance; returns its fresh generation. Any
    /// outstanding request of an earlier generation becomes stale.
    pub fn start(&mut self, instance: u64) -> u64 {
        self.retire(instance);
        self.next_generation += 1;
        self.live.insert(instance, self.next_generation);
        self.next_generation
    }

    /// Disable/terminate: drop pending requests; late results are rejected.
    /// Returns the cancelled request ids (the bridge settles their waiters).
    pub fn retire(&mut self, instance: u64) -> Vec<u64> {
        self.live.remove(&instance);
        self.drop_where(|p| p.instance == instance)
    }

    fn drop_where(&mut self, f: impl Fn(&Pending) -> bool) -> Vec<u64> {
        let ids: Vec<u64> = self.pending.iter().filter(|(_, p)| f(p)).map(|(id, _)| *id).collect();
        for id in &ids {
            self.pending.remove(id);
        }
        ids
    }

    /// Cancel one outstanding request (bridge-side timeout); true if it was pending.
    pub fn cancel(&mut self, id: u64) -> bool {
        self.pending.remove(&id).is_some()
    }

    pub fn generation(&self, instance: u64) -> Option<u64> {
        self.live.get(&instance).copied()
    }

    pub fn issue(&mut self, instance: u64, scope_key: &str, revision: u64, op: u32) -> Option<u64> {
        let generation = self.generation(instance)?;
        if self.pending.len() >= MAX_PENDING {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.pending.insert(id, Pending { instance, generation, scope_key: scope_key.to_string(), revision, op });
        Some(id)
    }

    /// Scope/revision changed: outstanding requests for other scope/revision
    /// of this instance are cancelled.
    pub fn supersede(&mut self, instance: u64, scope_key: &str, revision: u64) -> Vec<u64> {
        self.drop_where(|p| {
            p.instance == instance && p.op != 0 && !(p.scope_key == scope_key && p.revision == revision)
        })
    }

    /// Number of started-but-not-retired incarnations.
    pub fn live_len(&self) -> usize {
        self.live.len()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn accept(&mut self, r: &Response) -> Verdict {
        let v = self.check(r);
        if matches!(v, Verdict::Rejected { .. }) {
            self.rejected += 1;
        }
        v
    }

    fn check(&mut self, r: &Response) -> Verdict {
        let Some(p) = self.pending.get(&r.id) else { return Verdict::Rejected { reason: "unsolicited" } };
        if p.instance != r.instance || p.generation != r.generation {
            return Verdict::Rejected { reason: "mismatched_identity" };
        }
        if r.scope_key != p.scope_key || r.revision != p.revision {
            return Verdict::Rejected { reason: "mismatched_scope" };
        }
        let kind_ok = match r.kind.as_str() {
            "error" => true,
            "loaded" => p.op == 0 && p.scope_key == LOAD_SCOPE,
            "validated" => p.op == 0 && p.scope_key == VALIDATE_SCOPE,
            "result" => p.op != 0,
            _ => false,
        };
        if !kind_ok {
            return Verdict::Rejected { reason: "unexpected_kind" };
        }
        let p = self.pending.remove(&r.id).expect("present");
        if self.generation(p.instance) != Some(p.generation) {
            return Verdict::Rejected { reason: "stale" };
        }
        Verdict::Accepted { op: p.op }
    }
}
