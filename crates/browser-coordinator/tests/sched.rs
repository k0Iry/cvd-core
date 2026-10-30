//! Rust-owned scheduling/result-authority policy and fences (sched.rs),
//! exercised through the same JSON dispatcher the browser driver uses.
use cvd_browser_coordinator::*;
use serde_json::{json, Value};

const F: [&str; 20] = ["po", "ph", "pl", "price_close", "apo", "aph", "apl", "apc", "cvd_usdt", "ac", "delta_usdt", "ad",
    "bvb", "abvb", "svb", "asvb", "buy_volume_usdt", "agb", "sell_volume_usdt", "ags"];
fn bar(t: i64, k: f64) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("time".into(), json!(t));
    for (i, f) in F.iter().enumerate() { m.insert((*f).into(), json!(i as f64 + t as f64 / 60.0 + k)); }
    m.insert("trades".into(), json!(3));
    Value::Object(m)
}
fn bars(n: i64) -> Vec<Value> { (1..=n).map(|i| bar(i * 60, 0.0)).collect() }
fn scope() -> Value { json!({ "exchange": "binance", "market": "spot", "symbol": "BTCUSDT", "tf": "1m", "mode": "source" }) }
fn call(c: &mut Coordinator, v: Value) -> Value { ffi::dispatch(c, &v).unwrap() }
fn demand(c: &mut Coordinator, rev: u64, hrev: u64, b: &[Value]) -> Value {
    call(c, json!({ "cmd": "demand", "scope": scope(), "scope_key": "s", "revision": rev, "history_revision": hrev, "bars": b, "want_from_index": 0, "want_to_index": b.len() }))
}
fn next(c: &mut Coordinator) -> Value { call(c, json!({ "cmd": "next", "plugin": "a" })) }
fn done(c: &mut Coordinator, tok: &Value, err: Option<&str>, desc: Option<Value>) -> Value {
    let mut v = json!({ "cmd": "done", "plugin": "a", "token": tok });
    if let Some(e) = err { v["error"] = json!(e) }
    if let Some(d) = desc { v["descriptor"] = d } else if err.is_none() { v["output"] = json!({ "times": [], "series": {} }) }
    call(c, v)
}
fn done_out(c: &mut Coordinator, tok: &Value, out: Value) -> Value {
    call(c, json!({ "cmd": "done", "plugin": "a", "token": tok, "output": out }))
}
fn line(times: &[i64], vals: Vec<Value>) -> Value { json!({ "times": times, "series": { "x": { "type": "line", "values": vals } } }) }
fn view<'a>(r: &'a Value) -> &'a Value { &evs(r, "result")[0]["view"] }
fn evs<'a>(r: &'a Value, kind: &str) -> Vec<&'a Value> { r["events"].as_array().unwrap().iter().filter(|e| e["ev"] == kind).collect() }
fn loaded(live: bool) -> Coordinator {
    let mut c = Coordinator::new();
    call(&mut c, json!({ "cmd": "plugins", "list": [{ "id": "a", "load_key": "k" }] }));
    demand(&mut c, 1, 1, &bars(300));
    let l = next(&mut c);
    assert_eq!(l["act"], "load");
    let r = done(&mut c, &l["token"], None, Some(json!({ "live": live, "context": { "bars": 2 } })));
    assert_eq!(r["more"], true);
    c
}

