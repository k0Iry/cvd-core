//! Native oracle: runs the sample fixture through the CURRENT native C ABI v1
//! function table (`cvd_plugin_entry_v1`) on the canonical request and prints
//! the output JSON. Also writes hostile byte modules when given a directory.
#[cfg(not(target_arch = "wasm32"))]
use cvd_indicator_sdk::abi::*;
use cvd_indicator_sdk::*;

pub fn canonical_request() -> HistoryRequest {
    let bars: Vec<InputBar> = (0..120i64)
        .map(|i| {
            let x = i as f64;
            let c = 100.0 + (x * 0.37).sin() * 3.0 + x * 0.013;
            InputBar {
                time: 1_700_000_059 + i * 60,
                po: c - 0.1,
                ph: c + 0.7 + (x * 0.11).cos().abs(),
                pl: c - 0.6 - (x * 0.07).sin().abs(),
                price_close: c,
                apo: c - 0.05,
                aph: c + 0.9,
                apl: c - 0.8,
                apc: c + 0.01,
                cvd_usdt: x * 1234.5,
                ac: x * 2000.25,
                delta_usdt: if i % 7 == 0 { 0.0 } else { (x * 0.9).sin() * 1e4 },
                ad: (x * 0.5).cos() * 2e4,
                buy_volume_usdt: 5e4 + x,
                sell_volume_usdt: 4e4 + x * 0.5,
                agb: 9e4 + x,
                ags: 8e4 - x,
                trades: i as u64 * 3,
                ..Default::default()
            }
        })
        .collect();
    HistoryRequest {
        scope: Scope {
            exchange: "binance".into(),
            market: "spot".into(),
            symbol: "BTCUSDT".into(),
            tf: "1m".into(),
            mode: Mode::Source,
        },
        revision: 42,
        output_from: 10,
        bars,
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--hostile-dir") {
        let dir = std::path::Path::new(&args[2]);
        std::fs::create_dir_all(dir).unwrap();
        for (n, b, code) in cvd_browser_host::hostile_modules::all() {
            std::fs::write(dir.join(format!("{n}.wasm")), b).unwrap();
            println!("{n}\t{code}");
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--request") {
        println!("{}", serde_json::to_string(&canonical_request()).unwrap());
        return;
    }
    if args.get(1).map(String::as_str) == Some("--fixtures") {
        // One deterministic owned fixture set for the foundation gate.
        let dir = std::path::Path::new(&args[2]);
        let hd = dir.join("hostile");
        std::fs::create_dir_all(&hd).unwrap();
        let mut index = Vec::new();
        for (n, b, code) in cvd_browser_host::hostile_modules::all() {
            std::fs::write(hd.join(format!("{n}.wasm")), b).unwrap();
            index.push((n, code));
        }
        std::fs::write(hd.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        std::fs::write(dir.join("request.json"), serde_json::to_vec(&canonical_request()).unwrap()).unwrap();
        std::fs::write(dir.join("oracle.json"), native_history()).unwrap();
        std::fs::write(dir.join("live-script.json"), serde_json::to_vec(&live_script()).unwrap()).unwrap();
        std::fs::write(dir.join("stateful-script.json"), serde_json::to_vec(&stateful_script()).unwrap()).unwrap();
        return;
    }
    print!("{}", String::from_utf8(native_history()).unwrap());
}

#[cfg(not(target_arch = "wasm32"))]
struct Native {
    api: &'static CvdPluginApiV1,
    h: *mut std::ffi::c_void,
}

#[cfg(not(target_arch = "wasm32"))]
impl Native {
    fn new() -> Self {
        let api = unsafe { &*sample_indicator::cvd_plugin_entry_v1(ABI_VERSION) };
        let params = br#"{"length":14}"#;
        let mut h = std::ptr::null_mut();
        let mut err = CvdBuffer::empty();
        assert_eq!(unsafe { (api.create)(params.as_ptr(), params.len(), &mut h, &mut err) }, STATUS_OK);
        Self { api, h }
    }
    fn call(&mut self, op: u32, body: &[u8]) -> (i32, String) {
        let mut out = CvdBuffer::empty();
        let st = unsafe { (self.api.invoke)(self.h, op, body.as_ptr(), body.len(), &mut out) };
        let bytes = unsafe { out.as_slice() }.to_vec();
        unsafe { (self.api.free_buffer)(out) };
        (st, String::from_utf8(bytes).unwrap())
    }
}

/// Native C-ABI live oracle: init beyond the retention cap, 150 updates with
/// replaces, plus host-rejected controls and recovery. Each step records the
/// expected browser outcome.
#[cfg(not(target_arch = "wasm32"))]
fn live_script() -> serde_json::Value {
    use serde_json::json;
    let mut n = Native::new();
    let base = canonical_request();
    let mk = |i: i64| {
        let mut b = base.bars[(i as usize) % base.bars.len()].clone();
        b.time = 1_700_000_059 + i * 60;
        b.price_close += (i as f64) * 0.001;
        b
    };
    let mut steps = Vec::new();
    let init = LiveInit { scope: base.scope.clone(), revision: 77, bars: (0..700).map(mk).collect() };
    let body = serde_json::to_string(&init).unwrap();
    let (st, out) = n.call(OP_LIVE_INIT, body.as_bytes());
    assert_eq!(st, STATUS_OK);
    steps.push(json!({"op": 2, "body": body, "ok": out}));
    let mut next = 700i64;
    for seq in 1..=150u64 {
        let mut ops = vec![LiveOp::Append { bar: mk(next) }];
        if seq % 5 == 0 {
            let mut r = mk(next - 3);
            r.price_close += 0.5;
            ops.insert(0, LiveOp::Replace { bar: r });
        }
        next += 1;
        let body = serde_json::to_string(&LiveUpdate { revision: 77, seq, ops }).unwrap();
        let (st, out) = n.call(OP_LIVE_UPDATE, body.as_bytes());
        assert_eq!(st, STATUS_OK, "{out}");
        steps.push(json!({"op": 3, "body": body, "ok": out}));
    }
    // Host-rejected controls (never reach the plugin; state unchanged).
    let trimmed = mk(next - 600); // outside the 565-bar cap
    for (body, code) in [
        (serde_json::to_string(&LiveUpdate { revision: 77, seq: 153, ops: vec![] }).unwrap(), "sequence_gap"),
        (serde_json::to_string(&LiveUpdate { revision: 76, seq: 151, ops: vec![] }).unwrap(), "stale_revision"),
        (
            serde_json::to_string(&LiveUpdate { revision: 77, seq: 151, ops: vec![LiveOp::Replace { bar: trimmed }] })
                .unwrap(),
            "invalid_input",
        ),
        (
            serde_json::to_string(&LiveUpdate { revision: 77, seq: 151, ops: vec![LiveOp::Append { bar: mk(next) }] })
                .unwrap()
                .replacen("\"price_close\":", "\"price_close\":1e999,\"x\":", 1),
            "invalid_input",
        ),
    ] {
        steps.push(json!({"op": 3, "body": body, "err": code}));
    }
    // Recovery with the correct next seq.
    let body =
        serde_json::to_string(&LiveUpdate { revision: 77, seq: 151, ops: vec![LiveOp::Append { bar: mk(next) }] })
            .unwrap();
    let (st, out) = n.call(OP_LIVE_UPDATE, body.as_bytes());
    assert_eq!(st, STATUS_OK);
    steps.push(json!({"op": 3, "body": body, "ok": out}));
    json!({ "params": r#"{"length":14}"#, "scope_key": "binance:spot:BTCUSDT:1m:source", "revision": 77, "steps": steps })
}

#[cfg(not(target_arch = "wasm32"))]
fn native_history() -> Vec<u8> {
    let api = unsafe { &*sample_indicator::cvd_plugin_entry_v1(ABI_VERSION) };
    let params = br#"{"length":14}"#;
    let mut h = std::ptr::null_mut();
    let mut err = CvdBuffer::empty();
    assert_eq!(unsafe { (api.create)(params.as_ptr(), params.len(), &mut h, &mut err) }, STATUS_OK);
    let body = serde_json::to_vec(&canonical_request()).unwrap();
    let mut out = CvdBuffer::empty();
    let st = unsafe { (api.invoke)(h, OP_HISTORY, body.as_ptr(), body.len(), &mut out) };
    assert_eq!(st, STATUS_OK);
    let bytes = unsafe { out.as_slice() }.to_vec();
    unsafe { (api.free_buffer)(out) };
    unsafe { (api.destroy)(h) };
    bytes
}

/// Item 8 oracle with the mutable-state fixture, reproducing the ORIGINAL
/// native runner (indicator-runner `FrameKind::History`): every History
/// request runs on a FRESH C-ABI instance (create/invoke/destroy), while
/// LiveInit/LiveUpdate run on ONE persistent instance. Interleaved on purpose.
#[cfg(not(target_arch = "wasm32"))]
fn stateful_script() -> serde_json::Value {
    use cvd_indicator_sdk::author::{create, destroy, free_buffer, invoke};
    use serde_json::json;
    type S = stateful_indicator::Stateful;
    let mk = || {
        let mut h = std::ptr::null_mut();
        let mut e = CvdBuffer::empty();
        assert_eq!(unsafe { create::<S>(b"{}".as_ptr(), 2, &mut h, &mut e) }, STATUS_OK);
        h
    };
    let call = |h, op: u32, body: &[u8]| {
        let mut out = CvdBuffer::empty();
        let st = unsafe { invoke::<S>(h, op, body.as_ptr(), body.len(), &mut out) };
        assert_eq!(st, STATUS_OK);
        let s = String::from_utf8(unsafe { out.as_slice() }.to_vec()).unwrap();
        unsafe { free_buffer(out) };
        s
    };
    let a = canonical_request();
    let mut b = canonical_request();
    b.output_from = 30;
    let ha = serde_json::to_string(&a).unwrap();
    let hb = serde_json::to_string(&b).unwrap();
    let init =
        serde_json::to_string(&LiveInit { scope: a.scope.clone(), revision: 42, bars: a.bars[..60].to_vec() }).unwrap();
    let upd = |seq: u64| {
        let mut bar = a.bars[59 + seq as usize].clone();
        bar.price_close += 0.25;
        serde_json::to_string(&LiveUpdate { revision: 42, seq, ops: vec![LiveOp::Append { bar }] }).unwrap()
    };
    let plan: Vec<(u32, String)> = vec![
        (OP_HISTORY, ha.clone()),
        (OP_HISTORY, ha.clone()),
        (OP_HISTORY, hb.clone()),
        (OP_HISTORY, ha.clone()),
        (OP_LIVE_INIT, init),
        (OP_HISTORY, ha.clone()),
        (OP_LIVE_UPDATE, upd(1)),
        (OP_HISTORY, hb.clone()),
        (OP_LIVE_UPDATE, upd(2)),
        (OP_HISTORY, ha.clone()),
        (OP_LIVE_UPDATE, upd(3)),
    ];
    let live = mk();
    let mut steps = Vec::new();
    for (op, body) in plan {
        let out = if op == OP_HISTORY {
            let h = mk();
            let o = call(h, op, body.as_bytes());
            unsafe { destroy::<S>(h) };
            o
        } else {
            call(live, op, body.as_bytes())
        };
        steps.push(json!({"op": op, "body": body, "ok": out}));
    }
    unsafe { destroy::<S>(live) };
    json!({ "params": "{}", "scope_key": "binance:spot:BTCUSDT:1m:source", "revision": 42, "steps": steps })
}
