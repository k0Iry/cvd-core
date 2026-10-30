//! Rust-owned local-plugin scheduling and result authority.
//!
//! The browser JS is a thin async I/O driver: it asks `next(plugin)` for the
//! next unit of work, performs exactly that Worker call (load / compute), and
//! reports the outcome with `done(plugin, token, ...)`. Every decision lives
//! here: readiness / up-to-date, history-vs-live, bounded older user pages,
//! aggregate warm-up coverage, invalid-window fail-closed, bounded live
//! reinit -> `stalled`, reload vs fault, retire/epoch fencing of BOTH success
//! and failure, and result admission (scope / history-basis fence plus the
//! stale-presentation boundary `stale_from` for an older data revision).
//!
//! The canonical bar cache is mirrored once per history basis (`bars`) and
//! patched with a bounded tail on ordinary live ticks, so no full-history
//! serialization happens per tick. A tail that cannot be applied exactly
//! (gap, length mismatch) returns `need_full` and the driver resends bars.
//!
//! Accepted values are owned here too: a successful compute's output frame
//! crosses ONCE per completion (raw JSON appended to `done`, parsed once) and
//! is merged into `vals` (history/live precedence, bounded partial-range
//! preservation, changed times, invalidation/withdraw/drop). Every change is
//! returned as a batched `view` diff; the JS projection only applies it.

use super::*;
use serde_json::json;
use std::collections::{BTreeSet, VecDeque};

const RELOAD_CODES: [&str; 5] = ["superseded", "stale", "replaced", "disabled", "terminated"];
const NONFATAL_LIVE: [&str; 3] = ["sequence_gap", "stale_revision", "invalid_input"];
/// Attempts per (scope, data revision) before a live basis is `stalled`.
const MAX_LIVE_RETRIES: u32 = 2;
/// Bars kept per compute token for the stale-presentation diff.
const SNAP: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct OlderRange { pub from_index: usize, pub to_index: usize, pub from: i64, pub to: i64 }

#[derive(Debug, Clone)]
pub struct Demand {
    pub scope: Scope,
    pub scope_key: String,
    pub revision: u64,
    pub history_revision: u64,
    pub exhausted: bool,
    pub want_from_index: usize,
    pub want_to_index: Option<usize>,
    pub older: Vec<OlderRange>,
    pub invalid_times: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq)]
struct Hist { scope_key: String, history_revision: u64, keep_time: Option<i64>, to_time: Option<i64>, blocked: bool }

#[derive(Debug, Clone)]
enum Kind {
    Load,
    History { keep_time: Option<i64>, plan: Value, coverage: String, need_before: usize, range: (Option<i64>, Option<i64>) },
    Older { key: String, plan: Value, coverage: String, need_before: usize, range: (Option<i64>, Option<i64>) },
    Live { op: u32, touched: Vec<i64> },
}

#[derive(Debug, Clone)]
struct Token { id: u64, epoch: u64, scope_key: String, revision: u64, history_revision: u64, mode: String, snap: Vec<Value>, kind: Kind }

#[derive(Debug, Default)]
struct Rt {
    instance: u64,
    epoch: u64,
    load_key: String,
    retry: bool,
    loaded: bool,
    live: bool,
    ctx_bars: usize,
    ctx_per_tf: BTreeMap<String, usize>,
    hist: Option<Hist>,
    live_basis: Option<(String, u64)>,
    older_done: BTreeSet<String>,
    older_cov: BTreeMap<String, (String, usize)>,
    tail_cov: Option<(String, usize)>,
    coverage: Option<String>,
    state: String,
    code: String,
    fault: Option<String>,
    live_reject: Option<(String, u32)>,
    reinits: u64,
    ops: [u64; 2],
    plans: VecDeque<Value>,
    last_plan: Option<Value>,
    inflight: Option<Token>,
}

/// One accepted output column: History/merged values plus the persistent
/// live-session overlay (authoritative only on the history basis it was built on).
#[derive(Debug, Default)]
struct Col { v: BTreeMap<i64, Value>, ov: Option<(u64, BTreeMap<i64, Value>)> }
/// Live overlay bound (oldest times evicted first).
const OVERLAY_CAP: usize = 1024;

/// Batched view diff for the JS read-only projection.
#[derive(Debug, Default)]
pub struct View { clear: bool, drop: Vec<String>, cols: BTreeMap<String, (Vec<(i64, Value)>, Vec<i64>)>, changed: BTreeSet<i64>, shape: bool }
impl View {
    fn set(&mut self, k: &str, t: i64, v: Value) { self.cols.entry(k.to_string()).or_default().0.push((t, v)) }
    fn del(&mut self, k: &str, t: i64) { self.cols.entry(k.to_string()).or_default().1.push(t) }
    pub fn json(&self) -> Value {
        let cols: serde_json::Map<String, Value> = self.cols.iter().map(|(k, (s, d))|
            (k.clone(), json!({ "set": s.iter().map(|(t, v)| json!([t, v])).collect::<Vec<_>>(), "del": d }))).collect();
        json!({ "clear": self.clear, "drop": self.drop, "cols": cols, "changed": self.changed, "shape": self.shape })
    }
}

