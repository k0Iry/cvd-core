//! wasm32 FFI: JSON command in, JSON result in a slot (status 0 ok / 1 err).
//! `coord_alloc(n)`, `coord_free(p,n)`, `coord_call(p,n) -> status`,
//! `coord_result_ptr()`, `coord_result_len()`. Commands:
//! {cmd:"history", scope, revision, bars, context, want_from_index, want_to_index, exhausted}
//! {cmd:"live", plugin, scope_key, scope, context, tail}
//! Optional `invalid_times: [i64]` on history/live (fail-closed validity).
//! {cmd:"done", plugin, token, error?, descriptor?} + optional "\n" + raw output frame
//! {cmd:"invalidate", times}  {cmd:"presentation_fault", plugin}
//! {cmd:"approve", descriptor, params, strict} -> {descriptor, params}
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
        "plugins" => {
            let list = arr(cmd, "list")?.iter().map(|p| {
                let id = p.get("id").and_then(Value::as_str).ok_or_else(|| invalid("plugin id"))?.to_string();
                let key = p.get("load_key").and_then(Value::as_str).unwrap_or("").to_string();
                Ok((id, key))
            }).collect::<Result<Vec<_>, CoordError>>()?;
            Ok(serde_json::json!({ "events": c.sched_plugins(&list) }))
        }
        "clear_fault" => { c.sched_clear_fault(s("plugin")?); Ok(Value::Null) }
        "demand" => {
            let older = match cmd.get("older_ranges") {
                Some(Value::Array(a)) => a.iter().map(|r| Ok(sched::OlderRange {
                    from_index: usize_of(r, "from_index")?, to_index: usize_of(r, "to_index")?,
                    from: field(r, "from")?.as_i64().ok_or_else(|| invalid("older from"))?,
                    to: field(r, "to")?.as_i64().ok_or_else(|| invalid("older to"))?,
                })).collect::<Result<Vec<_>, CoordError>>()?,
                _ => Vec::new(),
            };
            let d = sched::Demand {
                scope: scope_of(cmd)?, scope_key: s("scope_key")?.to_string(),
                revision: field(cmd, "revision")?.as_u64().unwrap_or(0),
                history_revision: field(cmd, "history_revision")?.as_u64().unwrap_or(0),
                exhausted: cmd.get("exhausted").and_then(Value::as_bool).unwrap_or(false),
                want_from_index: cmd.get("want_from_index").and_then(Value::as_u64).unwrap_or(0) as usize,
                want_to_index: cmd.get("want_to_index").and_then(Value::as_u64).map(|x| x as usize),
                older, invalid_times: invalid_of(cmd)?,
            };
            let bars = match cmd.get("bars") { Some(Value::Array(a)) => Some(a.clone()), _ => None };
            let tail = match cmd.get("tail") { Some(Value::Array(a)) => Some((a.clone(), usize_of(cmd, "len")?)), _ => None };
            Ok(c.sched_demand(d, bars, tail))
        }
        "superseded" => { c.sched_superseded(s("plugin")?); Ok(Value::Null) }
        "next" => Ok(c.sched_next(s("plugin")?)),
        "done" => Ok(c.sched_done(s("plugin")?, usize_of(cmd, "token")? as u64, cmd.get("error").and_then(Value::as_str), cmd.get("descriptor"), cmd.get("output"))),
        "invalidate" => {
            let times = arr(cmd, "times")?.iter().map(|t| t.as_i64().ok_or_else(|| invalid("invalidate time not an integer"))).collect::<Result<Vec<_>, _>>()?;
            Ok(c.sched_invalidate(&times))
        }
        "drop_values" => Ok(c.sched_drop(s("plugin")?)),
        "presentation_fault" => Ok(c.sched_presentation_fault(s("plugin")?)),
        "pending" => Ok(Value::Bool(c.sched_pending(s("plugin")?))),
        "idle" => Ok(Value::Bool(c.sched_idle())),
        "inspect" => Ok(c.sched_inspect(s("plugin")?)),
        "running" => Ok(serde_json::json!(c.sched_running())),
        "stop" => Ok(serde_json::json!({ "events": c.sched_stop() })),
        "approve" => approve(field(cmd, "descriptor")?, cmd.get("params").unwrap_or(&Value::Null), cmd.get("strict").and_then(Value::as_bool).unwrap_or(false)),
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
    // `done` may append the Worker's raw output frame after one '\n'
    // (JSON.stringify never emits a raw newline): parsed exactly once here,
    // never re-encoded inside the command. Unparseable output = invalid_output.
    let (head, extra) = match input.iter().position(|&b| b == b'\n') { Some(i) => (&input[..i], Some(&input[i + 1..])), None => (input, None) };
    let parsed = serde_json::from_slice::<Value>(head).map(|mut cmd| {
        if let Some(raw) = extra {
            match serde_json::from_slice::<Value>(raw) { Ok(o) => cmd["output"] = o, Err(_) => cmd["error"] = Value::from("invalid_output") }
        }
        cmd
    });
    let (status, out) = match parsed {
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
