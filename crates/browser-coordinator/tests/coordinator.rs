use cvd_browser_coordinator::*;
use cvd_indicator_sdk::{author::LIVE_REPLACE_SLACK, LiveInit, LiveUpdate, Scope};
use serde_json::{json, Value};

fn scope() -> Scope { serde_json::from_value(json!({"exchange":"binance","market":"spot","symbol":"BTCUSDT","tf":"1m","mode":"source"})).unwrap() }
fn bar(t: i64, c: f64) -> Value {
    json!({"time":t,"po":c,"ph":c+1.0,"pl":c-1.0,"price_close":c,"apo":c,"aph":c+1.0,"apl":c-1.0,"apc":c,
      "cvd_usdt":c*2.0,"ac":c*3.0,"delta_usdt":0.0,"ad":-0.0,"bvb":1.0,"abvb":1.0,"svb":1.0,"asvb":1.0,
      "buy_volume_usdt":5.0,"agb":5.0,"sell_volume_usdt":5.0,"ags":5.0,"trades":3,"extra_server_field":"ignored"})
}
fn bars(n: i64) -> Vec<Value> { (1..=n).map(|i| bar(i * 60, 100.0 + i as f64)).collect() }

#[test]
fn mapping_fails_closed_and_keeps_authoritative_cvd() {
    let b = input_bar(&bar(60, 7.0)).unwrap();
    assert_eq!((b.cvd_usdt, b.ac, b.trades), (14.0, 21.0, 3));
    let mut bad = bar(60, 7.0); bad.as_object_mut().unwrap().remove("ac");
    assert_eq!(input_bar(&bad).unwrap_err().code, "invalid_input");
    let mut nul = bar(60, 7.0); nul["cvd_usdt"] = Value::Null;
    assert!(input_bar(&nul).is_err(), "null CVD must not become zero");
    assert!(input_bars(&[bar(120, 1.0), bar(60, 1.0)]).is_err());
}

#[test]
fn history_warmup_coverage_is_truthful() {
    let p = plan_history(&scope(), 9, &bars(5), 5, 0, 5, false).unwrap();
    assert_eq!((p.output_from, p.coverage, p.terminal), (0, Coverage::Warming, false), "actual prefix, requested bars kept");
    let p = plan_history(&scope(), 9, &bars(20), 5, 2, 20, false).unwrap();
    assert_eq!((p.coverage, p.need_before, p.terminal), (Coverage::Partial, 3, false));
    // Exhausted source with visible bars lacking context: terminal partial,
    // never falsely ready and never forever warming.
    let p = plan_history(&scope(), 9, &bars(20), 5, 2, 20, true).unwrap();
    assert_eq!((p.coverage, p.need_before, p.terminal), (Coverage::Partial, 3, true));
    // Exhausted with fewer bars than context: requested bars may still have
    // usable partial output -> terminal Partial, not Unavailable.
    let p = plan_history(&scope(), 9, &bars(5), 5, 0, 5, true).unwrap();
    assert_eq!((p.coverage, p.terminal), (Coverage::Partial, true));
    // Unavailable only when nothing at all is requested on an exhausted source.
    let p = plan_history(&scope(), 9, &bars(5), 5, 5, 5, true).unwrap();
    assert_eq!((p.coverage, p.terminal), (Coverage::Unavailable, true));
    let p = plan_history(&scope(), 9, &bars(20), 5, 10, 20, false).unwrap();
    assert_eq!((p.coverage, p.need_before), (Coverage::Ready, 0));
    let p = plan_history(&scope(), 9, &bars(3), 0, 0, 3, true).unwrap();
    assert_eq!(p.coverage, Coverage::Ready);
}