/// Value of one output series at `i` (JS `seriesValueAt` semantics):
/// `None` = undefined (absent), `Null` = explicit gap, finite numbers kept.
pub fn series_value_at(s: &Value, i: usize) -> Option<Value> {
    let num = |v: Option<&Value>| match v { Some(n @ Value::Number(_)) => n.clone(), _ => Value::Null };
    match s.get("type").and_then(Value::as_str) {
        Some("line") | Some("histogram") => Some(num(s.get("values").and_then(|a| a.get(i)))),
        Some("dual_range") => {
            let (u, l) = (num(s.get("upper").and_then(|a| a.get(i))), num(s.get("lower").and_then(|a| a.get(i))));
            Some(if u.is_null() || l.is_null() { Value::Null } else { json!({ "upper": u, "lower": l }) })
        }
        Some("segmented_lines") => Some(match s.get("layers") {
            Some(Value::Array(ls)) => Value::Array(ls.iter().map(|l| num(l.get(i))).collect()),
            _ => Value::Null,
        }),
        _ => None,
    }
}

#[derive(Default)]
pub struct Scheduler {
    vals: BTreeMap<String, Col>,
    runtimes: BTreeMap<String, Rt>,
    next_instance: u64,
    next_token: u64,
    demand: Option<Demand>,
    bars: Vec<Value>,
}

fn older_key(d: &Demand, r: &OlderRange) -> String {
    format!("{}|{}|{}|{}|{}", d.scope_key, d.history_revision, r.from, r.to, d.exhausted as u8)
}
fn rt_mut<'a>(c: &'a mut Coordinator, id: &str) -> &'a mut Rt { c.sched.runtimes.get_mut(id).unwrap() }
fn cov_str(c: Coverage) -> String { serde_json::to_value(c).unwrap().as_str().unwrap().to_string() }

/// Earliest bar time whose canonical input differs between a snapshot of an
/// older demand's newest bars and the current cache (appends, replacements,
/// removals); `None` = identical (nothing stale).
pub fn changed_from_time(old: &[Value], bars: &[Value]) -> Option<i64> {
    let t = |v: &Value| v.get("time").and_then(Value::as_i64).unwrap_or(i64::MIN);
    let (mut i, mut j) = (old.len() as isize - 1, bars.len() as isize - 1);
    let mut from: Option<i64> = None;
    let min = |x: i64, f: &mut Option<i64>| *f = Some(f.map_or(x, |y| y.min(x)));
    while i >= 0 && j >= 0 {
        let (a, b) = (&old[i as usize], &bars[j as usize]);
        let (ta, tb) = (t(a), t(b));
        if ta > tb { min(ta, &mut from); i -= 1; continue }
        if tb > ta { min(tb, &mut from); j -= 1; continue }
        if a != b { from = Some(ta) }
        i -= 1; j -= 1;
    }
    from
}

impl Rt {
    fn ctx(&self, tf: &str) -> usize { self.ctx_per_tf.get(tf).copied().unwrap_or(self.ctx_bars) }
    fn status(&mut self, ev: &mut Vec<Value>, id: &str, state: &str, code: &str, extra: Value) {
        self.state = state.into();
        self.code = code.into();
        let mut e = json!({ "ev": "status", "id": id, "state": state, "code": code });
        if let Value::Object(m) = extra { for (k, v) in m { e[k] = v } }
        ev.push(e);
    }
    fn aggregate(&mut self) -> (String, usize) {
        let (mut c, mut nb) = self.tail_cov.clone().unwrap_or(("ready".into(), 0));
        for (oc, onb) in self.older_cov.values() {
            if oc != "ready" {
                if c == "ready" || oc == "unavailable" { c = oc.clone() }
                nb = nb.max(*onb);
            }
        }
        if c == "ready" { nb = 0 }
        self.coverage = Some(c.clone());
        (c, nb)
    }
    fn prune_older(&mut self, d: &Demand) {
        let keep: BTreeSet<String> = d.older.iter().map(|r| older_key(d, r)).collect();
        self.older_done.retain(|k| keep.contains(k));
        self.older_cov.retain(|k, _| keep.contains(k));
    }
    fn ready_label(&self) -> String {
        match self.coverage.as_deref() { None | Some("ready") => "ready".into(), Some(c) => c.into() }
    }
    fn push_plan(&mut self, kind: &str, plan: &Value) {
        let mut p = plan.clone();
        p["kind"] = json!(kind);
        if self.plans.len() >= 16 { self.plans.pop_front(); }
        self.plans.push_back(p);
        if kind == "tail" { self.last_plan = Some(plan.clone()) }
    }
}

impl Coordinator {
    fn sched_retire(&mut self, id: &str, ev: &mut Vec<Value>) {
        let s = &mut self.sched;
        if let Some(rt) = s.runtimes.get_mut(id) {
            rt.epoch += 1;
            ev.push(json!({ "ev": "disable", "id": id, "inst": rt.instance }));
            rt.loaded = false;
            rt.hist = None;
            rt.live_basis = None;
            rt.older_done.clear();
            rt.older_cov.clear();
            rt.tail_cov = None;
            rt.inflight = None;
        }
        self.reset(id);
    }

