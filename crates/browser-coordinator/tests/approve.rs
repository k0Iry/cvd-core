//! Metadata/params approval: SDK `Descriptor::validate` + `resolve_params`
//! through the coordinator FFI dispatcher (the exact path the page uses).
use cvd_browser_coordinator::{ffi::dispatch, Coordinator};
use serde_json::{json, Value};

fn desc() -> Value {
    json!({"id":"p.x","version":"1.0.0","name":"X","abi":1,"context":{"bars":0},
      "params":[{"key":"n","type":"int","default":5,"min":0,"max":10},
                {"key":"f","type":"float","default":0.5,"min":0.0,"max":1.0},
                {"key":"b","type":"bool","default":true},
                {"key":"m","type":"choice","default":"a","options":["a","b"]}],
      "outputs":[{"key":"v","pane":"separate","type":"line","colors":["red"]}]})
}
fn approve(d: Value, p: Value, strict: bool) -> Result<Value, cvd_browser_coordinator::CoordError> {
    dispatch(&mut Coordinator::new(), &json!({"cmd":"approve","descriptor":d,"params":p,"strict":strict}))
}

#[test]
fn restore_discards_unknown_and_defaults_each_invalid_field() {
    let r = approve(desc(), json!({"n":99,"f":0,"b":false,"m":"b","evil":1}), false).unwrap();
    // zero / false preserved; out-of-range n defaulted; unknown dropped.
    assert_eq!(r["params"], json!({"n":5,"f":0.0,"b":false,"m":"b"}));
    let r = approve(desc(), json!({"n":"7","f":2.0,"b":"true","m":"zzz"}), false).unwrap();
    assert_eq!(r["params"], json!({"n":5,"f":0.5,"b":true,"m":"a"}));
    assert_eq!(approve(desc(), Value::Null, false).unwrap()["params"], json!({"n":5,"f":0.5,"b":true,"m":"a"}));
    // JS-unsafe integer never approved (would not survive the page round trip).
    let mut d = desc(); d["params"][0]["max"] = json!(i64::MAX);
    assert_eq!(approve(d, json!({"n": 1i64 << 60}), false).unwrap()["params"]["n"], json!(5));
}

#[test]
fn strict_edit_is_the_sdk_resolver() {
    assert_eq!(approve(desc(), json!({"n":0,"b":false}), true).unwrap()["params"], json!({"n":0,"f":0.5,"b":false,"m":"a"}));
    for bad in [json!({"n":11}), json!({"n":1.5}), json!({"n":null}), json!({"f":"x"}), json!({"m":"c"}), json!({"evil":1}), json!([1])] {
        assert_eq!(approve(desc(), bad.clone(), true).unwrap_err().code, "invalid_params", "{bad}");
    }
    let mut d = desc(); d["params"][0]["max"] = json!(i64::MAX);
    assert_eq!(approve(d.clone(), json!({"n": (1i64 << 53) - 1}), true).unwrap()["params"]["n"], json!((1i64 << 53) - 1));
    assert_eq!(approve(d, json!({"n": 1i64 << 53}), true).unwrap_err().code, "invalid_params");
}

#[test]
fn corrupt_descriptor_fails_closed() {
    let cases: Vec<Box<dyn Fn(&mut Value)>> = vec![
        Box::new(|d| d["id"] = json!("Bad Id")),
        Box::new(|d| d["abi"] = json!(2)),
        Box::new(|d| d["outputs"] = json!([])),
        Box::new(|d| d["outputs"][0]["type"] = json!("bogus")),
        Box::new(|d| d["outputs"][0]["pane"] = json!("left")),
        Box::new(|d| { d["outputs"][0]["type"] = json!("histogram"); d["outputs"][0]["readout_only"] = json!(true) }),
        Box::new(|d| d["outputs"][0]["widths"] = json!([0])),
        Box::new(|d| d["params"][1]["key"] = json!("n")),
        Box::new(|d| d["params"][0]["type"] = json!("text")),
        Box::new(|d| { d.as_object_mut().unwrap().remove("context"); }),
        Box::new(|d| *d = json!("not an object")),
    ];
    for (i, f) in cases.iter().enumerate() {
        let mut d = desc(); f(&mut d);
        assert_eq!(approve(d, json!({}), false).unwrap_err().code, "invalid_descriptor", "case {i}");
    }
    // Approved descriptor is the SDK's canonical form (defaults filled).
    let r = approve(desc(), json!({}), false).unwrap();
    assert_eq!((r["descriptor"]["live"].clone(), r["descriptor"]["outputs"][0]["readout_only"].clone()), (json!(true), json!(false)));
}

#[test]
fn fallback_default_goes_through_the_sdk_and_final_ints_are_js_safe() {
    let with = |i: usize, f: &str, v: Value| { let mut d = desc(); d["params"][i][f] = v; d };
    // Invalid default rejects whenever the fallback is used, in both modes.
    for p in [json!({}), json!({"n": 50}), Value::Null] {
        for strict in [false, true] {
            assert_eq!(approve(with(0, "default", json!(99)), p.clone(), strict).unwrap_err().code, "invalid_params", "{p} {strict}");
        }
    }
    assert_eq!(approve(with(3, "default", json!("zz")), json!({"m":"q"}), false).unwrap_err().code, "invalid_params");
    assert_eq!(approve(with(3, "default", json!("zz")), json!({}), false).unwrap_err().code, "invalid_params");
    // A valid supplied value does not need the (invalid) default.
    assert_eq!(approve(with(0, "default", json!(99)), json!({"n": 3}), false).unwrap()["params"]["n"], json!(3));
    // Unsafe positive / negative defaults reject in both modes.
    for (dv, min, max) in [(json!(1i64 << 53), json!(0), json!(i64::MAX)), (json!(-(1i64 << 53)), json!(i64::MIN), json!(0))] {
        let mut d = desc(); d["params"][0]["default"] = dv; d["params"][0]["min"] = min; d["params"][0]["max"] = max;
        for strict in [false, true] { assert_eq!(approve(d.clone(), json!({}), strict).unwrap_err().code, "invalid_params"); }
    }
    // i64::MIN never panics nor is approved.
    let mut d = desc(); d["params"][0]["min"] = json!(i64::MIN);
    assert_eq!(approve(d.clone(), json!({"n": i64::MIN}), false).unwrap()["params"]["n"], json!(5));
    assert_eq!(approve(d.clone(), json!({"n": i64::MIN}), true).unwrap_err().code, "invalid_params");
    d["params"][0]["default"] = json!(i64::MIN);
    for strict in [false, true] { assert_eq!(approve(d.clone(), json!({}), strict).unwrap_err().code, "invalid_params"); }
    // Valid defaults / zero / false / overrides unchanged.
    assert_eq!(approve(desc(), json!({"n":0,"b":false,"m":"b"}), false).unwrap()["params"], json!({"n":0,"f":0.5,"b":false,"m":"b"}));
}
