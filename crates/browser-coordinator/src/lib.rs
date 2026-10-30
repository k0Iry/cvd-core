//! Browser data/session coordinator for local plugins (PARENT-B-REVIEW §1).
//!
//! Owns the substantive logic that must not live in JS:
//! * canonical server bar -> SDK `InputBar` mapping (strict; fail closed);
//! * History request planning over an explicit requested range with the
//!   actually available warm-up prefix (`output_from = keep - from`)
//!   and truthful coverage (`warming` until context bars precede an output);
//! * per-plugin persistent LIVE session planning: LiveInit (op 2) with the
//!   SDK window cap, then LiveUpdate (op 3) deltas (append/replace) with
//!   exact `seq`, a session revision FIXED for the session (distinct from
//!   the browser cache revision), and explicit reinit (never a silent
//!   History rewrite) when the delta cannot be expressed: scope change,
//!   gap/rewrite older than the window, removed bars, failed update.
//!
//! Commit protocol: `plan` stages a pending shadow; `commit(ok=true)` makes
//! it current only after the host validated the plugin output; `commit(false)`
//! drops the session so the next plan is a LiveInit.

use cvd_indicator_sdk::{author::LIVE_REPLACE_SLACK, InputBar, Scope};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const OP_HISTORY: u32 = 1;
pub const OP_LIVE_INIT: u32 = 2;
pub const OP_LIVE_UPDATE: u32 = 3;