    /// Membership: `[{id, load_key}]` of enabled plugins (with optional
    /// `retry`). Returns lifecycle events (`disable`, `off`).
    pub fn sched_plugins(&mut self, list: &[(String, String)]) -> Vec<Value> {
        let mut ev = Vec::new();
        let wanted: BTreeSet<&String> = list.iter().map(|(i, _)| i).collect();
        let gone: Vec<String> = self.sched.runtimes.keys().filter(|k| !wanted.contains(k)).cloned().collect();
        for id in gone {
            self.sched_retire(&id, &mut ev);
            self.drop_values(&id, &mut ev);
            self.sched.runtimes.remove(&id);
            ev.push(json!({ "ev": "status", "id": id, "state": "off", "code": "" }));
        }
        for (id, key) in list {
            let replace = self.sched.runtimes.get(id).is_some_and(|rt| &rt.load_key != key || rt.retry);
            if replace {
                self.sched_retire(id, &mut ev);
                let rt = self.sched.runtimes.get_mut(id).unwrap();
                rt.fault = None;
                rt.retry = false;
            }
            if !self.sched.runtimes.contains_key(id) {
                self.sched.next_instance += 1;
                let inst = self.sched.next_instance;
                self.sched.runtimes.insert(id.clone(), Rt { instance: inst, state: "starting".into(), ..Default::default() });
            }
            self.sched.runtimes.get_mut(id).unwrap().load_key = key.clone();
        }
        ev
    }

    /// Retry intent after a fault; the following membership call reloads.
    pub fn sched_clear_fault(&mut self, id: &str) {
        if let Some(rt) = self.sched.runtimes.get_mut(id) { rt.retry = true }
    }

    /// Apply a new demand. `bars` = full canonical cache (history basis change)
    /// or `tail` + `len` (ordinary tick, patched into the mirror). Returns
    /// `need_full` when the tail cannot be applied exactly (demand NOT applied),
    /// and the loaded plugins to physically supersede on a scope change.
    pub fn sched_demand(&mut self, d: Demand, bars: Option<Vec<Value>>, tail: Option<(Vec<Value>, usize)>) -> Value {
        let t = |v: &Value| v.get("time").and_then(Value::as_i64);
        let scope_changed = self.sched.demand.as_ref().is_some_and(|o| o.scope_key != d.scope_key);
        match (bars, tail) {
            (Some(b), _) => self.sched.bars = b,
            (None, Some((tail, len))) => {
                if scope_changed || self.sched.demand.is_none() { return json!({ "need_full": true }) }
                let mut m = std::mem::take(&mut self.sched.bars);
                if let Some(t0) = tail.first().and_then(t) {
                    // Contiguous: the tail must start at/before mirror end + one bar.
                    let cut = m.partition_point(|b| t(b).unwrap_or(i64::MIN) < t0);
                    if cut == m.len() && !m.is_empty() && cut + tail.len() != len {
                        self.sched.bars = m;
                        return json!({ "need_full": true });
                    }
                    m.truncate(cut);
                    m.extend(tail);
                }
                if m.len() != len {
                    self.sched.bars = m;
                    return json!({ "need_full": true });
                }
                self.sched.bars = m;
            }
            (None, None) => return json!({ "need_full": true }),
        }
        let mut supersede = Vec::new();
        if scope_changed {
            let ids: Vec<String> = self.sched.runtimes.keys().cloned().collect();
            for id in ids {
                self.reset(&id);
                let rt = self.sched.runtimes.get_mut(&id).unwrap();
                rt.hist = None;
                rt.live_basis = None;
                if rt.loaded { supersede.push(json!({ "id": id, "inst": rt.instance })) }
            }
        }
        let mut view = View::default();
        if scope_changed && !self.sched.vals.is_empty() { self.sched.vals.clear(); view.clear = true }
        self.sched.demand = Some(d);
        json!({ "need_full": false, "supersede": supersede, "view": view.json() })
    }

    /// The driver's physical supersede cancelled an in-flight compute (or failed).
    pub fn sched_superseded(&mut self, id: &str) {
        if let Some(rt) = self.sched.runtimes.get_mut(id) { rt.epoch += 1; rt.loaded = false; rt.inflight = None }
    }

    fn keep_time(&self, d: &Demand) -> Option<i64> {
        let n = self.sched.bars.len();
        self.sched.bars.get(d.want_from_index.min(n)).and_then(|b| b.get("time")).and_then(Value::as_i64)
    }

    fn pending_older(&self, rt: &Rt, d: &Demand) -> Option<OlderRange> {
        d.older.iter().find(|r| !rt.older_done.contains(&older_key(d, r))).cloned()
    }

    fn up_to_date(&self, rt: &Rt) -> bool {
        let Some(d) = &self.sched.demand else { return true };
        if self.sched.bars.is_empty() { return true }
        let hrev = if rt.live { d.history_revision } else { d.revision };
        let hist_ok = rt.hist.as_ref().is_some_and(|h| h.scope_key == d.scope_key && h.history_revision == hrev && h.keep_time == self.keep_time(d));
        let live_ok = !rt.live || rt.live_basis.as_ref().is_some_and(|(k, r)| *k == d.scope_key && *r == d.revision);
        hist_ok && live_ok && self.pending_older(rt, d).is_none()
    }

    pub fn sched_pending(&self, id: &str) -> bool {
        self.sched.runtimes.get(id).is_some_and(|rt| rt.fault.is_none() && !self.up_to_date(rt))
    }
    pub fn sched_idle(&self) -> bool {
        self.sched.runtimes.values().all(|rt| rt.fault.is_some() || (rt.inflight.is_none() && self.up_to_date(rt)))
    }