#[test]
fn history_input_is_trimmed_to_requested_range_plus_context() {
    // Large cache, request starts deep inside: body holds only range + context.
    let p = plan_history(&scope(), 9, &bars(5000), 50, 4000, 5000, false).unwrap();
    assert_eq!((p.trimmed_from, p.output_from, p.coverage), (3950, 50, Coverage::Ready));
    let req: cvd_indicator_sdk::HistoryRequest = serde_json::from_str(&p.body).unwrap();
    assert_eq!(req.bars.len(), 1050, "range+context, not whole cache");
    // Left edge: nothing to trim, truthful need_before.
    let p = plan_history(&scope(), 9, &bars(100), 50, 10, 100, false).unwrap();
    assert_eq!((p.trimmed_from, p.need_before), (0, 40));
}

#[test]
fn live_init_then_append_replace_with_fixed_session_revision() {
    let mut c = Coordinator::new();
    let ctx = 3;
    let mut cache = bars(200);
    let p = c.plan_live("a", "k", &scope(), ctx, &cache).unwrap();
    assert_eq!((p.op, p.reason), (OP_LIVE_INIT, "new"));
    let init: LiveInit = serde_json::from_str(&p.body).unwrap();
    assert_eq!(init.bars.len(), ctx + 1 + LIVE_REPLACE_SLACK, "bounded window");
    let rev = p.session_revision;
    // Not committed yet -> still no session.
    assert!(!c.has_session("a"));
    c.commit("a", true);
    assert_eq!(c.plan_live("a", "k", &scope(), ctx, &cache).unwrap().op, 0, "unchanged tail costs nothing");
    // replace forming bar + append
    *cache.last_mut().unwrap() = bar(200 * 60, 999.0);
    cache.push(bar(201 * 60, 5.0));
    let p = c.plan_live("a", "k", &scope(), ctx, &cache).unwrap();
    assert_eq!(p.op, OP_LIVE_UPDATE);
    let u: LiveUpdate = serde_json::from_str(&p.body).unwrap();
    assert_eq!((u.revision, u.seq, u.ops.len()), (rev, 1, 2));
    assert_eq!(p.touched, vec![200 * 60, 201 * 60]);
    c.commit("a", true);
    cache.push(bar(202 * 60, 6.0));
    let p = c.plan_live("a", "k", &scope(), ctx, &cache).unwrap();
    let u: LiveUpdate = serde_json::from_str(&p.body).unwrap();
    assert_eq!((u.revision, u.seq), (rev, 2), "session revision fixed, seq exact");
    // failed update => explicit reinit, never silent history
    c.commit("a", false);
    let p = c.plan_live("a", "k", &scope(), ctx, &cache).unwrap();
    assert_eq!((p.op, p.reason), (OP_LIVE_INIT, "new"));
    assert_ne!(p.session_revision, rev);
}

#[test]
fn scope_gap_and_removed_bars_reinit() {
    let mut c = Coordinator::new();
    let cache = bars(100);
    c.plan_live("a", "k", &scope(), 2, &cache).unwrap(); c.commit("a", true);
    assert_eq!(c.plan_live("a", "k2", &scope(), 2, &cache).unwrap().reason, "scope");
    c.commit("a", true);
    let far: Vec<Value> = (1000..1200).map(|i| bar(i * 60, 1.0)).collect();
    assert_eq!(c.plan_live("a", "k2", &scope(), 2, &far).unwrap().reason, "gap");
    c.commit("a", true);
    let mut holes = far.clone(); holes.remove(190);
    assert_eq!(c.plan_live("a", "k2", &scope(), 2, &holes).unwrap().reason, "removed");
    // reset (disable/uninstall) drops the session
    c.commit("a", true); c.reset("a");
    assert_eq!(c.plan_live("a", "k2", &scope(), 2, &holes).unwrap().reason, "new");
}

#[test]
fn ffi_dispatch_roundtrip() {
    let mut c = Coordinator::new();
    let r = ffi::dispatch(&mut c, &json!({"cmd":"live","plugin":"p","scope_key":"k","scope":scope(),"context":1,"tail":bars(3)})).unwrap();
    assert_eq!(r["op"], 2);
    assert_eq!(ffi::dispatch(&mut c, &json!({"cmd":"nope"})).unwrap_err().code, "bad_command");
}

