//! Book admission policy through the real FFI dispatcher.
use crate::ffi::dispatch;
use crate::Coordinator;
use serde_json::{json, Value};

fn call(c: &mut Coordinator, v: Value) -> Value {
    dispatch(c, &v).unwrap()
}
fn setup() -> Coordinator {
    let mut c = Coordinator::new();
    call(&mut c, json!({"cmd":"book_plugins","list":[{"id":"ob","load_key":"k1"}]}));
    call(&mut c, json!({"cmd":"book_source","source":"binance/usdm/BTC"}));
    let r = call(&mut c, json!({"cmd":"book_loaded","plugin":"ob","load_key":"k1"}));
    assert_eq!(r["actions"], json!([]), "no frame yet: no work");
    c
}
fn frame(c: &mut Coordinator, generation: u64, seq: u64, t: i64) -> Value {
    call(c, json!({"cmd":"book_frame","source":"binance/usdm/BTC","generation":generation,"seq":seq,"received_ms":t}))
}
fn done(c: &mut Coordinator, token: &Value, error: Option<&str>) -> Value {
    call(c, json!({"cmd":"book_done","plugin":"ob","token":token,"error":error}))
}
fn view(c: &mut Coordinator, now: i64) -> Value {
    call(c, json!({"cmd":"book_view","now_ms":now}))["plugins"][0].clone()
}

/// Feed faster than compute: output keeps advancing with bounded lag; each
/// accepted result carries the seq it was computed from; seq never regresses.
#[test]
fn faster_feed_does_not_starve_and_basis_is_truthful() {
    let mut c = setup();
    let mut tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
    let mut accepted = Vec::new();
    for k in 1..=20u64 {
        // Two frames arrive during each compute.
        assert_eq!(frame(&mut c, 1, 10 + 2 * k - 1, 1_000 + 100 * k as i64)["actions"], json!([]));
        assert_eq!(frame(&mut c, 1, 10 + 2 * k, 1_050 + 100 * k as i64)["actions"], json!([]));
        let r = done(&mut c, &tok, None);
        assert_eq!(r["admit"], true);
        let v = view(&mut c, 1_060 + 100 * k as i64);
        accepted.push(v["seq"].as_u64().unwrap());
        assert_eq!(v["state"], "updating", "a newer frame is being computed");
        tok = r["actions"][0]["token"].clone();
        assert_eq!(r["actions"][0]["seq"], 10 + 2 * k, "next compute uses the latest frame");
    }
    assert_eq!(accepted.len(), 20);
    assert!(accepted.windows(2).all(|w| w[0] < w[1]), "{accepted:?}");
    assert_eq!(accepted[0], 10);
    assert_eq!(*accepted.last().unwrap(), 10 + 2 * 19, "lag bounded to one compute");
}

/// Raw frames do not refresh an old result: age follows the computed basis.
#[test]
fn expired_result_is_not_refreshed_by_raw_frames() {
    let mut c = setup();
    let tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
    // Compute hangs; raw frames keep arriving.
    for k in 1..50 {
        frame(&mut c, 1, 10 + k, 1_000 + 100 * k as i64);
    }
    assert_eq!(view(&mut c, 6_500)["state"], "waiting");
    let r = done(&mut c, &tok, None);
    assert_eq!(view(&mut c, 6_000)["state"], "updating", "at the bound: not yet stale");
    let v = view(&mut c, 6_500);
    assert_eq!((v["state"].as_str(), v["seq"].as_u64(), v["age_ms"].as_i64()), (Some("stale"), Some(10), Some(5_500)));
    // More raw frames still do not refresh it.
    frame(&mut c, 1, 100, 6_500);
    assert_eq!(view(&mut c, 6_500)["seq"], 10);
    // The already-planned next compute uses the latest frame seen at completion.
    assert_eq!(r["actions"][0]["seq"], 59);
    let r = done(&mut c, &r["actions"][0]["token"].clone(), None);
    let v = view(&mut c, 6_000);
    assert_eq!((v["state"].as_str(), v["seq"].as_u64()), (Some("updating"), Some(59)));
    done(&mut c, &r["actions"][0]["token"].clone(), None);
    let v = view(&mut c, 6_100);
    assert_eq!((v["state"].as_str(), v["seq"].as_u64()), (Some("live"), Some(100)));
}