#[test]
fn history_then_live_init_then_bounded_update() {
    let mut c = loaded(true);
    let h = next(&mut c);
    assert_eq!((h["act"].as_str(), h["op"].as_u64()), (Some("compute"), Some(1)));
    let r = done(&mut c, &h["token"], None, None);
    assert_eq!(evs(&r, "result")[0]["kind"], "history");
    let i = next(&mut c);
    assert_eq!(i["op"], 2);
    let body: Value = serde_json::from_str(i["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["bars"].as_array().unwrap().len(), 2 + 1 + 64, "bounded window, not whole cache");
    done(&mut c, &i["token"], None, None);
    assert_eq!(next(&mut c)["act"], "idle");
    // ordinary tick via tail patch: replace forming bar + append one
    let r = call(&mut c, json!({ "cmd": "demand", "scope": scope(), "scope_key": "s", "revision": 2, "history_revision": 1,
        "tail": [bar(300 * 60, 9.0), bar(301 * 60, 0.0)], "len": 301, "want_from_index": 0, "want_to_index": 301 }));
    assert_eq!(r["need_full"], false);
    let u = next(&mut c);
    assert_eq!(u["op"], 3);
    let body: Value = serde_json::from_str(u["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["ops"].as_array().unwrap().len(), 2);
}

#[test]
fn tail_that_cannot_apply_requests_full_cache() {
    let mut c = loaded(true);
    // gap: tail starts after mirror end + more bars than len accounts for
    let r = call(&mut c, json!({ "cmd": "demand", "scope": scope(), "scope_key": "s", "revision": 2, "history_revision": 1,
        "tail": [bar(400 * 60, 0.0)], "len": 400 }));
    assert_eq!(r["need_full"], true);
}

#[test]
fn history_on_older_basis_is_discarded_and_recomputed() {
    let mut c = loaded(false);
    let h = next(&mut c);
    demand(&mut c, 2, 2, &bars(310)); // prepend/correction mid-compute
    let r = done(&mut c, &h["token"], None, None);
    assert!(evs(&r, "result").is_empty(), "older basis never admitted");
    assert_eq!(r["more"], true);
    assert_eq!(next(&mut c)["op"], 1);
}

#[test]
fn older_revision_live_result_carries_stale_from() {
    let mut c = loaded(true);
    let h = next(&mut c); done(&mut c, &h["token"], None, None);
    let i = next(&mut c);
    let mut b = bars(300);
    b[299] = bar(300 * 60, 5.0);
    demand(&mut c, 2, 1, &b); // tail replaced while LiveInit in flight
    let r = done(&mut c, &i["token"], None, None);
    let res = evs(&r, "result");
    assert_eq!(res[0]["stale_from"], 300 * 60, "values at the changed bar are never painted");
}

#[test]
fn retired_incarnation_failure_cannot_fault_new_one() {
    let mut c = loaded(false);
    let h = next(&mut c);
    let r = call(&mut c, json!({ "cmd": "plugins", "list": [{ "id": "a", "load_key": "k2" }] }));
    assert_eq!(evs(&r, "disable").len(), 1);
    let r = done(&mut c, &h["token"], Some("trap"), None);
    assert!(evs(&r, "status").is_empty());
    assert_eq!(call(&mut c, json!({ "cmd": "inspect", "plugin": "a" }))["fault"], Value::Null);
    assert_eq!(next(&mut c)["act"], "load");
}

#[test]
fn fault_stops_without_restart_until_retry() {
    let mut c = loaded(false);
    let h = next(&mut c);
    let r = done(&mut c, &h["token"], Some("trap"), None);
    assert_eq!(evs(&r, "status")[0]["state"], "fault");
    assert_eq!(evs(&r, "disable").len(), 1);
    assert_eq!(next(&mut c)["act"], "idle");
    call(&mut c, json!({ "cmd": "clear_fault", "plugin": "a" }));
    call(&mut c, json!({ "cmd": "plugins", "list": [{ "id": "a", "load_key": "k" }] }));
    assert_eq!(next(&mut c)["act"], "load");
}

#[test]
fn rejected_live_basis_is_bounded_then_stalled() {
    let mut c = loaded(true);
    let h = next(&mut c); done(&mut c, &h["token"], None, None);
    let i = next(&mut c);
    assert_eq!(done(&mut c, &i["token"], Some("sequence_gap"), None)["more"], true);
    let i = next(&mut c);
    assert_eq!(i["op"], 2, "explicit reinit");
    let r = done(&mut c, &i["token"], Some("sequence_gap"), None);
    assert_eq!(evs(&r, "status")[0]["state"], "stalled");
    assert_eq!(next(&mut c)["act"], "idle", "no spin");
    assert_eq!(call(&mut c, json!({ "cmd": "pending", "plugin": "a" })), false);
}

#[test]
fn invalid_window_fails_closed() {
    let mut c = loaded(false);
    call(&mut c, json!({ "cmd": "demand", "scope": scope(), "scope_key": "s", "revision": 2, "history_revision": 2, "bars": bars(300),
        "want_from_index": 0, "want_to_index": 300, "invalid_times": [150 * 60] }));
    let r = next(&mut c);
    assert_eq!(evs(&r, "invalid").len(), 1);
    assert_eq!(evs(&r, "status")[0]["code"], "invalid_input_window");
    assert_eq!(next(&mut c)["act"], "idle");
}

// ---- P1: stale-basis failure boundary -------------------------------------

#[test]
fn obsolete_history_basis_failure_reloads_instead_of_faulting() {
    let mut c = loaded(false);
    let h = next(&mut c);
    demand(&mut c, 2, 2, &bars(310)); // same scope, history basis advanced
    let r = done(&mut c, &h["token"], Some("trap"), None);
    assert!(evs(&r, "status").iter().all(|e| e["state"] != "fault"), "stale trap has no authority: {r}");
    assert_eq!(evs(&r, "disable").len(), 1, "dead Worker released, not assumed loaded");
    assert_eq!(r["more"], true, "progress on the quiet new basis without another tick");
    assert_eq!(call(&mut c, json!({ "cmd": "inspect", "plugin": "a" }))["fault"], Value::Null);
    assert_eq!(next(&mut c)["act"], "load", "reload on the current demand");
}

#[test]
fn obsolete_basis_reload_then_computes_current_basis() {
    let mut c = loaded(false);
    let h = next(&mut c);
    demand(&mut c, 2, 2, &bars(310));
    done(&mut c, &h["token"], Some("timeout"), None);
    let l = next(&mut c);
    assert_eq!(l["act"], "load");
    done(&mut c, &l["token"], None, Some(json!({ "live": false, "context": { "bars": 2 } })));
    let h2 = next(&mut c);
    assert_eq!((h2["act"].as_str(), h2["revision"].as_u64()), (Some("compute"), Some(2)));
    // A failure on the CURRENT basis still faults (no masking).
    let r = done(&mut c, &h2["token"], Some("trap"), None);
    assert_eq!(evs(&r, "status")[0]["state"], "fault");
}

#[test]
fn live_data_revision_advance_does_not_mask_a_fault() {
    let mut c = loaded(true);
    let h = next(&mut c); done(&mut c, &h["token"], None, None);
    let i = next(&mut c);
    let mut b = bars(301); b[300] = bar(301 * 60, 0.0);
    demand(&mut c, 2, 1, &b); // only the live data revision moved
    let r = done(&mut c, &i["token"], Some("timeout"), None);
    assert_eq!(evs(&r, "status")[0]["state"], "fault", "busy feed must not suppress real faults");
}

#[test]
fn load_failure_faults_even_if_basis_changed() {
    let mut c = Coordinator::new();
    call(&mut c, json!({ "cmd": "plugins", "list": [{ "id": "a", "load_key": "k" }] }));
    demand(&mut c, 1, 1, &bars(300));
    let l = next(&mut c);
    demand(&mut c, 2, 2, &bars(310));
    let r = done(&mut c, &l["token"], Some("abi_mismatch"), None);
    assert_eq!(evs(&r, "status")[0]["state"], "fault");
}

#[test]
fn delayed_old_basis_success_is_not_admitted() {
    let mut c = loaded(false);
    let h = next(&mut c);
    demand(&mut c, 2, 2, &bars(310));
    let r = done_out(&mut c, &h["token"], line(&[60], vec![json!(1)]));
    assert!(evs(&r, "result").is_empty());
    assert!(c.sched_values_json("a").as_object().unwrap().is_empty(), "no stale values accepted");
}

// ---- accepted-value authority ----------------------------------------------

#[test]
fn history_values_partial_range_and_null_vs_absent() {
    let mut c = loaded(false);
    let h = next(&mut c);
    let r = done_out(&mut c, &h["token"], line(&[60, 120, 180], vec![json!(0), Value::Null, json!(2.5)]));
    let v = view(&r);
    assert_eq!(v["shape"], true);
    assert_eq!(v["cols"]["a/x|source"]["set"], json!([[60, 0], [120, null], [180, 2.5]]), "zero and null are values");
    // unchanged recompute = empty diff
    demand(&mut c, 2, 2, &bars(300));
    let h = next(&mut c);
    let r = done_out(&mut c, &h["token"], line(&[60, 120, 180], vec![json!(0), Value::Null, json!(2.5)]));
    assert_eq!(view(&r)["cols"], json!({}));
    assert_eq!(view(&r)["changed"], json!([]));
    // missing series => column dropped (shape change)
    demand(&mut c, 3, 3, &bars(300));
    let h = next(&mut c);
    let r = done_out(&mut c, &h["token"], json!({ "times": [60], "series": {} }));
    assert_eq!(view(&r)["drop"], json!(["a/x|source"]));
}

#[test]
fn live_overlay_has_precedence_over_stateless_history() {
    let mut c = loaded(true);
    let h = next(&mut c);
    done_out(&mut c, &h["token"], line(&[17940, 18000], vec![json!(1), json!(1)]));
    let i = next(&mut c);
    let r = done_out(&mut c, &i["token"], line(&[18000], vec![json!(9)]));
    assert_eq!(view(&r)["cols"]["a/x|source"]["set"], json!([[18000, 9]]));
    assert_eq!(view(&r)["changed"], json!([18000]));
    // History recompute on the SAME basis keeps the live value.
    call(&mut c, json!({ "cmd": "reset", "plugin": "a" }));
    let vals = c.sched_values_json("a");
    assert_eq!(vals["a/x|source"]["18000"], 9);
}

#[test]
fn stale_cutoff_and_invalidation_and_fault_drop() {
    let mut c = loaded(true);
    let h = next(&mut c);
    done_out(&mut c, &h["token"], line(&[17880, 17940, 18000], vec![json!(1), json!(2), json!(3)]));
    let i = next(&mut c);
    let mut b = bars(300); b[299] = bar(300 * 60, 5.0);
    demand(&mut c, 2, 1, &b);
    let r = done_out(&mut c, &i["token"], line(&[17940, 18000], vec![json!(7), json!(8)]));
    assert_eq!(view(&r)["cols"]["a/x|source"]["set"], json!([[17940, 7]]), "value at the stale bar never accepted");
    let inv = call(&mut c, json!({ "cmd": "invalidate", "times": [17940] }));
    assert_eq!(inv["cols"]["a/x|source"]["del"], json!([17940, 18000]), "enabled plugin withholds at/after earliest bucket");
    let f = call(&mut c, json!({ "cmd": "presentation_fault", "plugin": "a" }));
    assert_eq!(evs(&f, "view")[0]["view"]["drop"], json!(["a/x|source"]));
    assert_eq!(evs(&f, "status")[0]["code"], "presentation_error");
}

#[test]
fn unparseable_output_is_invalid_output_not_accepted() {
    let mut c = loaded(false);
    let h = next(&mut c);
    let r = done_out(&mut c, &h["token"], json!("garbage"));
    assert_eq!(evs(&r, "status")[0]["code"], "invalid_output");
}

#[test]
fn series_value_at_semantics() {
    use cvd_browser_coordinator::sched::series_value_at as v;
    assert_eq!(v(&json!({ "type": "line", "values": [0, null, 3] }), 0), Some(json!(0)), "true zero kept");
    assert_eq!(v(&json!({ "type": "line", "values": [0, null, 3] }), 1), Some(Value::Null));
    assert_eq!(v(&json!({ "type": "histogram", "values": [-0.0] }), 0).unwrap().as_f64().unwrap().to_bits(), (-0.0f64).to_bits(), "-0 kept");
    assert_eq!(v(&json!({ "type": "line", "values": [1] }), 5), Some(Value::Null), "past the column is a gap");
    assert_eq!(v(&json!({ "type": "line", "values": ["x"] }), 0), Some(Value::Null), "non-number -> null");
    assert_eq!(v(&json!({ "type": "nope", "values": [1] }), 0), None, "unknown kind undefined");
    assert_eq!(v(&Value::Null, 0), None);
    assert_eq!(v(&json!({ "type": "dual_range", "upper": [0, 2], "lower": [0, null] }), 0), Some(json!({ "upper": 0, "lower": 0 })));
    assert_eq!(v(&json!({ "type": "dual_range", "upper": [0, 2], "lower": [0, null] }), 1), Some(Value::Null));
    assert_eq!(v(&json!({ "type": "segmented_lines", "layers": [[0, 1], [null, 2], "x"] }), 0), Some(json!([0, null, null])));
    assert_eq!(v(&json!({ "type": "segmented_lines", "layers": null }), 0), Some(Value::Null));
}

#[test]
fn invalidation_of_non_member_column_drops_only_the_bucket() {
    let mut c = loaded(false);
    let h = next(&mut c);
    done_out(&mut c, &h["token"], line(&[60, 120, 180], vec![json!(1), json!(2), json!(3)]));
    // plugin leaves membership: its values are dropped (no stale reuse on re-enable)
    let r = call(&mut c, json!({ "cmd": "plugins", "list": [] }));
    assert_eq!(evs(&r, "view")[0]["view"]["drop"], json!(["a/x|source"]));
    let inv = call(&mut c, json!({ "cmd": "invalidate", "times": [120] }));
    assert_eq!(inv["cols"], json!({}));
}

#[test]
fn scope_change_clears_and_drop_values_is_explicit() {
    let mut c = loaded(false);
    let h = next(&mut c);
    done_out(&mut c, &h["token"], line(&[60], vec![json!(1)]));
    let mut c2 = loaded(false);
    let h2 = next(&mut c2);
    done_out(&mut c2, &h2["token"], line(&[60], vec![json!(1)]));
    let r = call(&mut c2, json!({ "cmd": "drop_values", "plugin": "a" }));
    assert_eq!(r["view"]["drop"], json!(["a/x|source"]));
    let mut s2 = scope(); s2["tf"] = json!("5m");
    let r = call(&mut c, json!({ "cmd": "demand", "scope": s2, "scope_key": "s2", "revision": 9, "history_revision": 9, "bars": bars(10), "want_from_index": 0, "want_to_index": 10 }));
    assert_eq!(r["view"]["clear"], true);
}
