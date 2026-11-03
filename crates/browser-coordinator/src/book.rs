//! Order-book (OP_BOOK) planning and result admission, owned by the existing
//! coordinator. JS adapters only move frames/Workers/DOM and apply the
//! returned actions/views; every admission decision is made here.
//!
//! Policy (bounded lag, monotonic, truthful):
//! - At most one compute in flight per plugin. A frame arriving while busy
//!   marks `pending`; on completion the LATEST frame is planned next, so output
//!   keeps flowing when the feed is faster than compute (no exact-latest-only
//!   admission that would starve).
//! - Each compute is bound to the frame it was planned from (`basis`,
//!   generation, seq, receipt time). A completion (success OR error) is fenced
//!   if its token is not the plugin's in-flight token, the plugin identity was
//!   removed/replaced, or the basis changed (source switch, reconnect,
//!   non-live/resync withdrawal).
//! - Accepted (generation, seq) never regresses within a basis.
//! - Displayed age is the receipt age of the frame the accepted result was
//!   computed from; newer raw frames never refresh it. Beyond
//!   `MAX_RESULT_AGE_MS` the view is `stale`; a newer frame awaiting compute
//!   shows `updating`.
use serde_json::{json, Value};
use std::collections::BTreeMap;

use cvd_indicator_sdk::codes;

use crate::{invalid, CoordError};

/// Equal to the client raw-feed stale bound (BOOK_STALE_MS): a quiet but
/// valid book is not withdrawn earlier than the feed itself would be.
pub const MAX_RESULT_AGE_MS: i64 = 5_000;
/// Errors after which the Worker is gone (load again to recover).
const RECOVERABLE: [&str; 3] = ["plugin_error", "invalid_input", "busy"];

/// Fixed, safe, actionable hint for a plugin's own public classification.
/// Only SDK library-allowlisted codes are kept; arbitrary plugin text never is.
fn hint(public: Option<&str>, code: &str) -> String {
    match public {
        Some(codes::INVALID_PARAMS) => "parameters rejected — edit parameters".into(),
        Some(codes::LIMIT_EXCEEDED) => "limit exceeded — edit parameters".into(),
        Some(p) => format!("{p}: {}", codes::public_message(p)),
        None => code.to_string(),
    }
}

#[derive(Debug, Clone)]
struct Fault {
    /// Outer transport/host classification (decides recoverability).
    code: String,
    /// Plugin's own allowlisted public classification, when reported.
    public: Option<&'static str>,
    /// Basis the fault was observed on (recoverable faults are bound to it).
    basis: u64,
    fatal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Meta {
    basis: u64,
    generation: u64,
    seq: u64,
    received_ms: i64,
}

#[derive(Debug, Default)]
struct Plugin {
    load_key: String,
    ready: bool,
    in_flight: Option<(u64, Meta)>,
    pending: bool,
    accepted: Option<(u64, Meta)>,
    fault: Option<Fault>,
}

#[derive(Debug, Default)]
pub struct BookSched {
    basis: u64,
    source: String,
    latest: Option<Meta>,
    plugins: BTreeMap<String, Plugin>,
    next_token: u64,
}

fn s<'a>(v: &'a Value, k: &str) -> Result<&'a str, CoordError> {
    v.get(k).and_then(Value::as_str).ok_or_else(|| invalid(format!("{k} not a string")))
}
fn u(v: &Value, k: &str) -> Result<u64, CoordError> {
    v.get(k).and_then(Value::as_u64).ok_or_else(|| invalid(format!("{k} not an integer")))
}
fn i(v: &Value, k: &str) -> Result<i64, CoordError> {
    v.get(k).and_then(Value::as_i64).ok_or_else(|| invalid(format!("{k} not an integer")))
}