/// Late success AND late error during source switch / resync / reconnect are fenced.
#[test]
fn late_completions_fenced_by_basis() {
    for kind in ["source", "withdraw", "resync"] {
        for err in [None, Some("plugin_error"), Some("worker_crashed")] {
            let mut c = setup();
            let tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
            match kind {
                "source" => {
                    call(&mut c, json!({"cmd":"book_source","source":"binance/usdm/ETH"}));
                }
                "withdraw" => {
                    call(&mut c, json!({"cmd":"book_withdraw"}));
                }
                _ => {
                    frame(&mut c, 2, 5, 1_100);
                }
            }
            let r = done(&mut c, &tok, err);
            assert_eq!(r["admit"], false, "{kind} {err:?}");
            let v = view(&mut c, 1_200);
            assert!(v["fault"].is_null() && v["seq"].is_null(), "{kind} {err:?}: {v}");
            if kind == "resync" {
                // Pending new-generation frame is computed next.
                assert_eq!(r["actions"][0]["seq"], 5, "{kind} {err:?}");
            }
        }
    }
}

/// Disabled / replaced / reloaded identities fence old completions; stale
/// tokens never admit; params edit (reload) recovers a recoverable fault.
#[test]
fn identity_lifecycle_and_param_recovery() {
    let mut c = setup();
    let tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
    // Params edit => new load_key: old in-flight fenced.
    let r = call(&mut c, json!({"cmd":"book_plugins","list":[{"id":"ob","load_key":"k2"}]}));
    assert_eq!(r["start"], json!(["ob"]));
    assert_eq!(done(&mut c, &tok, None)["admit"], false);
    let a = call(&mut c, json!({"cmd":"book_loaded","plugin":"ob","load_key":"k2"}));
    let t2 = a["actions"][0]["token"].clone();
    // Invalid group: recoverable fault, visible; retries on newer frames.
    assert_eq!(done(&mut c, &t2, Some("plugin_error"))["admit"], true);
    assert_eq!(view(&mut c, 1_100)["state"], "fault");
    let t3 = frame(&mut c, 1, 11, 1_100)["actions"][0]["token"].clone();
    assert!(t3.is_u64(), "recoverable fault still plans newer frames");
    // Edit params again: old token fenced, new load succeeds.
    call(&mut c, json!({"cmd":"book_plugins","list":[{"id":"ob","load_key":"k3"}]}));
    assert_eq!(done(&mut c, &t3, Some("plugin_error"))["admit"], false);
    let t4 = call(&mut c, json!({"cmd":"book_loaded","plugin":"ob","load_key":"k3"}))["actions"][0]["token"].clone();
    assert_eq!(done(&mut c, &t4, None)["admit"], true);
    assert_eq!(view(&mut c, 1_200)["state"], "live");
    // Stale load ack of an old key is ignored.
    assert_eq!(call(&mut c, json!({"cmd":"book_loaded","plugin":"ob","load_key":"k2"}))["admit"], false);
    // Fatal error: not ready until reload.
    let t5 = frame(&mut c, 1, 12, 1_200)["actions"][0]["token"].clone();
    done(&mut c, &t5, Some("worker_crashed"));
    assert_eq!(frame(&mut c, 1, 13, 1_300)["actions"], json!([]));
    // Disable: identity removed, completions fenced, view empty.
    call(&mut c, json!({"cmd":"book_plugins","list":[]}));
    assert_eq!(done(&mut c, &t5, None)["admit"], false);
    assert_eq!(call(&mut c, json!({"cmd":"book_view","now_ms":0}))["plugins"], json!([]));
}

/// Frames for another source or regressing seq are not admitted.
#[test]
fn foreign_and_regressing_frames_rejected() {
    let mut c = setup();
    let r = call(&mut c, json!({"cmd":"book_frame","source":"binance/usdm/ETH","generation":1,"seq":1,"received_ms":1}));
    assert_eq!(r["admit"], false);
    frame(&mut c, 1, 10, 1_000);
    assert_eq!(frame(&mut c, 1, 9, 1_001)["admit"], false);
    assert_eq!(frame(&mut c, 1, 10, 1_001)["admit"], false);
}

fn done_pub(c: &mut Coordinator, token: &Value, error: &str, public: &str) -> Value {
    call(c, json!({"cmd":"book_done","plugin":"ob","token":token,"error":error,"public_code":public}))
}

