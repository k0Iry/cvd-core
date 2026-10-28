//! Test-only library that speaks the raw C ABI directly (bypassing the SDK
//! author layer) to inject hostile error buffers and descendant processes.
//! Never shipped.
//!
//! Params JSON: `{"mode": "...", "pidfile": "/path"}`. Modes:
//! `ok`, `create_big_err`, `invoke_big_err`, `create_malformed_err`,
//! `invoke_malformed_err`, `unknown_code`, `unicode_err`, `documented_err`,
//! `reserved_code`, `sleeper` (fork a TERM-ignoring `sleep` in create, write
//! its pid to `pidfile`), `sleeper_hang` (same, then invoke hangs).
//!
//! Every library buffer is counted; a call that starts while a previous
//! buffer is still outstanding aborts, proving the runner always frees.

use cvd_indicator_sdk::abi::*;
use cvd_indicator_sdk::*;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

static OUTSTANDING: AtomicIsize = AtomicIsize::new(0);

fn buf(v: Vec<u8>) -> CvdBuffer {
    OUTSTANDING.fetch_add(1, Ordering::SeqCst);
    CvdBuffer::from_vec(v)
}

fn check_freed() {
    if OUTSTANDING.load(Ordering::SeqCst) != 0 {
        eprintln!("raw-abi fixture: leaked library buffer");
        std::process::abort();
    }
}

struct Inst {
    mode: String,
}

fn descriptor() -> Descriptor {
    Descriptor {
        id: "test.raw_abi".into(),
        version: "0.0.1".into(),
        name: "Raw ABI fixture".into(),
        description: String::new(),
        abi: ABI_VERSION,
        context: ContextSpec { bars: 0, per_tf: Default::default() },
        params: vec![],
        inputs: vec![InputField::Price],
        outputs: vec![OutputSpec {
            key: "close".into(),
            label: "Close".into(),
            pane: Pane::Price,
            pane_group: None,
            colors: vec![],
            widths: vec![],
            readout_only: false,
            kind: OutputKind::Line,
        }],
        live: true,
    }
}

const SECRET: &str = "/opt/data/secret-path INTERNAL";

fn error_doc(mode: &str) -> Option<Vec<u8>> {
    let big = || {
        let msg = format!("{SECRET} {}", "x".repeat(2 * 1024 * 1024));
        serde_json::to_vec(&serde_json::json!({"code": "compute_failed", "message": msg})).unwrap()
    };
    Some(match mode {
        "create_big_err" | "invoke_big_err" => big(),
        "create_malformed_err" | "invoke_malformed_err" => format!("not json {SECRET}").into_bytes(),
        "unknown_code" => {
            br#"{"code":"INTERNAL_DB_password=hunter2","message":"/opt/data/secret-path INTERNAL"}"#.to_vec()
        }
        // 600 bytes of 3-byte chars: crosses every small cap mid-character.
        "unicode_err" => {
            serde_json::to_vec(&serde_json::json!({"code": "invalid_input", "message": "\u{20ac}".repeat(200)}))
                .unwrap()
        }
        "documented_err" => br#"{"code":"invalid_params","message":"window must be positive"}"#.to_vec(),
        "reserved_code" => br#"{"code":"crashed","message":"pretend transport failure"}"#.to_vec(),
        _ => return None,
    })
}

fn spawn_sleeper(pidfile: &str) {
    // SAFETY: test fixture; child only calls async-signal-safe functions.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            let prog = c"/bin/sleep";
            let a1 = c"30";
            let argv = [prog.as_ptr(), a1.as_ptr(), std::ptr::null()];
            libc::execv(prog.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
        std::fs::write(pidfile, pid.to_string()).unwrap();
    }
}

unsafe extern "C" fn describe(out: *mut CvdBuffer) -> i32 {
    check_freed();
    *out = buf(serde_json::to_vec(&descriptor()).unwrap());
    STATUS_OK
}

unsafe extern "C" fn create(p: *const u8, n: usize, h: *mut *mut c_void, err: *mut CvdBuffer) -> i32 {
    check_freed();
    let params: serde_json::Value =
        serde_json::from_slice(if n == 0 { b"{}" } else { std::slice::from_raw_parts(p, n) }).unwrap_or_default();
    let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("ok").to_string();
    if mode.starts_with("create_") {
        *err = buf(error_doc(&mode).unwrap());
        return STATUS_ERROR;
    }
    if mode.starts_with("sleeper") {
        spawn_sleeper(params.get("pidfile").and_then(|v| v.as_str()).expect("pidfile"));
    }
    *h = Box::into_raw(Box::new(Inst { mode })).cast();
    STATUS_OK
}

unsafe extern "C" fn invoke(h: *mut c_void, op: u32, p: *const u8, n: usize, out: *mut CvdBuffer) -> i32 {
    check_freed();
    let inst = &*(h as *const Inst);
    if inst.mode == "sleeper_hang" {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    if let Some(doc) = error_doc(&inst.mode) {
        *out = buf(doc);
        return STATUS_ERROR;
    }
    let body = std::slice::from_raw_parts(p, n);
    let (times, vals): (Vec<i64>, Vec<Option<f64>>) = if op == OP_HISTORY {
        let r: HistoryRequest = serde_json::from_slice(body).unwrap();
        r.bars[r.output_from..].iter().map(|b| (b.time, Some(b.price_close))).unzip()
    } else {
        let r: LiveInit = serde_json::from_slice(body).unwrap();
        r.bars.last().map(|b| (b.time, Some(b.price_close))).into_iter().unzip()
    };
    let mut series = std::collections::BTreeMap::new();
    series.insert("close".to_string(), SeriesValues::Line { values: vals });
    *out = buf(serde_json::to_vec(&Output { times, series }).unwrap());
    STATUS_OK
}

unsafe extern "C" fn destroy(h: *mut c_void) {
    if !h.is_null() {
        drop(Box::from_raw(h as *mut Inst));
    }
}

unsafe extern "C" fn free_buffer(b: CvdBuffer) {
    if b.into_vec().is_some() {
        OUTSTANDING.fetch_sub(1, Ordering::SeqCst);
    }
}

static TABLE: CvdPluginApiV1 = CvdPluginApiV1 {
    abi_version: ABI_VERSION,
    struct_size: API_V1_SIZE,
    describe,
    create,
    invoke,
    destroy,
    free_buffer,
};

/// # Safety
/// Fixture only.
#[no_mangle]
pub unsafe extern "C" fn cvd_plugin_entry_v1(host: u32) -> *const CvdPluginApiV1 {
    if host == ABI_VERSION {
        &TABLE
    } else {
        std::ptr::null()
    }
}