impl BookSched {
    fn plan(&mut self, id: &str) -> Option<Value> {
        let latest = self.latest?;
        let p = self.plugins.get_mut(id)?;
        // Non-recoverable faults clear `ready`; recoverable ones (e.g. invalid
        // params for this book) retry on newer frames and stay visible until
        // a success.
        if !p.ready {
            return None;
        }
        if p.in_flight.is_some() {
            p.pending = true;
            return None;
        }
        // Nothing newer than what is already accepted: no work.
        if p.accepted.is_some_and(|(_, a)| a.basis == latest.basis && (a.generation, a.seq) >= (latest.generation, latest.seq)) {
            p.pending = false;
            return None;
        }
        self.next_token += 1;
        let t = self.next_token;
        p.in_flight = Some((t, latest));
        p.pending = false;
        Some(json!({ "plugin": id, "token": t, "generation": latest.generation, "seq": latest.seq }))
    }

    fn withdraw(&mut self) {
        self.basis += 1;
        self.latest = None;
        for p in self.plugins.values_mut() {
            p.accepted = None;
            p.pending = false;
            // A recoverable fault belongs to the withdrawn basis; fatal
            // (Worker gone / load failed) faults stay visible.
            if p.fault.as_ref().is_some_and(|f| !f.fatal) {
                p.fault = None;
            }
        }
    }

    fn view(&self, now_ms: i64) -> Value {
        let list: Vec<Value> = self
            .plugins
            .iter()
            .map(|(id, p)| {
                let (state, token, meta) = match (&p.fault, p.accepted) {
                    (Some(_), _) => ("fault", None, None),
                    (None, None) => (if p.ready { "waiting" } else { "starting" }, None, None),
                    (None, Some((t, m))) => {
                        let age = now_ms - m.received_ms;
                        let newer = self.latest.is_some_and(|l| (l.generation, l.seq) > (m.generation, m.seq));
                        (if age > MAX_RESULT_AGE_MS { "stale" } else if newer { "updating" } else { "live" }, Some(t), Some((m, age)))
                    }
                };
                json!({
                    "plugin": id, "state": state, "token": token,
                    "fault": p.fault.as_ref().map(|f| &f.code),
                    "public_code": p.fault.as_ref().and_then(|f| f.public),
                    "fatal": p.fault.as_ref().map(|f| f.fatal),
                    "fault_basis": p.fault.as_ref().map(|f| f.basis),
                    "hint": p.fault.as_ref().map(|f| hint(f.public, &f.code)),
                    "generation": meta.map(|(m, _)| m.generation), "seq": meta.map(|(m, _)| m.seq),
                    "age_ms": meta.map(|(_, a)| a),
                })
            })
            .collect();
        json!({ "basis": self.basis, "plugins": list })
    }