    fn token(&mut self, id: &str, kind: Kind) -> Token {
        let d = self.sched.demand.clone().expect("demand");
        self.sched.next_token += 1;
        let n = self.sched.bars.len();
        let tok = Token {
            id: self.sched.next_token,
            epoch: self.sched.runtimes[id].epoch,
            scope_key: d.scope_key.clone(),
            revision: d.revision,
            history_revision: d.history_revision,
            mode: serde_json::to_value(&d.scope.mode).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            snap: self.sched.bars[n.saturating_sub(SNAP)..].to_vec(),
            kind,
        };
        self.sched.runtimes.get_mut(id).unwrap().inflight = Some(tok.clone());
        tok
    }

    fn want_json(d: &Demand) -> Value {
        json!({ "scope_key": d.scope_key, "revision": d.revision, "history_revision": d.history_revision, "mode": d.scope.mode })
    }

    /// Next unit of work for one plugin:
    /// `{act:"idle"|"again"|"load"|"compute", events, ...}`.
    /// A planning failure (e.g. a malformed canonical bar) faults the plugin:
    /// stop/release, no autorestart, no fallback.
    pub fn sched_next(&mut self, id: &str) -> Value {
        match self.sched_next_inner(id) {
            Ok(v) => v,
            Err(e) => {
                let mut ev = Vec::new();
                if self.sched.runtimes.contains_key(id) { self.fault(id, e.code, &mut ev) }
                json!({ "act": "idle", "events": ev })
            }
        }
    }

