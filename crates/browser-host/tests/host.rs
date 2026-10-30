use cvd_browser_host::contract::{check_region, PluginSession};
use cvd_browser_host::fence::{Controller, Response, Verdict};
use cvd_browser_host::{hostile_modules, module_check::validate_module};
use cvd_indicator_sdk::abi::*;
use cvd_indicator_sdk::wasm::WasmGlue;
use cvd_indicator_sdk::*;

fn wasm_path(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var("CVD_WASM_DIR").ok()?;
    std::fs::read(std::path::Path::new(&dir).join(name)).ok()
}

#[test]
fn hostile_modules_rejected_before_compile() {
    let mut bad = Vec::new();
    for (name, bytes, code) in hostile_modules::all() {
        match validate_module(&bytes) {
            Err(e) if e.code == code => {}
            other => bad.push(format!("{name}: expected {code}, got {other:?}")),
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    let e = validate_module(&vec![0u8; (16 << 20) + 1]).unwrap_err();
    assert_eq!(e.code, "module_too_large");
}

#[test]
fn real_release_modules_accepted() {
    // Required gate in CI/local runs: CVD_WASM_DIR must point at the wasm-release dir.
    let dir = std::env::var("CVD_WASM_DIR").expect("set CVD_WASM_DIR to the wasm32 wasm-release output dir");
    for m in ["sample_indicator.wasm", "hostile_wasm_plugin.wasm"] {
        let b = wasm_path(m).unwrap_or_else(|| panic!("{dir}/{m} missing"));
        let r = validate_module(&b).unwrap_or_else(|e| panic!("{m}: {e}"));
        assert!(r.initial_pages <= 256);
    }
    // The host module itself is not a plugin: it has the wrong export set.
    let host = wasm_path("cvd_browser_host.wasm").expect("host wasm");
    assert_eq!(validate_module(&host).unwrap_err().code, "unexpected_export");
}

#[test]
fn region_checks() {
    assert!(check_region(65536, 16, 100, 1 << 20).is_ok());
    assert!(check_region(65536, 0, 0, 1 << 20).is_ok());
    for (m, p, l) in [(65536, 65500, 100), (65536, 0, 4), (65536, u64::MAX - 1, 4), (65536, 16, (32 << 20) + 1)] {
        assert_eq!(check_region(m, p, l, 32 << 20).unwrap_err().code, "invalid_output_region");
    }
    assert_eq!(check_region((512 << 20) + 65536, 16, 4, 64).unwrap_err().code, "memory_limit");
}

fn sample_flow() -> (PluginSession, WasmGlue<sample_indicator::SampleChannel>) {
    let mut s = PluginSession::new();
    let mut g = WasmGlue::<sample_indicator::SampleChannel>::new();
    s.accept_abi(1).unwrap();
    let st = g.describe();
    s.accept_descriptor(st, g.result()).unwrap();
    let st = g.create(br#"{"length":3}"#);
    s.accept_create(st, g.result()).unwrap();
    (s, g)
}

fn bars(n: i64) -> Vec<InputBar> {
    (1..=n)
        .map(|i| InputBar {
            time: i * 60,
            price_close: i as f64,
            ph: i as f64 + 1.0,
            pl: i as f64 - 1.0,
            ..Default::default()
        })
        .collect()
}

fn scope() -> Scope {
    Scope { exchange: "x".into(), market: "spot".into(), symbol: "s".into(), tf: "1m".into(), mode: Mode::Source }
}

#[test]
fn session_validates_outputs_and_lifecycle() {
    let mut s = PluginSession::new();
    assert_eq!(s.accept_abi(2).unwrap_err().code, "abi_mismatch");
    assert_eq!(s.accept_abi(1).unwrap_err().code, "poisoned");

    let (mut s, mut g) = sample_flow();
    let body =
        serde_json::to_vec(&HistoryRequest { scope: scope(), revision: 1, output_from: 2, bars: bars(6) }).unwrap();
    s.begin(OP_HISTORY, &body).unwrap();
    assert_eq!(s.begin(OP_HISTORY, &body).unwrap_err().code, "lifecycle", "one call in flight");
    let st = g.invoke(OP_HISTORY, &body);
    let out: Output = serde_json::from_str(&s.accept_result(st, g.result()).unwrap()).unwrap();
    assert_eq!(out.times, vec![180, 240, 300, 360]);
    assert_eq!(s.accept_result(0, b"{}").unwrap_err().code, "lifecycle", "unsolicited result");

    // plugin error is non-fatal and mapped to public code
    s.begin(OP_HISTORY, &body).unwrap();
    assert_eq!(
        s.accept_result(STATUS_ERROR, br#"{"code":"secret_x","message":"leak"}"#).unwrap_err().message,
        "compute_failed: indicator computation failed"
    );
    // axis mismatch poisons
    s.begin(OP_HISTORY, &body).unwrap();
    let bad = br#"{"times":[1],"series":{}}"#;
    assert_eq!(s.accept_result(STATUS_OK, bad).unwrap_err().code, "invalid_output");
    assert!(s.is_poisoned());
    assert_eq!(s.begin(OP_HISTORY, &body).unwrap_err().code, "poisoned");
}

#[test]
fn live_axes_follow_window() {
    let (mut s, mut g) = sample_flow();
    let init = serde_json::to_vec(&LiveInit { scope: scope(), revision: 3, bars: bars(5) }).unwrap();
    s.begin(OP_LIVE_INIT, &init).unwrap();
    let st = g.invoke(OP_LIVE_INIT, &init);
    s.accept_result(st, g.result()).unwrap();
    let b6 = InputBar { time: 360, price_close: 6.0, ph: 7.0, pl: 5.0, ..Default::default() };
    let mut b5 = b6.clone();
    b5.time = 300;
    let upd = serde_json::to_vec(&LiveUpdate {
        revision: 3,
        seq: 1,
        ops: vec![LiveOp::Replace { bar: b5 }, LiveOp::Append { bar: b6 }],
    })
    .unwrap();
    s.begin(OP_LIVE_UPDATE, &upd).unwrap();
    let st = g.invoke(OP_LIVE_UPDATE, &upd);
    let out: Output = serde_json::from_str(&s.accept_result(st, g.result()).unwrap()).unwrap();
    assert_eq!(out.times, vec![300, 360]);
}

#[test]
fn fence_rejects_unsolicited_and_stale() {
    let mut f = Controller::new();
    let g1 = f.start(7);
    let id = f.issue(7, "binance:spot:BTC:1m:source", 5, OP_HISTORY).unwrap();
    let ok = Response {
        id,
        instance: 7,
        generation: g1,
        kind: "result".into(),
        scope_key: "binance:spot:BTC:1m:source".into(),
        revision: 5,
    };
    let mut bad = ok.clone();
    bad.id = 999;
    assert_eq!(f.accept(&bad), Verdict::Rejected { reason: "unsolicited" });
    let mut wrong_rev = ok.clone();
    wrong_rev.revision = 4;
    assert_eq!(f.accept(&wrong_rev), Verdict::Rejected { reason: "mismatched_scope" });
    assert_eq!(f.accept(&ok), Verdict::Accepted { op: OP_HISTORY });
    assert_eq!(f.accept(&ok), Verdict::Rejected { reason: "unsolicited" }, "duplicate");
    // disable drops in-flight; restart bumps generation
    let id2 = f.issue(7, "k", 6, OP_HISTORY).unwrap();
    assert_eq!(f.retire(7), vec![id2]);
    assert!(f.issue(7, "k", 6, OP_HISTORY).is_none());
    let g2 = f.start(7);
    assert!(g2 > g1);
    let late =
        Response { id: id2, instance: 7, generation: g1, kind: "result".into(), scope_key: "k".into(), revision: 6 };
    assert_eq!(f.accept(&late), Verdict::Rejected { reason: "unsolicited" });
    let id3 = f.issue(7, "k", 6, OP_HISTORY).unwrap();
    let old_gen =
        Response { id: id3, instance: 7, generation: g1, kind: "result".into(), scope_key: "k".into(), revision: 6 };
    assert_eq!(f.accept(&old_gen), Verdict::Rejected { reason: "mismatched_identity" });
    assert_eq!(f.supersede(7, "k2", 1).len(), 1);
    assert_eq!(f.rejected, 5);
}

// ---- parent review #1: real-artifact counterexamples (one defect each) ----

fn leb(b: &[u8], p: &mut usize) -> u64 {
    let (mut r, mut s) = (0u64, 0);
    loop {
        let x = b[*p];
        *p += 1;
        r |= u64::from(x & 0x7f) << s;
        s += 7;
        if x < 0x80 {
            return r;
        }
    }
}
fn enc(mut v: u64) -> Vec<u8> {
    let mut o = Vec::new();
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            o.push(b);
            return o;
        }
        o.push(b | 0x80);
    }
}
/// Rebuild `m` with section `id`'s body replaced by `f(body)`.
fn mutate(m: &[u8], id: u8, f: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = m[..8].to_vec();
    let mut p = 8;
    let mut hit = false;
    while p < m.len() {
        let sid = m[p];
        p += 1;
        let n = leb(m, &mut p) as usize;
        let body = &m[p..p + n];
        p += n;
        let nb = if sid == id {
            hit = true;
            f(body)
        } else {
            body.to_vec()
        };
        out.push(sid);
        out.extend(enc(nb.len() as u64));
        out.extend(nb);
    }
    assert!(hit, "section {id} not found");
    out
}
fn limits(body: &[u8], skip: usize) -> (u64, Option<u64>) {
    let mut p = skip + 1;
    let fl = body[skip];
    let min = leb(body, &mut p);
    let max = (fl & 1 == 1).then(|| leb(body, &mut p));
    (min, max)
}

#[test]
fn real_module_resource_counterexamples_rejected() {
    let sample = wasm_path("sample_indicator.wasm").expect("CVD_WASM_DIR sample_indicator.wasm");
    let r = validate_module(&sample).expect("bounded real build accepted");
    assert!(r.max_pages <= 8192 && r.table.is_some_and(|(_, m)| m <= 65_536), "{r:?}");
    // memory without maximum (what a build lacking --max-memory produces)
    let m = mutate(&sample, 5, |b| {
        let (min, _) = limits(b, 1);
        [vec![1, 0], enc(min)].concat()
    });
    assert_eq!(validate_module(&m).unwrap_err().code, "bad_memory", "unbounded memory");
    // memory maximum above the 512 MiB profile
    let m = mutate(&sample, 5, |b| {
        let (min, _) = limits(b, 1);
        [vec![1, 1], enc(min), enc(8193)].concat()
    });
    assert_eq!(validate_module(&m).unwrap_err().code, "bad_memory", "over-profile max");
    // growable (no max) funcref table
    let m = mutate(&sample, 4, |b| {
        let (min, _) = limits(b, 2);
        [vec![1, 0x70, 0], enc(min)].concat()
    });
    assert_eq!(validate_module(&m).unwrap_err().code, "bad_table", "growable table");
    // table max above profile
    let m = mutate(&sample, 4, |b| {
        let (min, _) = limits(b, 2);
        [vec![1, 0x70, 1], enc(min), enc(1 << 20)].concat()
    });
    assert_eq!(validate_module(&m).unwrap_err().code, "bad_table", "huge table");
}

// ---- parent review #6: host live mirror == SDK live window semantics ----

fn bar(t: i64, v: f64) -> InputBar {
    InputBar { time: t, price_close: v, ph: v + 1.0, pl: v - 1.0, ..Default::default() }
}

#[test]
fn live_mirror_matches_sdk_window_beyond_cap() {
    let (mut s, mut g) = sample_flow();
    let init = serde_json::to_vec(&LiveInit { scope: scope(), revision: 9, bars: bars(400) }).unwrap();
    s.begin(OP_LIVE_INIT, &init).unwrap();
    let st = g.invoke(OP_LIVE_INIT, &init);
    s.accept_result(st, g.result()).unwrap();
    let call =
        |s: &mut PluginSession, g: &mut WasmGlue<sample_indicator::SampleChannel>, seq: u64, ops: Vec<LiveOp>| {
            let b = serde_json::to_vec(&LiveUpdate { revision: 9, seq, ops }).unwrap();
            s.begin(OP_LIVE_UPDATE, &b).map(|_| {
                let st = g.invoke(OP_LIVE_UPDATE, &b);
                s.accept_result(st, g.result())
            })
        };
    // 300 appends (well past the retention cap) with interleaved replaces; host
    // admission and expected axis must agree with the SDK at every step.
    let mut t = 400 * 60;
    for seq in 1..=300u64 {
        t += 60;
        let mut ops = vec![LiveOp::Append { bar: bar(t, seq as f64) }];
        if seq % 7 == 0 {
            ops.insert(0, LiveOp::Replace { bar: bar(t - 120, 1.5) });
        }
        call(&mut s, &mut g, seq, ops).expect("admitted").expect("valid output");
    }
    let cap = s.live().unwrap().cap;
    assert_eq!(s.live().unwrap().times.len(), cap, "bounded window");
    assert_eq!(s.live().unwrap().seq, 300);
    // replace of a bar trimmed out of the window: host rejects BEFORE the plugin
    let old = *s.live().unwrap().times.first().unwrap() - 60;
    assert_eq!(
        call(&mut s, &mut g, 301, vec![LiveOp::Replace { bar: bar(old, 1.0) }]).unwrap_err().code,
        "invalid_input"
    );
    // bad seq / revision / non-finite input: nonfatal host rejections
    assert_eq!(call(&mut s, &mut g, 305, vec![]).unwrap_err().code, "sequence_gap");
    let b = serde_json::to_vec(&LiveUpdate { revision: 8, seq: 301, ops: vec![] }).unwrap();
    assert_eq!(s.begin(OP_LIVE_UPDATE, &b).unwrap_err().code, "stale_revision");
    let mut nan = bar(t + 60, 1.0);
    nan.price_close = f64::NAN;
    let b = serde_json::to_string(&LiveUpdate { revision: 9, seq: 301, ops: vec![LiveOp::Append { bar: nan }] })
        .unwrap()
        .replace("null", "1e999");
    assert!(s.begin(OP_LIVE_UPDATE, b.as_bytes()).is_err(), "non-finite rejected");
    assert!(!s.is_poisoned());
    assert_eq!(s.live().unwrap().seq, 300, "rejections did not commit");
    // recovery: the next correct seq works and matches the SDK
    let out = call(&mut s, &mut g, 301, vec![LiveOp::Append { bar: bar(t + 60, 2.0) }]).unwrap().unwrap();
    let out: Output = serde_json::from_str(&out).unwrap();
    assert_eq!(out.times, vec![t + 60]);
}

// ---- parent review #3/#4: complete identity, expected kind, quotas ----

fn resp(j: &str) -> Result<Response, serde_json::Error> {
    serde_json::from_str(j)
}

#[test]
fn fence_requires_complete_identity_and_kind() {
    let mut f = Controller::new();
    let g = f.start(1);
    let id = f.issue(1, "k", 5, OP_HISTORY).unwrap();
    // missing / null scope or revision is not a valid response at all
    for j in [
        format!(r#"{{"id":{id},"instance":1,"generation":{g},"kind":"result","revision":5}}"#),
        format!(r#"{{"id":{id},"instance":1,"generation":{g},"kind":"result","scope_key":null,"revision":5}}"#),
        format!(r#"{{"id":{id},"instance":1,"generation":{g},"kind":"result","scope_key":"k"}}"#),
        format!(r#"{{"id":{id},"instance":1,"generation":{g},"scope_key":"k","revision":5}}"#),
    ] {
        assert!(resp(&j).is_err(), "incomplete identity must not parse: {j}");
    }
    // wrong kind for a compute (a `loaded` frame) is rejected and does NOT consume the waiter
    let wrong =
        resp(&format!(r#"{{"id":{id},"instance":1,"generation":{g},"kind":"loaded","scope_key":"k","revision":5}}"#))
            .unwrap();
    assert_eq!(f.accept(&wrong), Verdict::Rejected { reason: "unexpected_kind" });
    let cross =
        resp(&format!(r#"{{"id":{id},"instance":2,"generation":{g},"kind":"result","scope_key":"k","revision":5}}"#))
            .unwrap();
    assert_eq!(f.accept(&cross), Verdict::Rejected { reason: "mismatched_identity" });
    assert_eq!(f.pending_len(), 1, "rejected frames left the valid waiter intact");
    let ok =
        resp(&format!(r#"{{"id":{id},"instance":1,"generation":{g},"kind":"result","scope_key":"k","revision":5}}"#))
            .unwrap();
    assert_eq!(f.accept(&ok), Verdict::Accepted { op: OP_HISTORY });
    assert_eq!(f.accept(&ok), Verdict::Rejected { reason: "unsolicited" }, "duplicate");
    // load frames: op 0 expects `loaded` (or `error`) with the special load scope
    let lid = f.issue(1, cvd_browser_host::fence::LOAD_SCOPE, 0, 0).unwrap();
    let res = resp(&format!(
        r#"{{"id":{lid},"instance":1,"generation":{g},"kind":"result","scope_key":"__load","revision":0}}"#
    ))
    .unwrap();
    assert_eq!(f.accept(&res), Verdict::Rejected { reason: "unexpected_kind" });
    let err = resp(&format!(
        r#"{{"id":{lid},"instance":1,"generation":{g},"kind":"error","scope_key":"__load","revision":0}}"#
    ))
    .unwrap();
    assert_eq!(f.accept(&err), Verdict::Accepted { op: 0 });
}

#[test]
fn fence_quota_and_supersede_report_ids() {
    let mut f = Controller::new();
    f.start(1);
    let ids: Vec<u64> = (0..cvd_browser_host::fence::MAX_PENDING).map(|_| f.issue(1, "k", 1, 1).unwrap()).collect();
    assert!(f.issue(1, "k", 1, 1).is_none(), "pending quota enforced");
    let dropped = f.supersede(1, "k2", 2);
    assert_eq!(dropped, ids, "supersede returns the cancelled ids so JS can settle them");
    let a = f.issue(1, "k2", 2, 1).unwrap();
    assert_eq!(f.retire(1), vec![a]);
}