/// Closure §2 C2-NEW-01: the plugin's allowlisted public classification
/// survives as a fixed actionable hint; arbitrary text never does; the outer
/// transport code still decides recoverability.
#[test]
fn safe_public_classification_and_fixed_hint() {
    let mut c = setup();
    let tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
    assert_eq!(done_pub(&mut c, &tok, "plugin_error", "invalid_params")["admit"], true);
    let v = view(&mut c, 1_010);
    assert_eq!((v["state"].clone(), v["fault"].clone(), v["public_code"].clone()), (json!("fault"), json!("plugin_error"), json!("invalid_params")));
    assert_eq!(v["hint"], "parameters rejected — edit parameters");
    assert_eq!(v["fatal"], false, "invalid_params stays recoverable");
    // Recoverable: the next frame is still planned for the same Worker.
    let tok = frame(&mut c, 1, 11, 1_100)["actions"][0]["token"].clone();
    assert!(tok.is_u64());
    // Non-allowlisted / arbitrary plugin text is dropped, never displayed.
    done_pub(&mut c, &tok, "plugin_error", "/home/x/secret: stack trace");
    let v = view(&mut c, 1_110);
    assert_eq!((v["public_code"].clone(), v["hint"].clone()), (Value::Null, json!("plugin_error")));
}

/// Closure §2 C2-NEW-02: a previously ACCEPTED recoverable fault is withdrawn
/// with its basis (source switch, non-live withdrawal, new generation) before
/// any new result; a fatal Worker/load fault stays visible; a parameter edit
/// (new load_key) recovers.
#[test]
fn accepted_recoverable_fault_is_basis_bound_and_fatal_stays() {
    for withdraw in ["source", "withdraw", "generation"] {
        let mut c = setup();
        let tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
        done_pub(&mut c, &tok, "plugin_error", "invalid_params");
        assert_eq!(view(&mut c, 1_010)["state"], "fault");
        match withdraw {
            "source" => {
                call(&mut c, json!({"cmd":"book_source","source":"binance/usdm/ETH"}));
            }
            "withdraw" => {
                call(&mut c, json!({"cmd":"book_withdraw"}));
            }
            _ => {
                // New exchange generation; its compute is still in flight.
                assert!(frame(&mut c, 2, 1, 1_200)["actions"][0]["token"].is_u64());
            }
        }
        let v = view(&mut c, 1_300);
        assert_eq!(v["fault"], Value::Null, "{withdraw}: old-basis fault must not label the new basis");
        assert_eq!(v["hint"], Value::Null, "{withdraw}");
        assert_eq!(v["state"], "waiting", "{withdraw}: awaiting new data, not starting/fault");
    }
    // Fatal Worker fault survives every withdrawal and is never shown as starting.
    let mut c = setup();
    let tok = frame(&mut c, 1, 10, 1_000)["actions"][0]["token"].clone();
    done(&mut c, &tok, Some("worker_crashed"));
    call(&mut c, json!({"cmd":"book_source","source":"binance/usdm/ETH"}));
    call(&mut c, json!({"cmd":"book_withdraw"}));
    let v = view(&mut c, 1_100);
    assert_eq!((v["state"].clone(), v["fault"].clone(), v["fatal"].clone()), (json!("fault"), json!("worker_crashed"), json!(true)));
    // Fatal load failure likewise.
    call(&mut c, json!({"cmd":"book_loaded","plugin":"ob","load_key":"k1","ok":false,"error":"load_failed"}));
    call(&mut c, json!({"cmd":"book_withdraw"}));
    assert_eq!(view(&mut c, 1_200)["fatal"], true);
    // Parameter edit = new load_key identity: fault gone, recovers on load.
    call(&mut c, json!({"cmd":"book_plugins","list":[{"id":"ob","load_key":"k2"}]}));
    call(&mut c, json!({"cmd":"book_loaded","plugin":"ob","load_key":"k2"}));
    call(&mut c, json!({"cmd":"book_source","source":"binance/usdm/BTC"}));
    let tok = frame(&mut c, 1, 20, 2_000)["actions"][0]["token"].clone();
    assert_eq!(done(&mut c, &tok, None)["admit"], true);
    let v = view(&mut c, 2_010);
    assert_eq!((v["state"].clone(), v["fault"].clone()), (json!("live"), Value::Null));
}
