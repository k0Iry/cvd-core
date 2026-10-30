//! wasm32 FFI: JSON command in, JSON result in a slot (status 0 ok / 1 err).
//! `coord_alloc(n)`, `coord_free(p,n)`, `coord_call(p,n) -> status`,
//! `coord_result_ptr()`, `coord_result_len()`. Commands:
//! {cmd:"history", scope, revision, bars, context, want_from_index, want_to_index, exhausted}
//! {cmd:"live", plugin, scope_key, scope, context, tail}
//! Optional `invalid_times: [i64]` on history/live (fail-closed validity).
//! {cmd:"commit", plugin, ok}  {cmd:"reset", plugin}  {cmd:"map", bars}
use super::*;
use std::cell::RefCell;

thread_local! {
    static COORD: RefCell<Coordinator> = RefCell::new(Coordinator::new());
    static RESULT: RefCell<Vec<u8>> = RefCell::new(Vec::new());
}

fn field<'a>(v: &'a Value, k: &str) -> Result<&'a Value, CoordError> {
    v.get(k).ok_or_else(|| CoordError { code: "bad_command", message: format!("missing {k}") })
}
fn scope_of(v: &Value) -> Result<Scope, CoordError> {
    serde_json::from_value(field(v, "scope")?.clone()).map_err(|e| invalid(format!("scope: {e}")))
}
fn usize_of(v: &Value, k: &str) -> Result<usize, CoordError> {
    field(v, k)?.as_u64().map(|x| x as usize).ok_or_else(|| invalid(format!("{k} not an integer")))
}
fn arr<'a>(v: &'a Value, k: &str) -> Result<&'a Vec<Value>, CoordError> {
    field(v, k)?.as_array().ok_or_else(|| invalid(format!("{k} not an array")))
}

fn invalid_of(v: &Value) -> Result<Vec<i64>, CoordError> {
    match v.get("invalid_times") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a.iter().map(|t| t.as_i64().ok_or_else(|| invalid("invalid_times entry not an integer"))).collect(),
        Some(_) => Err(invalid("invalid_times not an array")),
    }
}

/// Pure dispatcher (also used by native tests).
pub fn dispatch(c: &mut Coordinator, cmd: &Value) -> Result<Value, CoordError> {
    let s = |k| field(cmd, k).and_then(|v| v.as_str().ok_or_else(|| invalid(format!("{k} not a string"))));
    match s("cmd")? {
        "history" => {
            let p = plan_history_checked(&scope_of(cmd)?, field(cmd, "revision")?.as_u64().unwrap_or(0), arr(cmd, "bars")?, usize_of(cmd, "context")?,
                usize_of(cmd, "want_from_index")?, usize_of(cmd, "want_to_index")?, field(cmd, "exhausted")?.as_bool().unwrap_or(false), &invalid_of(cmd)?)?;
            Ok(serde_json::to_value(p).unwrap())
        }
        "live" => {
            let p = c.plan_live_checked(s("plugin")?, s("scope_key")?, &scope_of(cmd)?, usize_of(cmd, "context")?, arr(cmd, "tail")?, &invalid_of(cmd)?)?;
            Ok(serde_json::to_value(p).unwrap())
        }
        "commit" => { c.commit(s("plugin")?, field(cmd, "ok")?.as_bool().unwrap_or(false)); Ok(Value::Null) }
        "reset" => { c.reset(s("plugin")?); Ok(Value::Null) }
        "map" => Ok(serde_json::to_value(input_bars(arr(cmd, "bars")?)?).unwrap()),
        _ => Err(CoordError { code: "bad_command", message: "unknown cmd".into() }),
    }
}

#[no_mangle]
pub extern "C" fn coord_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}
/// # Safety: `p`/`n` must come from `coord_alloc`.
#[no_mangle]
pub unsafe extern "C" fn coord_free(p: *mut u8, n: usize) { drop(Vec::from_raw_parts(p, 0, n.max(1))) }
/// # Safety: `p..p+n` must be a live region written by the caller.
#[no_mangle]
pub unsafe extern "C" fn coord_call(p: *const u8, n: usize) -> u32 {
    let input = std::slice::from_raw_parts(p, n);
    let (status, out) = match serde_json::from_slice::<Value>(input) {
        Err(_) => (1, serde_json::json!({ "code": "bad_command", "message": "malformed json" })),
        Ok(cmd) => match COORD.with(|c| dispatch(&mut c.borrow_mut(), &cmd)) {
            Ok(v) => (0, v),
            Err(e) => (1, serde_json::to_value(e).unwrap()),
        },
    };
    RESULT.with(|r| *r.borrow_mut() = serde_json::to_vec(&out).unwrap());
    status
}
#[no_mangle]
pub extern "C" fn coord_result_ptr() -> *const u8 { RESULT.with(|r| r.borrow().as_ptr()) }
#[no_mangle]
pub extern "C" fn coord_result_len() -> usize { RESULT.with(|r| r.borrow().len()) }