fn req(p: &HistoryPlan) -> cvd_indicator_sdk::HistoryRequest { serde_json::from_str(&p.body).unwrap() }

/// Old PUBLIC server window basis (indicators.rs compute_history_guarded):
/// from = keep.saturating_sub(ctx), output_from = keep - from, inputs[from..end].
fn server_basis(bars: &[Value], ctx: usize, keep: usize, end: usize) -> (usize, Vec<i64>) {
    let from = keep.saturating_sub(ctx);
    (keep - from, bars[from..end].iter().map(|b| b["time"].as_i64().unwrap()).collect())
}

#[test]
fn window_sufficient_prefix_matches_server_basis_and_is_right_bounded() {
    let cache = bars(3000);
    let p = plan_history(&scope(), 9, &cache, 40, 1000, 1500, false).unwrap();
    let (of, times) = server_basis(&cache, 40, 1000, 1500);
    assert_eq!(p.output_from, of);
    assert_eq!(req(&p).bars.iter().map(|b| b.time).collect::<Vec<_>>(), times, "input stops at the explicit requested end");
    assert_eq!((p.range_from_time, p.range_to_time), (Some(1001 * 60), Some(1500 * 60)));
    assert_eq!(p.coverage, Coverage::Ready);
}

#[test]
fn window_short_prefix_keeps_requested_bars_with_actual_output_from() {
    let cache = bars(300);
    let p = plan_history(&scope(), 9, &cache, 50, 20, 300, false).unwrap();
    let (of, times) = server_basis(&cache, 50, 20, 300);
    assert_eq!((p.trimmed_from, p.output_from, p.need_before), (0, of, 30));
    assert_eq!(req(&p).bars.iter().map(|b| b.time).collect::<Vec<_>>(), times, "no requested bar silently dropped");
    assert_eq!(p.coverage, Coverage::Partial);
    assert_eq!(p.range_from_time, Some(21 * 60));
}

#[test]
fn window_source_exhaustion_is_terminal_and_truthful() {
    let p = plan_history(&scope(), 9, &bars(30), 50, 0, 30, true).unwrap();
    assert_eq!((p.output_from, p.coverage, p.terminal), (0, Coverage::Partial, true));
    assert_eq!(req(&p).bars.len(), 30);
    let p = plan_history(&scope(), 9, &bars(80), 50, 10, 80, true).unwrap();
    assert_eq!((p.output_from, p.coverage, p.terminal), (10, Coverage::Partial, true));
}

#[test]
fn window_middle_range_in_large_cache_is_body_identical_regardless_of_cache_extent() {
    let big = bars(20_000);
    let small: Vec<Value> = big[9_000..12_000].to_vec();
    let a = plan_history(&scope(), 9, &big, 64, 10_000, 10_500, false).unwrap();
    let b = plan_history(&scope(), 9, &small, 64, 1_000, 1_500, false).unwrap();
    assert_eq!(a.body, b.body, "byte-identical History body for the same requested window");
    assert_eq!(req(&a).bars.len(), 564);
}

/// Generic PUBLIC window-origin-sensitive computation (binary64 running sum
/// from the window origin, as a prefix-sum indicator would do).
fn prefix_sum_outputs(r: &cvd_indicator_sdk::HistoryRequest) -> Vec<u64> {
    let mut acc = 0.0f64;
    let mut out = Vec::new();
    for (i, b) in r.bars.iter().enumerate() {
        acc += b.cvd_usdt * 0.1 + 1e-7;
        if i >= r.output_from { out.push(acc.to_bits()); }
    }
    out
}

