//! Raw wasm32 exports used by the trusted JS bridge (no wasm-bindgen).
//! Every result (success JSON or `HostError` JSON) is placed in a single
//! host-owned slot read via `host_result_ptr/len`; status 0 = ok, 1 = error.

use crate::contract::{check_region, region_limit, PluginSession};
use crate::fence::{Controller, Response};
use crate::{module_check, HostError, HostLimits};
use cvd_indicator_sdk::wasm::raw;
use serde::Deserialize;
use std::sync::Mutex;

struct State {
    result: Vec<u8>,
    session: PluginSession,
    fence: Option<Controller>,
}

static STATE: Mutex<State> = Mutex::new(State { result: Vec::new(), session: PluginSession::new(), fence: None });

fn st() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

fn finish<T: serde::Serialize>(r: Result<T, HostError>) -> i32 {
    let mut s = st();
    match r {
        Ok(v) => {
            s.result = serde_json::to_vec(&v).unwrap_or_default();
            0
        }
        Err(e) => {
            s.result = serde_json::to_vec(&e).unwrap_or_default();
            1
        }
    }
}

unsafe fn input<'a>(ptr: usize, len: usize) -> &'a [u8] {
    raw::input(ptr, len).unwrap_or(&[])
}

#[no_mangle]
pub extern "C" fn host_alloc(len: usize) -> usize {
    raw::alloc_bytes(len)
}
#[no_mangle]
pub unsafe extern "C" fn host_free(ptr: usize, len: usize) {
    raw::dealloc_bytes(ptr, len)
}
#[no_mangle]
pub extern "C" fn host_result_ptr() -> usize {
    st().result.as_ptr() as usize
}
#[no_mangle]
pub extern "C" fn host_result_len() -> usize {
    st().result.len()
}
#[no_mangle]
pub extern "C" fn host_limits() -> i32 {
    finish(Ok(HostLimits::default()))
}
#[no_mangle]
pub unsafe extern "C" fn host_validate_module(ptr: usize, len: usize) -> i32 {
    finish(module_check::validate_module(input(ptr, len)))
}
#[no_mangle]
pub extern "C" fn host_session_reset() {
    st().session = PluginSession::new();
}
#[no_mangle]
pub extern "C" fn host_accept_abi(v: u32) -> i32 {
    let r = st().session.accept_abi(v);
    finish(r)
}
/// `mem_len`/`ptr`/`len` are passed as f64 so a JS Number up to 2^53 is exact.
#[no_mangle]
pub extern "C" fn host_check_region(mem_len: f64, ptr: f64, len: f64, kind: u32) -> i32 {
    let ok = |v: f64| v >= 0.0 && v.fract() == 0.0 && v <= 9.0e15;
    let r = if !(ok(mem_len) && ok(ptr) && ok(len)) {
        Err(HostError::new("invalid_output_region", "non-integral region"))
    } else {
        check_region(mem_len as u64, ptr as u64, len as u64, region_limit(kind))
    };
    if let Err(e) = &r {
        st().session.poison(&e.code);
    }
    finish(r)
}
#[no_mangle]
pub unsafe extern "C" fn host_accept_descriptor(status: i32, ptr: usize, len: usize) -> i32 {
    let r = st().session.accept_descriptor(status, input(ptr, len)).cloned();
    finish(r)
}
#[no_mangle]
pub unsafe extern "C" fn host_accept_create(status: i32, ptr: usize, len: usize) -> i32 {
    let r = st().session.accept_create(status, input(ptr, len));
    finish(r)
}
#[no_mangle]
pub unsafe extern "C" fn host_begin(op: u32, ptr: usize, len: usize) -> i32 {
    let r = st().session.begin(op, input(ptr, len));
    finish(r)
}
#[no_mangle]
pub unsafe extern "C" fn host_accept_output(status: i32, ptr: usize, len: usize) -> i32 {
    let r = st().session.accept_result(status, input(ptr, len));
    // already JSON text: store raw (not re-quoted)
    let mut s = st();
    match r {
        Ok(json) => {
            s.result = json.into_bytes();
            0
        }
        Err(e) => {
            s.result = serde_json::to_vec(&e).unwrap_or_default();
            1
        }
    }
}
/// Trap / deadline / worker fault: poison the session.
#[no_mangle]
pub unsafe extern "C" fn host_poison(ptr: usize, len: usize) -> i32 {
    let code = std::str::from_utf8(input(ptr, len))
        .ok()
        .filter(|c| matches!(*c, "trap" | "deadline" | "memory_limit"))
        .unwrap_or("trap");
    let e = st().session.poison(code);
    finish::<()>(Err(e))
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum FenceCmd {
    Start { instance: u64 },
    Retire { instance: u64 },
    Issue { instance: u64, scope_key: String, revision: u64, op: u32 },
    Supersede { instance: u64, scope_key: String, revision: u64 },
    Accept { response: Response },
    Cancel { id: u64 },
    Stats,
}

/// Main-thread fence commands (JSON in, JSON out).
#[no_mangle]
pub unsafe extern "C" fn host_fence(ptr: usize, len: usize) -> i32 {
    let cmd: FenceCmd = match serde_json::from_slice(input(ptr, len)) {
        Ok(c) => c,
        Err(_) => return finish::<()>(Err(HostError::new("bad_command", "malformed fence command"))),
    };
    let r: Result<serde_json::Value, HostError> = {
        let mut s = st();
        let f = s.fence.get_or_insert_with(Controller::new);
        match cmd {
            FenceCmd::Start { instance } => Ok(serde_json::json!({ "generation": f.start(instance) })),
            FenceCmd::Retire { instance } => Ok(serde_json::json!({ "dropped": f.retire(instance) })),
            FenceCmd::Issue { instance, scope_key, revision, op } => f
                .issue(instance, &scope_key, revision, op)
                .map(|id| serde_json::json!({ "id": id, "generation": f.generation(instance) }))
                .ok_or_else(|| {
                    if f.generation(instance).is_some() {
                        HostError::new("busy", "pending request quota exceeded")
                    } else {
                        HostError::new("stale", "instance is not running")
                    }
                }),
            FenceCmd::Supersede { instance, scope_key, revision } => {
                Ok(serde_json::json!({ "dropped": f.supersede(instance, &scope_key, revision) }))
            }
            FenceCmd::Cancel { id } => Ok(serde_json::json!({ "cancelled": f.cancel(id) })),
            FenceCmd::Accept { response } => Ok(serde_json::to_value(f.accept(&response)).unwrap_or_default()),
            FenceCmd::Stats => Ok(serde_json::json!({ "pending": f.pending_len(), "live": f.live_len(), "rejected": f.rejected })),
        }
    };
    finish(r)
}