    pub fn dispatch(&mut self, cmd: &str, v: &Value) -> Result<Value, CoordError> {
        match cmd {
            // Persistent plugin set: removed or replaced (load_key) identities
            // are dropped, which fences their in-flight completions.
            "book_plugins" => {
                let list = v.get("list").and_then(Value::as_array).ok_or_else(|| invalid("list not an array"))?;
                let mut want = BTreeMap::new();
                for e in list {
                    want.insert(s(e, "id")?.to_string(), e.get("load_key").and_then(Value::as_str).unwrap_or("").to_string());
                }
                let mut started = Vec::new();
                self.plugins.retain(|id, p| want.get(id) == Some(&p.load_key));
                for (id, key) in want {
                    if !self.plugins.contains_key(&id) {
                        self.plugins.insert(id.clone(), Plugin { load_key: key, ..Default::default() });
                        started.push(id);
                    }
                }
                Ok(json!({ "start": started }))
            }
            // Worker (re)loaded for this identity; `ok=false` records a load fault.
            "book_loaded" => {
                let id = s(v, "plugin")?;
                let Some(p) = self.plugins.get_mut(id) else { return Ok(json!({ "admit": false })) };
                if v.get("load_key").and_then(Value::as_str).is_some_and(|k| k != p.load_key) {
                    return Ok(json!({ "admit": false }));
                }
                p.in_flight = None;
                p.pending = false;
                if v.get("ok").and_then(Value::as_bool) == Some(false) {
                    p.ready = false;
                    let code = v.get("error").and_then(Value::as_str).unwrap_or("load_failed").to_string();
                    let public = v.get("public_code").and_then(Value::as_str).and_then(codes::library_allowed);
                    p.fault = Some(Fault { code, public, basis: self.basis, fatal: true });
                    return Ok(json!({ "admit": true, "actions": [] }));
                }
                p.ready = true;
                p.fault = None;
                let id = id.to_string();
                Ok(json!({ "admit": true, "actions": self.plan(&id).into_iter().collect::<Vec<_>>() }))
            }
            // Source switch / reconnect / non-live frame: new basis, nothing old shown.
            "book_source" => {
                let src = s(v, "source")?.to_string();
                self.withdraw();
                self.source = src;
                Ok(json!({ "basis": self.basis }))
            }
            "book_withdraw" => {
                self.withdraw();
                Ok(json!({ "basis": self.basis }))
            }
            "book_frame" => {
                if s(v, "source")? != self.source {
                    return Ok(json!({ "admit": false, "actions": [] }));
                }
                let m = Meta { basis: self.basis, generation: u(v, "generation")?, seq: u(v, "seq")?, received_ms: i(v, "received_ms")? };
                if self.latest.is_some_and(|l| (m.generation, m.seq) <= (l.generation, l.seq)) {
                    return Ok(json!({ "admit": false, "actions": [] }));
                }
                if self.latest.is_some_and(|l| m.generation != l.generation) {
                    // New exchange generation (resync): old results withdrawn.
                    for p in self.plugins.values_mut() {
                        p.accepted = None;
                        if p.fault.as_ref().is_some_and(|f| !f.fatal) {
                            p.fault = None;
                        }
                    }
                }
                self.latest = Some(m);
                let ids: Vec<String> = self.plugins.keys().cloned().collect();
                let actions: Vec<Value> = ids.iter().filter_map(|id| self.plan(id)).collect();
                Ok(json!({ "admit": true, "actions": actions }))
            }
            "book_done" => {
                let id = s(v, "plugin")?;
                let token = u(v, "token")?;
                let err = v.get("error").and_then(Value::as_str);
                // Only an SDK library-allowlisted classification is kept.
                let public = v.get("public_code").and_then(Value::as_str).and_then(codes::library_allowed);
                let basis = self.basis;
                let Some(p) = self.plugins.get_mut(id) else { return Ok(json!({ "admit": false, "reason": "removed", "actions": [] })) };
                let Some((t, m)) = p.in_flight.filter(|(t, _)| *t == token) else {
                    return Ok(json!({ "admit": false, "reason": "token", "actions": [] }));
                };
                let _ = t;
                p.in_flight = None;
                let current = m.basis == basis && self.latest.is_some_and(|l| l.generation == m.generation);
                let admit = match err {
                    _ if !current => false,
                    Some("superseded") => false,
                    Some(code) => {
                        p.accepted = None;
                        let fatal = !RECOVERABLE.contains(&code);
                        p.fault = Some(Fault { code: code.to_string(), public, basis, fatal });
                        if fatal {
                            p.ready = false;
                        }
                        true
                    }
                    None => {
                        if p.accepted.is_some_and(|(_, a)| a.basis == m.basis && (a.generation, a.seq) >= (m.generation, m.seq)) {
                            false
                        } else {
                            p.accepted = Some((token, m));
                            p.fault = None;
                            true
                        }
                    }
                };
                let id = id.to_string();
                // Pending newer frame: plan it now (also after a recoverable error).
                let next = if self.plugins.get(&id).is_some_and(|p| p.pending) { self.plan(&id) } else { None };
                Ok(json!({ "admit": admit, "reason": if admit { "ok" } else { "fenced" }, "actions": next.into_iter().collect::<Vec<_>>() }))
            }
            "book_view" => Ok(self.view(i(v, "now_ms")?)),
            _ => Err(CoordError { code: "bad_command", message: "unknown cmd".into() }),
        }
    }
}

#[cfg(test)]
#[path = "book_tests.rs"]
mod tests;