#[test]
fn window_sensitive_prefix_sum_is_bit_stable_only_on_the_same_origin() {
    let big = bars(20_000);
    let small: Vec<Value> = big[9_000..12_000].to_vec();
    let a = plan_history(&scope(), 9, &big, 64, 10_000, 10_500, false).unwrap();
    let b = plan_history(&scope(), 9, &small, 64, 1_000, 1_500, false).unwrap();
    assert_eq!(prefix_sum_outputs(&req(&a)), prefix_sum_outputs(&req(&b)));
    // A reanchored origin changes binary64 results: the planner must not
    // reanchor to a moving viewport and claim parity.
    let c = plan_history(&scope(), 9, &big, 64, 9_990, 10_500, false).unwrap();
    let (ta, tc) = (prefix_sum_outputs(&req(&a)), prefix_sum_outputs(&req(&c)));
    assert_ne!(tc[tc.len() - ta.len()..], ta[..]);
}

// ---- V2 validity: successor of the retired server window_poisoned guard ----

/// Cache after the producer dropped bucket `bad` (removed, NOT zero-filled).
fn cache_without(n: i64, bad: i64) -> Vec<Value> { bars(n).into_iter().filter(|b| b["time"].as_i64() != Some(bad)).collect() }

#[test]
fn history_window_spanning_invalid_bucket_fails_closed_unaffected_windows_exact() {
    let bad = 30 * 60;
    let cache = cache_without(40, bad); // 39 bars, gap at 30
    // Window (requested 35..40 = indices 34.., ctx 2 => input from index 32 = t33) after the invalid bucket: allowed and
    // byte-identical to the unchecked plan (no change for unaffected windows).
    let a = plan_history_checked(&scope(), 9, &cache, 2, 34, 39, false, &[bad]).unwrap();
    let b = plan_history(&scope(), 9, &cache, 2, 34, 39, false).unwrap();
    assert_eq!(a.body, b.body);
    // Trailing invalid (requested range itself spans it).
    let e = plan_history_checked(&scope(), 9, &cache, 2, 20, 39, false, &[bad]).unwrap_err();
    assert_eq!(e.code, "invalid_window");
    assert!(e.message.contains(&bad.to_string()));
    // Context-only invalid: requested 31.. (index 29 = t31) but ctx 2 reaches back over 30.
    assert_eq!(plan_history_checked(&scope(), 9, &cache, 2, 29, 39, false, &[bad]).unwrap_err().code, "invalid_window");
    // Leading: page entirely BEFORE the invalid bucket (explicit right end) stays exact.
    let lead = plan_history_checked(&scope(), 9, &cache, 2, 5, 20, false, &[bad]).unwrap();
    assert_eq!(lead.body, plan_history(&scope(), 9, &cache, 2, 5, 20, false).unwrap().body);
    // Invalid times outside the cache (other page) do not poison.
    assert!(plan_history_checked(&scope(), 9, &cache, 2, 34, 39, false, &[bad, 10_000 * 60, 0]).is_ok());
    // Repair: once cleared (valid replacement re-inserted), full window is exact again.
    let repaired = bars(40);
    assert!(plan_history_checked(&scope(), 9, &repaired, 2, 20, 40, false, &[]).is_ok());
}