    fn sched_next_inner(&mut self, id: &str) -> Result<Value, CoordError> {
        let mut ev = Vec::new();
        let Some(rt) = self.sched.runtimes.get(id) else { return Ok(json!({ "act": "idle", "events": ev })) };
        if rt.fault.is_some() || self.up_to_date(rt) { return Ok(json!({ "act": "idle", "events": ev })) }
        if rt.inflight.is_some() { return Ok(json!({ "act": "idle", "events": ev })) }
        let d = self.sched.demand.clone().expect("demand when not up to date");
        if !rt.loaded {
            let inst = rt.instance;
            self.sched.runtimes.get_mut(id).unwrap().status(&mut ev, id, "starting", "", json!({}));
            let tok = self.token(id, Kind::Load);
            return Ok(json!({ "act": "load", "inst": inst, "token": tok.id, "events": ev }));
        }
        let live = rt.live;
        let ctx = rt.ctx(&d.scope.tf);
        let inst = rt.instance;
        let n = self.sched.bars.len();
        let hrev = if live { d.history_revision } else { d.revision };
        let from_idx = d.want_from_index.min(n);
        let to_idx = d.want_to_index.unwrap_or(n).min(n).max(from_idx);
        let keep_time = self.keep_time(&d);
        let want_to_time = if to_idx > 0 { self.sched.bars.get(to_idx - 1).and_then(|b| b.get("time")).and_then(Value::as_i64) } else { None };
        let covered = rt.hist.as_ref().is_some_and(|h| h.scope_key == d.scope_key && h.history_revision == hrev && h.keep_time == keep_time
            && (live || match (h.to_time, want_to_time) { (_, None) => true, (None, Some(_)) => false, (Some(a), Some(b)) => a >= b }));
        let want = Self::want_json(&d);
        if !covered {
            match plan_history_checked(&d.scope, d.revision, &self.sched.bars, ctx, from_idx, to_idx, d.exhausted, &d.invalid_times) {
                Err(e) if e.code == "invalid_window" => {
                    self.reset(id);
                    let rt = self.sched.runtimes.get_mut(id).unwrap();
                    rt.hist = Some(Hist { scope_key: d.scope_key.clone(), history_revision: hrev, keep_time, to_time: want_to_time, blocked: true });
                    rt.tail_cov = Some(("unavailable".into(), 0));
                    rt.live_basis = None;
                    rt.prune_older(&d);
                    rt.aggregate();
                    let view = self.withdraw(id, &d, None, None);
                    let rt = self.sched.runtimes.get_mut(id).unwrap();
                    ev.push(json!({ "ev": "invalid", "id": id, "want": want, "from": null, "to": null, "reason": e.message, "view": view }));
                    rt.status(&mut ev, id, "unavailable", "invalid_input_window", json!({ "coverage": "unavailable" }));
                    return Ok(json!({ "act": if live { "again" } else { "idle" }, "events": ev }));
                }
                Err(e) => return Err(e),
                Ok(plan) => {
                    let cov = cov_str(plan.coverage);
                    let meta = json!({ "trimmed_from": plan.trimmed_from, "input_to": plan.input_to, "output_from": plan.output_from,
                        "range_from_time": plan.range_from_time, "range_to_time": plan.range_to_time, "coverage": cov });
                    self.sched.runtimes.get_mut(id).unwrap().status(&mut ev, id, "computing", "", json!({ "coverage": cov }));
                    let tok = self.token(id, Kind::History { keep_time, plan: meta, coverage: cov, need_before: plan.need_before, range: (plan.range_from_time, plan.range_to_time) });
                    return Ok(json!({ "act": "compute", "inst": inst, "token": tok.id, "op": OP_HISTORY, "body": plan.body, "scope_key": d.scope_key, "revision": d.revision, "events": ev }));
                }
            }
        }
        self.sched.runtimes.get_mut(id).unwrap().prune_older(&d);
        if let Some(older) = self.pending_older(&self.sched.runtimes[id], &d) {
            let key = older_key(&d, &older);
            match plan_history_checked(&d.scope, d.revision, &self.sched.bars, ctx, older.from_index, older.to_index, d.exhausted, &d.invalid_times) {
                Err(e) if e.code == "invalid_window" => {
                    let view = self.withdraw(id, &d, Some(older.from), Some(older.to));
                    let rt = self.sched.runtimes.get_mut(id).unwrap();
                    rt.older_done.insert(key.clone());
                    rt.older_cov.insert(key, ("unavailable".into(), 0));
                    rt.aggregate();
                    ev.push(json!({ "ev": "invalid", "id": id, "want": want, "from": older.from, "to": older.to, "reason": e.message, "view": view }));
                    return Ok(json!({ "act": "again", "events": ev }));
                }
                Err(e) => return Err(e),
                Ok(plan) => {
                    let cov = cov_str(plan.coverage);
                    let meta = json!({ "trimmed_from": plan.trimmed_from, "input_to": plan.input_to, "output_from": plan.output_from,
                        "range_from_time": plan.range_from_time, "range_to_time": plan.range_to_time, "coverage": cov });
                    let tok = self.token(id, Kind::Older { key, plan: meta, coverage: cov, need_before: plan.need_before, range: (plan.range_from_time, plan.range_to_time) });
                    return Ok(json!({ "act": "compute", "inst": inst, "token": tok.id, "op": OP_HISTORY, "body": plan.body, "scope_key": d.scope_key, "revision": d.revision, "events": ev }));
                }
            }
        }
        // Live tick: LiveInit or bounded LiveUpdate from the mirrored tail.
        let cap = ctx + 1 + LIVE_REPLACE_SLACK;
        let tail = self.sched.bars[n.saturating_sub(cap)..].to_vec();
        let plan = self.plan_live_checked(id, &d.scope_key, &d.scope, ctx, &tail, &d.invalid_times)?;
        if plan.op == 0 && plan.reason == "invalid_window" {
            let view = self.withdraw(id, &d, plan.touched.first().copied(), None);
            let rt = self.sched.runtimes.get_mut(id).unwrap();
            rt.live_basis = Some((d.scope_key.clone(), d.revision));
            ev.push(json!({ "ev": "invalid", "id": id, "want": want, "from": plan.touched.first(), "to": null, "reason": "live window spans invalid bucket", "view": view }));
            rt.status(&mut ev, id, "unavailable", "invalid_input_window", json!({ "coverage": "unavailable" }));
            return Ok(json!({ "act": "idle", "events": ev }));
        }
        let rt = self.sched.runtimes.get_mut(id).unwrap();
        if plan.op == 0 {
            rt.live_basis = Some((d.scope_key.clone(), d.revision));
            let blocked = rt.hist.as_ref().is_some_and(|h| h.blocked);
            if rt.state == "computing" || rt.state == "starting" || (rt.code == "invalid_input_window" && !blocked) {
                let l = rt.ready_label();
                let c = rt.coverage.clone();
                rt.status(&mut ev, id, &l, "", json!({ "coverage": c }));
            }
            return Ok(json!({ "act": "again", "events": ev }));
        }
        let tok = self.token(id, Kind::Live { op: plan.op, touched: plan.touched.clone() });
        Ok(json!({ "act": "compute", "inst": inst, "token": tok.id, "op": plan.op, "body": plan.body, "scope_key": d.scope_key, "revision": d.revision, "events": ev }))
    }

    /// Mode label of the current demand (`source` / `aggregate`).
    fn mode_of(d: &Demand) -> String {
        serde_json::to_value(&d.scope.mode).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }

    /// Withdraw `id`'s accepted values (History + overlay) in `[from, to]`
    /// (`None` = unbounded) for the demand's mode. Returns the view diff.
    fn withdraw(&mut self, id: &str, d: &Demand, from: Option<i64>, to: Option<i64>) -> Value {
        let (pre, suf) = (format!("{id}/"), format!("|{}", Self::mode_of(d)));
        let inside = |t: i64| from.map_or(true, |f| t >= f) && to.map_or(true, |x| t <= x);
        let mut view = View::default();
        for (k, col) in self.sched.vals.iter_mut().filter(|(k, _)| k.starts_with(&pre) && k.ends_with(&suf)) {
            for t in col.v.keys().copied().filter(|&t| inside(t)).collect::<Vec<_>>() { col.v.remove(&t); view.del(k, t) }
            if let Some((_, ov)) = &mut col.ov { ov.retain(|&t, _| !inside(t)) }
        }
        view.json()
    }

    /// Drop every accepted value of `id` (fault / membership removal).
    fn drop_values(&mut self, id: &str, ev: &mut Vec<Value>) {
        let pre = format!("{id}/");
        let keys: Vec<String> = self.sched.vals.keys().filter(|k| k.starts_with(&pre)).cloned().collect();
        if keys.is_empty() { return }
        for k in &keys { self.sched.vals.remove(k); }
        let view = View { drop: keys, ..Default::default() };
        ev.push(json!({ "ev": "view", "id": id, "view": view.json() }));
    }