const F64_FIELDS: [&str; 20] = [
    "po", "ph", "pl", "price_close", "apo", "aph", "apl", "apc", "cvd_usdt", "ac", "delta_usdt", "ad",
    "bvb", "abvb", "svb", "asvb", "buy_volume_usdt", "agb", "sell_volume_usdt", "ags",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoordError {
    pub code: &'static str,
    pub message: String,
}
fn invalid(message: impl Into<String>) -> CoordError {
    CoordError { code: "invalid_input", message: message.into() }
}

/// Strict projection of a canonical server bar (serde wire names of the
/// server `Bar`) to the SDK input. Missing / non-finite fields fail closed.
pub fn input_bar(v: &Value) -> Result<InputBar, CoordError> {
    let o = v.as_object().ok_or_else(|| invalid("bar is not an object"))?;
    let time = o.get("time").and_then(Value::as_i64).filter(|t| *t > 0).ok_or_else(|| invalid("bar without canonical time"))?;
    let mut m = Map::new();
    m.insert("time".into(), Value::from(time));
    for f in F64_FIELDS {
        let x = o.get(f).and_then(Value::as_f64).filter(|x| x.is_finite()).ok_or_else(|| invalid(format!("bar {time}: {f} missing")))?;
        m.insert(f.into(), Value::from(x));
    }
    let trades = o.get("trades").and_then(Value::as_u64).ok_or_else(|| invalid(format!("bar {time}: trades missing")))?;
    m.insert("trades".into(), Value::from(trades));
    serde_json::from_value(Value::Object(m)).map_err(|e| invalid(e.to_string()))
}

pub fn input_bars(vs: &[Value]) -> Result<Vec<InputBar>, CoordError> {
    let bars: Vec<InputBar> = vs.iter().map(input_bar).collect::<Result<_, _>>()?;
    if bars.windows(2).any(|w| w[0].time >= w[1].time) {
        return Err(invalid("bars not strictly increasing"));
    }
    Ok(bars)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// No output yet: fewer than context+1 bars available.
    Warming,
    /// Some outputs, but the oldest cached bars lack context and more history exists.
    Partial,
    /// Every cached bar from `first_output_time` on has full context; source exhausted or covered.
    Ready,
    /// Source exhausted with no bar that can ever have full context (terminal).
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct HistoryPlan {
    pub body: String,
    pub output_from: usize,
    pub context: usize,
    pub coverage: Coverage,
    /// Extra older bars needed so that `want_from_time` (oldest visible) has full context.
    pub need_before: usize,
    /// True when older history is exhausted: coverage will not improve by warm-up.
    pub terminal: bool,
    /// Index into the caller's bars where the trimmed History input starts.
    pub trimmed_from: usize,
    /// Exclusive end index of the input (explicit requested end).
    pub input_to: usize,
    /// Canonical requested range [from, to] (bar times) the result covers;
    /// the consumer replaces only values inside it.
    pub range_from_time: Option<i64>,
    pub range_to_time: Option<i64>,
}

/// Plan a stateless History for the canonical requested range
/// `[want_from_index, want_to_index)` of the shared cache, mirroring the old
/// PUBLIC server basis (indicators.rs compute_history_guarded):
/// `from = keep.saturating_sub(ctx)`, `output_from = keep - from` (the
/// ACTUALLY available prefix), `inputs[from..end]` with an explicit right
/// bound. Short/exhausted context keeps every requested bar (the plugin
/// decides its declared partial/null semantics); coverage stays truthful.
/// Note: window-origin-sensitive (e.g. binary64 prefix-sum) algorithms are
/// only bit-stable for the same origin, so the caller must pass a canonical
/// requested start rather than a moving viewport/cache origin.
pub fn plan_history(scope: &Scope, revision: u64, bars: &[Value], context: usize, want_from_index: usize, want_to_index: usize, exhausted: bool) -> Result<HistoryPlan, CoordError> {
    plan_history_checked(scope, revision, bars, context, want_from_index, want_to_index, exhausted, &[])
}

/// Canonical bucket times reported invalid (dropped as non-renderable by the
/// producer: `/history.invalid_times` or a WS `invalidate`) that fall inside
/// the closed input time span `[from, to]`. Successor of the retired server
/// `window_poisoned`: the browser cache no longer contains such a bucket, so a
/// window spanning it would silently bridge the gap as if contiguous.
pub fn invalid_in_span(invalid_times: &[i64], from: i64, to: i64) -> Vec<i64> {
    invalid_times.iter().copied().filter(|&t| t >= from && t <= to).collect()
}

fn invalid_window(times: &[i64]) -> CoordError {
    CoordError { code: "invalid_window", message: format!("computation window spans invalid canonical bucket(s) {times:?}") }
}

/// [`plan_history`] that fails closed (`invalid_window`) when the planned
/// input (actual warm-up prefix + requested range) spans a known invalid
/// bucket. No zero/fabricated bar is inserted and the gap is never bridged.
#[allow(clippy::too_many_arguments)]
pub fn plan_history_checked(scope: &Scope, revision: u64, bars: &[Value], context: usize, want_from_index: usize, want_to_index: usize, exhausted: bool, invalid_times: &[i64]) -> Result<HistoryPlan, CoordError> {
    let input_to = want_to_index.min(bars.len());
    let keep = want_from_index.min(input_to);
    let trimmed_from = keep.saturating_sub(context);
    let output_from = keep - trimmed_from;
    let need_before = context - output_from;
    let input = input_bars(&bars[trimmed_from..input_to])?;
    if let (Some(first), Some(last)) = (input.first(), input.last()) {
        let hit = invalid_in_span(invalid_times, first.time, last.time);
        if !hit.is_empty() {
            return Err(invalid_window(&hit));
        }
    }
    let requested = input_to - keep;
    // Missing full context is NOT "no usable output": an exhausted source with
    // requested bars is terminal `Partial` (plugin's declared partial/null
    // semantics decide values); `Unavailable` only when nothing was requested.
    let coverage = if requested == 0 {
        if exhausted { Coverage::Unavailable } else { Coverage::Warming }
    } else if need_before > 0 && input.len() <= context && !exhausted {
        Coverage::Warming
    } else if need_before > 0 {
        Coverage::Partial
    } else {
        Coverage::Ready
    };
    let range_from_time = (requested > 0).then(|| input[output_from].time);
    let range_to_time = (requested > 0).then(|| input[input.len() - 1].time);
    let body = serde_json::to_string(&cvd_indicator_sdk::HistoryRequest { scope: scope.clone(), revision, output_from, bars: input })
        .map_err(|e| invalid(e.to_string()))?;
    Ok(HistoryPlan { body, output_from, context, coverage, need_before, terminal: exhausted, trimmed_from, input_to, range_from_time, range_to_time })
}


#[derive(Debug, Clone)]
struct Session {
    scope_key: String,
    revision: u64,
    seq: u64,
    window: Vec<InputBar>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LivePlan {
    /// 0 = nothing to do, 2 = LiveInit, 3 = LiveUpdate.
    pub op: u32,
    pub body: String,
    pub seq: u64,
    pub session_revision: u64,
    /// Why a LiveInit was chosen (diagnostics/tests): "new", "scope", "gap", "rewrite", "removed", "reset".
    pub reason: &'static str,
    pub touched: Vec<i64>,
}

#[derive(Default)]
pub struct Coordinator {
    sessions: BTreeMap<String, Session>,
    pending: BTreeMap<String, Session>,
    next_revision: u64,
}

impl Coordinator {
    pub fn new() -> Self { Self { next_revision: 1, ..Default::default() } }

    /// Drop a plugin's live session (disable/uninstall/replace/fault/reload).
    pub fn reset(&mut self, plugin: &str) {
        self.sessions.remove(plugin);
        self.pending.remove(plugin);
    }

    pub fn has_session(&self, plugin: &str) -> bool { self.sessions.contains_key(plugin) }

    /// Plan the next live step for `plugin` from the cache tail. `tail` must
    /// be the newest canonical bars (oldest first), at least the SDK window cap.
    pub fn plan_live(&mut self, plugin: &str, scope_key: &str, scope: &Scope, context: usize, tail: &[Value]) -> Result<LivePlan, CoordError> {
        self.plan_live_checked(plugin, scope_key, scope, context, tail, &[])
    }

    /// [`plan_live`] aware of invalid canonical buckets: the session window
    /// never spans one. The window is trimmed to the bars AFTER the newest
    /// invalid time inside it; a committed session whose window reaches back
    /// to/over it is explicitly re-initialised (`reason = "invalid"`). When the
    /// trimmed window cannot give the newest output full context the session
    /// is dropped and `op = 0, reason = "invalid_window"` is returned: the
    /// caller must withdraw live values (fail closed) until the context moves
    /// past the invalid bucket or a newer valid replacement clears it.
    pub fn plan_live_checked(&mut self, plugin: &str, scope_key: &str, scope: &Scope, context: usize, tail: &[Value], invalid_times: &[i64]) -> Result<LivePlan, CoordError> {
        let cap = context + 1 + LIVE_REPLACE_SLACK;
        let start = tail.len().saturating_sub(cap);
        let mut window = input_bars(&tail[start..])?;
        if window.is_empty() {
            return Ok(LivePlan { op: 0, body: String::new(), seq: 0, session_revision: 0, reason: "empty", touched: vec![] });
        }
        self.pending.remove(plugin);
        let newest_invalid = invalid_in_span(invalid_times, window[0].time, window[window.len() - 1].time).into_iter().max();
        if let Some(li) = newest_invalid {
            window.retain(|b| b.time > li);
            if window.len() < context + 1 {
                self.sessions.remove(plugin);
                return Ok(LivePlan { op: 0, body: String::new(), seq: 0, session_revision: 0, reason: "invalid_window", touched: vec![li] });
            }
        }
        let reason = match self.sessions.get(plugin) {
            None => Some("new"),
            Some(s) if s.scope_key != scope_key => Some("scope"),
            Some(s) if newest_invalid.is_some_and(|li| s.window.first().is_some_and(|b| b.time <= li)) => Some("invalid"),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            return self.init(plugin, scope_key, scope, window, reason);
        }
        let s = self.sessions.get(plugin).expect("session");
        // Diff the new window against the committed session window. Indexed
        // by time (O(n log n) at most), never nested linear searches.
        let by_time: BTreeMap<i64, usize> = window.iter().enumerate().map(|(i, b)| (b.time, i)).collect();
        let last = s.window.last().map(|b| b.time).unwrap_or(i64::MIN);
        let mut ops: Vec<Value> = Vec::new();
        let mut touched = Vec::new();
        let first_new = window.first().map(|b| b.time).unwrap_or(i64::MAX);
        // Every session bar inside the new window's time range must still exist.
        for old in s.window.iter().filter(|b| b.time >= first_new) {
            match by_time.get(&old.time).map(|&i| &window[i]) {
                None => return self.init(plugin, scope_key, scope, window, "removed"),
                Some(nb) if nb != old => {
                    ops.push(serde_json::json!({ "op": "replace", "bar": nb }));
                    touched.push(nb.time);
                }
                Some(_) => {}
            }
        }
        let appended: Vec<&InputBar> = window.iter().filter(|b| b.time > last).collect();
        if appended.len() == window.len() && !s.window.is_empty() {
            // No overlap at all: the delta exceeds what the session can express.
            return self.init(plugin, scope_key, scope, window, "gap");
        }
        for b in appended {
            ops.push(serde_json::json!({ "op": "append", "bar": b }));
            touched.push(b.time);
        }
        if ops.is_empty() {
            return Ok(LivePlan { op: 0, body: String::new(), seq: s.seq, session_revision: s.revision, reason: "unchanged", touched });
        }
        // Stage the post-update window exactly as the SDK will (trim to cap).
        let mut next = s.clone();
        next.seq += 1;
        let mut w = s.window.clone();
        let slot_of: BTreeMap<i64, usize> = w.iter().enumerate().map(|(i, b)| (b.time, i)).collect();
        for t in &touched {
            let nb = window[by_time[t]].clone();
            match slot_of.get(t) { Some(&i) => w[i] = nb, None => w.push(nb) }
        }
        if w.len() > cap { w.drain(..w.len() - cap); }
        next.window = w;
        let body = serde_json::json!({ "revision": s.revision, "seq": next.seq, "ops": ops }).to_string();
        let plan = LivePlan { op: OP_LIVE_UPDATE, body, seq: next.seq, session_revision: s.revision, reason: "delta", touched };
        self.pending.insert(plugin.to_string(), next);
        Ok(plan)
    }

    fn init(&mut self, plugin: &str, scope_key: &str, scope: &Scope, window: Vec<InputBar>, reason: &'static str) -> Result<LivePlan, CoordError> {
        let revision = self.next_revision;
        self.next_revision += 1;
        let touched = window.last().map(|b| vec![b.time]).unwrap_or_default();
        let body = serde_json::to_string(&cvd_indicator_sdk::LiveInit { scope: scope.clone(), revision, bars: window.clone() }).map_err(|e| invalid(e.to_string()))?;
        self.sessions.remove(plugin);
        self.pending.insert(plugin.to_string(), Session { scope_key: scope_key.to_string(), revision, seq: 0, window });
        Ok(LivePlan { op: OP_LIVE_INIT, body, seq: 0, session_revision: revision, reason, touched })
    }

    /// Host verdict for the last plan. Only a validated OK commits state.
    pub fn commit(&mut self, plugin: &str, ok: bool) {
        match (ok, self.pending.remove(plugin)) {
            (true, Some(s)) => { self.sessions.insert(plugin.to_string(), s); }
            _ => { self.sessions.remove(plugin); }
        }
    }
}

pub mod ffi;