#[test]
fn live_session_never_bridges_invalid_bucket_and_recovers() {
    let mut c = Coordinator::new();
    let ctx = 3;
    let full = bars(200);
    let p = c.plan_live_checked("a", "k", &scope(), ctx, &full, &[]).unwrap();
    assert_eq!(p.op, OP_LIVE_INIT);
    c.commit("a", true);
    // Producer invalidates the newest bucket 200: cache drops it.
    let bad = 200 * 60;
    let cache = cache_without(200, bad);
    // Newest output (t199) context {196..199} does not include 200 -> but session window spans 200? no: 200 > 199 (outside).
    // invalidate an interior bucket within the window instead:
    let bad2 = 198 * 60;
    let cache2: Vec<Value> = cache.iter().filter(|b| b["time"].as_i64() != Some(bad2)).cloned().collect();
    let p = c.plan_live_checked("a", "k", &scope(), ctx, &cache2, &[bad, bad2]).unwrap();
    assert_eq!((p.op, p.reason), (0, "invalid_window"), "only 1 bar after the invalid bucket < ctx+1");
    assert_eq!(p.touched, vec![bad2]);
    assert!(!c.has_session("a"), "session dropped (fail closed)");
    // Unrelated new valid ticks: still poisoned until context moves past 198.
    let mut grow = cache2.clone();
    grow.push(bar(201 * 60, 1.0));
    grow.push(bar(202 * 60, 1.0));
    assert_eq!(c.plan_live_checked("a", "k", &scope(), ctx, &grow, &[bad, bad2]).unwrap().reason, "invalid_window");
    grow.push(bar(203 * 60, 1.0));
    // 201..203 after newest invalid (200): ctx+1 = 4 not yet reached.
    assert_eq!(c.plan_live_checked("a", "k", &scope(), ctx, &grow, &[bad, bad2]).unwrap().reason, "invalid_window");
    grow.push(bar(204 * 60, 1.0));
    // 201..204: exactly ctx+1 -> LiveInit with a window starting after it.
    let p = c.plan_live_checked("a", "k", &scope(), ctx, &grow, &[bad, bad2]).unwrap();
    assert_eq!((p.op, p.reason), (OP_LIVE_INIT, "new"));
    let init: LiveInit = serde_json::from_str(&p.body).unwrap();
    assert_eq!(init.bars.iter().map(|b| b.time).collect::<Vec<_>>(), vec![201 * 60, 202 * 60, 203 * 60, 204 * 60]);
    assert!(init.bars.iter().all(|b| b.time > bad), "window starts after the invalid bucket");
    c.commit("a", true);
    // A committed clean session stays a delta when an OLDER invalid is reported.
    grow.push(bar(205 * 60, 1.0));
    let p = c.plan_live_checked("a", "k", &scope(), ctx, &grow, &[bad, bad2]).unwrap();
    assert_eq!(p.op, OP_LIVE_UPDATE);
    c.commit("a", true);
    // Session reaching back over a NEW invalidation -> explicit reinit "invalid".
    let bad3 = 202 * 60;
    let g3: Vec<Value> = grow.iter().filter(|b| b["time"].as_i64() != Some(bad3)).cloned().chain((206..=209).map(|i| bar(i * 60, 2.0))).collect();
    let p = c.plan_live_checked("a", "k", &scope(), ctx, &g3, &[bad, bad2, bad3]).unwrap();
    assert_eq!((p.op, p.reason), (OP_LIVE_INIT, "invalid"));
    let init: LiveInit = serde_json::from_str(&p.body).unwrap();
    assert!(init.bars.iter().all(|b| b.time > bad3));
    // Repair (poison cleared by a newer valid version): full window, no invalid trimming.
    c.commit("a", true);
    let mut fixed = grow.clone(); fixed.extend((206..=209).map(|i| bar(i * 60, 2.0)));
    let p = c.plan_live_checked("a", "k", &scope(), ctx, &fixed, &[]).unwrap();
    assert_ne!(p.reason, "invalid_window");
}

#[test]
fn ffi_accepts_optional_invalid_times_and_rejects_malformed() {
    let mut c = Coordinator::new();
    let cache = cache_without(40, 1800);
    let cmd = |inv: Value| json!({"cmd":"history","scope":scope(),"revision":1,"bars":cache,"context":2,"want_from_index":20,"want_to_index":39,"exhausted":false,"invalid_times":inv});
    assert_eq!(ffi::dispatch(&mut c, &cmd(json!([1800]))).unwrap_err().code, "invalid_window");
    assert!(ffi::dispatch(&mut c, &cmd(Value::Null)).is_ok(), "absent/null = none (back-compatible)");
    assert_eq!(ffi::dispatch(&mut c, &cmd(json!(["x"]))).unwrap_err().code, "invalid_input");
}