    /// Explicit drop (package replace / params change): `{view}`.
    pub fn sched_drop(&mut self, id: &str) -> Value {
        let mut ev = Vec::new();
        self.drop_values(id, &mut ev);
        json!({ "view": ev.first().map(|e| e["view"].clone()) })
    }

    /// Synchronous invalidation before a canonical bar changes/is removed:
    /// the bucket itself is withdrawn from every column, and every enabled
    /// plugin conservatively withholds outputs at/after the earliest bucket
    /// (context is warm-up, not a finite influence bound).
    pub fn sched_invalidate(&mut self, times: &[i64]) -> Value {
        let mut view = View::default();
        let Some(&from) = times.iter().min() else { return view.json() };
        let enabled: Vec<String> = self.sched.runtimes.keys().map(|id| format!("{id}/")).collect();
        for (k, col) in self.sched.vals.iter_mut() {
            let all = enabled.iter().any(|p| k.starts_with(p));
            let hit = |t: i64| times.contains(&t) || (all && t >= from);
            for t in col.v.keys().copied().filter(|&t| hit(t)).collect::<Vec<_>>() { col.v.remove(&t); view.del(k, t) }
            if let Some((_, ov)) = &mut col.ov { ov.retain(|&t, _| !hit(t)) }
        }
        view.json()
    }

    /// Merge one admitted output frame (`{times, series}`) into the accepted
    /// values: history/live precedence, bounded partial-range preservation,
    /// stale cutoff and changed-time calculation. Returns the view diff.
    fn merge(&mut self, id: &str, mode: &str, live: bool, out: &Value, stale_from: Option<i64>, range: (Option<i64>, Option<i64>), hrev: u64) -> View {
        let mut view = View::default();
        let fresh = |t: i64| stale_from.map_or(true, |s| t < s);
        let times: Vec<(usize, i64)> = out.get("times").and_then(Value::as_array).map(|a| a.iter().enumerate()
            .filter_map(|(i, t)| t.as_i64().map(|t| (i, t))).collect()).unwrap_or_default();
        let empty = serde_json::Map::new();
        let series = out.get("series").and_then(Value::as_object).unwrap_or(&empty);
        let (pre, suf) = (format!("{id}/"), format!("|{mode}"));
        let mut seen = BTreeSet::new();
        for (name, s) in series {
            let k = format!("{id}/{name}|{mode}");
            seen.insert(k.clone());
            let existed = self.sched.vals.contains_key(&k);
            let col = self.sched.vals.entry(k.clone()).or_default();
            if col.ov.as_ref().is_some_and(|(b, _)| *b != hrev) { col.ov = None }
            if live && col.ov.is_none() { col.ov = Some((hrev, BTreeMap::new())) }
            if live && existed {
                // Bounded delta: only the touched times of the live column.
                let ov = &mut col.ov.as_mut().unwrap().1;
                for &(i, t) in &times {
                    if !fresh(t) { continue }
                    let v = series_value_at(s, i);
                    if !(v.is_some() && col.v.get(&t) == v.as_ref()) { view.changed.insert(t); }
                    match v {
                        None => { if col.v.remove(&t).is_some() { view.del(&k, t) } ov.remove(&t); }
                        Some(v) => { if col.v.get(&t) != Some(&v) { view.set(&k, t, v.clone()) } col.v.insert(t, v.clone()); ov.insert(t, v); }
                    }
                }
                while ov.len() > OVERLAY_CAP { ov.pop_first(); }
                continue
            }
            let mut column = BTreeMap::new();
            for &(i, t) in &times {
                if !fresh(t) { continue }
                if let Some(v) = series_value_at(s, i) {
                    if live { col.ov.as_mut().unwrap().1.insert(t, v.clone()); }
                    column.insert(t, v);
                }
            }
            // Partial-range merge: cached values outside the recomputed
            // canonical range are preserved (never blanked by a bounded page).
            if !live && existed { if let Some(rf) = range.0 {
                let rt = range.1.unwrap_or(0);
                for (t, v) in &col.v { if (*t < rf || *t > rt) && !column.contains_key(t) { column.insert(*t, v.clone()); } }
            } }
            // Stateless History never overwrites the live session's values.
            if !live { if let Some((_, ov)) = &col.ov { for (t, v) in ov { column.insert(*t, v.clone()); } } }
            if !existed { view.shape = true }
            for (t, v) in &column { if col.v.get(t) != Some(v) { view.changed.insert(*t); view.set(&k, *t, v.clone()) } }
            for t in col.v.keys() { if !column.contains_key(t) { view.changed.insert(*t); view.del(&k, *t) } }
            col.v = column;
        }
        if !live {
            let gone: Vec<String> = self.sched.vals.keys().filter(|k| k.starts_with(&pre) && k.ends_with(&suf) && !seen.contains(*k)).cloned().collect();
            for k in gone { self.sched.vals.remove(&k); view.drop.push(k); view.shape = true }
        }
        view
    }

    /// Fault `id` (stop/release, values dropped, no autorestart).
    fn fault(&mut self, id: &str, code: &str, ev: &mut Vec<Value>) {
        self.sched_retire(id, ev);
        self.drop_values(id, ev);
        let Some(rt) = self.sched.runtimes.get_mut(id) else { return };
        rt.fault = Some(code.to_string());
        ev.push(json!({ "ev": "log", "msg": "local plugin fault", "id": id, "code": code }));
        rt.status(ev, id, "fault", code, json!({}));
    }

    /// The driver could not publish an approved change (application/chart
    /// callback threw): the projection may no longer equal the accepted
    /// values, so fail the plugin truthfully as `presentation_error`.
    pub fn sched_presentation_fault(&mut self, id: &str) -> Value {
        let mut ev = Vec::new();
        if self.sched.runtimes.contains_key(id) { self.fault(id, "presentation_error", &mut ev) }
        json!({ "events": ev })
    }

    /// Outcome of the driver's Worker call for `token`. `error` = code on
    /// failure; `descriptor` = loaded plugin descriptor (JSON) for a load;
    /// `output` = the compute's output frame (once per completion).
    /// Returns `{more, events}`; a `result` event carries the approved view diff.
    pub fn sched_done(&mut self, id: &str, token: u64, error: Option<&str>, descriptor: Option<&Value>, output: Option<&Value>) -> Value {
        let mut ev = Vec::new();
        let Some(rt) = self.sched.runtimes.get_mut(id) else { return json!({ "more": false, "events": ev }) };
        // Fence: only the CURRENT in-flight token of the CURRENT incarnation.
        let tok = match rt.inflight.take() {
            Some(t) if t.id == token => t,
            other => { rt.inflight = other; return json!({ "more": false, "events": ev }) }
        };
        if tok.epoch != rt.epoch { return json!({ "more": true, "events": ev }) }
        let d = self.sched.demand.clone().expect("demand");
        let is_load = matches!(tok.kind, Kind::Load);
        // A successful compute without a parseable output frame is a plugin failure.
        let error = match (error, output, is_load) {
            (Some(c), _, _) => Some(c),
            (None, Some(o), false) if !o.is_object() => Some("invalid_output"),
            (None, None, false) => Some("invalid_output"),
            _ => None,
        };
        if let Some(code) = error {
            if let Kind::Live { .. } = tok.kind {
                if NONFATAL_LIVE.contains(&code) {
                    self.commit(id, false);
                    let rt = self.sched.runtimes.get_mut(id).unwrap();
                    rt.reinits += 1;
                    let key = format!("{}|{}", tok.scope_key, tok.revision);
                    let n = match &rt.live_reject { Some((k, n)) if *k == key => n + 1, _ => 1 };
                    rt.live_reject = Some((key, n));
                    if n >= MAX_LIVE_RETRIES {
                        rt.live_basis = Some((tok.scope_key.clone(), tok.revision));
                        let c = rt.coverage.clone();
                        rt.status(&mut ev, id, "stalled", code, json!({ "coverage": c }));
                        return json!({ "more": false, "events": ev });
                    }
                    return json!({ "more": true, "events": ev });
                }
            }
            if RELOAD_CODES.contains(&code) {
                rt_mut(self, id).loaded = false;
                self.reset(id);
                return json!({ "more": true, "events": ev });
            }
            // A COMPUTE failure on an obsolete compute basis (scope or history
            // basis replaced while in flight) has no authority over the new
            // basis. The Worker may be dead: release it and reload on the
            // current demand. Load/ABI failures and a mere live data-revision
            // advance never take this path (they still fault).
            if !is_load && (d.scope_key != tok.scope_key || d.history_revision != tok.history_revision) {
                self.sched_retire(id, &mut ev);
                ev.push(json!({ "ev": "log", "msg": "local plugin stale-basis failure discarded; reloading", "id": id, "code": code }));
                return json!({ "more": true, "events": ev });
            }
            self.fault(id, code, &mut ev);
            return json!({ "more": false, "events": ev });
        }
        let stale_from = if tok.revision != d.revision { changed_from_time(&tok.snap, &self.sched.bars) } else { None };
        let want = json!({ "scope_key": tok.scope_key, "revision": tok.revision, "history_revision": tok.history_revision, "mode": tok.mode });
        let rt = self.sched.runtimes.get_mut(id).unwrap();
        let out = output.cloned().unwrap_or(Value::Null);
        match tok.kind {
            Kind::Load => {
                let desc = descriptor.cloned().unwrap_or(Value::Null);
                let desc = match desc { Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null), v => v };
                rt.live = desc.get("live").and_then(Value::as_bool).unwrap_or(false);
                let ctx = desc.get("context");
                let ok = |v: Option<&Value>| v.and_then(Value::as_u64).filter(|&n| n <= (1u64 << 53)).map(|n| n as usize);
                rt.ctx_bars = ok(ctx.and_then(|c| c.get("bars"))).unwrap_or(0);
                rt.ctx_per_tf = ctx.and_then(|c| c.get("per_tf")).and_then(Value::as_object)
                    .map(|m| m.iter().filter_map(|(k, v)| ok(Some(v)).map(|n| (k.clone(), n))).collect()).unwrap_or_default();
                rt.loaded = true;
                rt.hist = None;
                rt.live_basis = None;
                self.reset(id);
                json!({ "more": true, "events": ev })
            }
            Kind::History { keep_time, plan, coverage, need_before, range } => {
                if d.scope_key != tok.scope_key || d.history_revision != tok.history_revision { return json!({ "more": true, "events": ev }) }
                let hrev = if rt.live { tok.history_revision } else { tok.revision };
                rt.hist = Some(Hist { scope_key: tok.scope_key.clone(), history_revision: hrev, keep_time, to_time: range.1, blocked: false });
                rt.tail_cov = Some((coverage, need_before));
                rt.prune_older(&d);
                let (c, nb) = rt.aggregate();
                rt.push_plan("tail", &plan);
                let live = rt.live;
                let view = self.merge(id, &tok.mode, false, &out, stale_from, range, d.history_revision);
                ev.push(json!({ "ev": "result", "id": id, "want": want, "kind": "history", "coverage": c, "need_before": nb,
                    "range_from": range.0, "range_to": range.1, "stale_from": stale_from, "view": view.json() }));
                if !live {
                    let l = if c == "ready" { "ready".to_string() } else { c.clone() };
                    rt_mut(self, id).status(&mut ev, id, &l, "", json!({ "coverage": c, "needBefore": nb }));
                }
                json!({ "more": true, "events": ev })
            }
            Kind::Older { key, plan, coverage, need_before, range } => {
                if d.scope_key != tok.scope_key || d.history_revision != tok.history_revision { return json!({ "more": true, "events": ev }) }
                rt.older_done.insert(key.clone());
                rt.older_cov.insert(key, (coverage, need_before));
                let (c, nb) = rt.aggregate();
                rt.push_plan("older", &plan);
                let live = rt.live;
                let view = self.merge(id, &tok.mode, false, &out, stale_from, range, d.history_revision);
                ev.push(json!({ "ev": "result", "id": id, "want": want, "kind": "history", "older": true, "coverage": c, "need_before": nb,
                    "range_from": range.0, "range_to": range.1, "stale_from": stale_from, "view": view.json() }));
                if !live {
                    let l = if c == "ready" { "ready".to_string() } else { c.clone() };
                    rt_mut(self, id).status(&mut ev, id, &l, "", json!({ "coverage": c, "needBefore": nb }));
                }
                json!({ "more": true, "events": ev })
            }
            Kind::Live { op, touched } => {
                self.commit(id, true);
                let rt = self.sched.runtimes.get_mut(id).unwrap();
                rt.ops[(op - 2) as usize] += 1;
                if d.scope_key != tok.scope_key || d.history_revision != tok.history_revision { return json!({ "more": true, "events": ev }) }
                rt.live_basis = Some((tok.scope_key.clone(), tok.revision));
                let view = self.merge(id, &tok.mode, true, &out, stale_from, (None, None), d.history_revision);
                ev.push(json!({ "ev": "result", "id": id, "want": want, "kind": "live", "op": op, "touched": touched, "stale_from": stale_from, "view": view.json() }));
                let rt = rt_mut(self, id);
                if rt.hist.as_ref().is_some_and(|h| h.blocked) {
                    rt.status(&mut ev, id, "unavailable", "invalid_input_window", json!({ "coverage": "unavailable" }));
                } else {
                    let l = rt.ready_label();
                    let c = rt.coverage.clone();
                    rt.status(&mut ev, id, &l, "", json!({ "coverage": c }));
                }
                json!({ "more": true, "events": ev })
            }
        }
    }

    /// Driver-visible diagnostics (readonly).
    pub fn sched_inspect(&self, id: &str) -> Value {
        let Some(rt) = self.sched.runtimes.get(id) else { return Value::Null };
        json!({ "id": id, "instance": rt.instance, "epoch": rt.epoch, "loaded": rt.loaded, "live": rt.live, "state": rt.state, "code": rt.code,
            "coverage": rt.coverage, "fault": rt.fault.as_ref().map(|c| json!({ "code": c })), "reinits": rt.reinits,
            "ops": { "2": rt.ops[0], "3": rt.ops[1] }, "plans": rt.plans, "lastPlan": rt.last_plan, "inflight": rt.inflight.is_some(),
            "blocked": rt.hist.as_ref().is_some_and(|h| h.blocked),
            "olderDone": rt.older_done, "olderCov": rt.older_cov.iter().map(|(k, (c, n))| (k.clone(), json!({ "coverage": c, "needBefore": n }))).collect::<serde_json::Map<_, _>>() })
    }
    /// Accepted values of `id` (tests/diagnostics): `{key: {time: value}}`.
    pub fn sched_values_json(&self, id: &str) -> Value {
        let pre = format!("{id}/");
        Value::Object(self.sched.vals.iter().filter(|(k, _)| k.starts_with(&pre))
            .map(|(k, c)| (k.clone(), Value::Object(c.v.iter().map(|(t, v)| (t.to_string(), v.clone())).collect()))).collect())
    }
    pub fn sched_running(&self) -> Vec<String> {
        self.sched.runtimes.iter().filter(|(_, rt)| rt.loaded).map(|(k, _)| k.clone()).collect()
    }
    pub fn sched_ids(&self) -> Vec<String> { self.sched.runtimes.keys().cloned().collect() }
    pub fn sched_stop(&mut self) -> Vec<Value> {
        let mut ev = Vec::new();
        for id in self.sched_ids() { self.sched_retire(&id, &mut ev) }
        self.sched.runtimes.clear();
        self.sched.vals.clear();
        ev
    }
}
