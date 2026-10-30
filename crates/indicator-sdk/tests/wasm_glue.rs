//! Native unit tests of the target-independent `cvd-wasm` glue state.
use cvd_indicator_sdk::abi::*;
use cvd_indicator_sdk::wasm::{raw, WasmGlue, MAX_INPUT_BYTES, MAX_PARAMS_BYTES};
use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

struct Echo;
impl Indicator for Echo {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "t.echo".into(),
            version: "1".into(),
            name: "e".into(),
            description: String::new(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: 0, per_tf: Default::default() },
            params: vec![],
            inputs: vec![InputField::Price],
            outputs: vec![OutputSpec {
                key: "c".into(),
                label: "c".into(),
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
    fn new(_: &ParamValues) -> Result<Self, PluginError> {
        Ok(Echo)
    }
    fn compute(&mut self, _: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        let mut s = BTreeMap::new();
        s.insert("c".into(), SeriesValues::Line { values: bars[from..].iter().map(|b| Some(b.price_close)).collect() });
        Ok(s)
    }
}

fn req() -> Vec<u8> {
    let bars: Vec<InputBar> =
        (1..=3).map(|i| InputBar { time: i * 60, price_close: i as f64, ..Default::default() }).collect();
    serde_json::to_vec(&HistoryRequest {
        scope: Scope {
            exchange: "x".into(),
            market: "spot".into(),
            symbol: "s".into(),
            tf: "1m".into(),
            mode: Mode::Source,
        },
        revision: 7,
        output_from: 1,
        bars,
    })
    .unwrap()
}

#[test]
fn glue_lifecycle_and_bounds() {
    let mut g = WasmGlue::<Echo>::new();
    assert_eq!(g.invoke(OP_HISTORY, &req()), STATUS_INVALID_ARGUMENT, "invoke before create");
    assert_eq!(g.describe(), STATUS_OK);
    let d: Descriptor = serde_json::from_slice(g.result()).unwrap();
    assert_eq!(d.id, "t.echo");
    assert_eq!(g.create(b"{}"), STATUS_OK);
    assert_eq!(g.create(b"{}"), STATUS_INVALID_ARGUMENT, "second create rejected");
    assert_eq!(g.invoke(OP_HISTORY, &req()), STATUS_OK);
    let o: Output = serde_json::from_slice(g.result()).unwrap();
    assert_eq!(o.times, vec![120, 180]);
    assert_eq!(g.invoke(99, &req()), STATUS_INVALID_ARGUMENT);
    assert_eq!(g.invoke(OP_HISTORY, b"{not json"), STATUS_ERROR);
    let e: PluginError = serde_json::from_slice(g.result()).unwrap();
    assert_eq!(e.code, "invalid_input");
    g.destroy();
    assert_eq!(g.invoke(OP_HISTORY, &req()), STATUS_INVALID_ARGUMENT, "destroyed");
    assert_eq!(g.create(&vec![b' '; MAX_PARAMS_BYTES + 1]), STATUS_INVALID_ARGUMENT);
}

#[test]
fn raw_alloc_bounds() {
    assert_eq!(raw::alloc_bytes(0), 0);
    assert_eq!(raw::alloc_bytes(MAX_INPUT_BYTES + 1), 0);
    let p = raw::alloc_bytes(16);
    assert_ne!(p, 0);
    unsafe { raw::dealloc_bytes(p, 16) };
    assert!(unsafe { raw::input(0, 4) }.is_none());
}

/// Mutable-state indicator: output folds a per-instance call counter.
struct Counter(u64);
impl Indicator for Counter {
    fn descriptor() -> Descriptor {
        Echo::descriptor()
    }
    fn new(_: &ParamValues) -> Result<Self, PluginError> {
        Ok(Counter(0))
    }
    fn compute(&mut self, _: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        self.0 += 1;
        let mut s = BTreeMap::new();
        let k = self.0 as f64 * 1000.0;
        s.insert(
            "c".into(),
            SeriesValues::Line { values: bars[from..].iter().map(|b| Some(b.price_close + k)).collect() },
        );
        Ok(s)
    }
}

/// Item 8: original native runner semantics — History = fresh instance per
/// request (create/invoke/destroy); Live = one persistent instance. The WASM
/// glue must reproduce both, interleaved, without cross-contamination.
#[test]
fn history_is_stateless_and_isolated_from_live() {
    use cvd_indicator_sdk::author::Instance;
    let hist_b = {
        let mut r: HistoryRequest = serde_json::from_slice(&req()).unwrap();
        r.output_from = 0;
        serde_json::to_vec(&r).unwrap()
    };
    let scope: HistoryRequest = serde_json::from_slice(&req()).unwrap();
    let init =
        serde_json::to_vec(&LiveInit { scope: scope.scope.clone(), revision: 7, bars: scope.bars.clone() }).unwrap();
    let upd = |seq: u64, t: i64| {
        serde_json::to_vec(&LiveUpdate {
            revision: 7,
            seq,
            ops: vec![LiveOp::Append { bar: InputBar { time: t, price_close: 9.0, ..Default::default() } }],
        })
        .unwrap()
    };
    let fresh = |op, body: &[u8]| {
        let mut i = Instance::<Counter>::create(b"{}").unwrap();
        serde_json::to_vec(&i.invoke(op, body).unwrap()).unwrap()
    };
    let mut live = Instance::<Counter>::create(b"{}").unwrap();
    let mut g = WasmGlue::<Counter>::new();
    assert_eq!(g.create(b"{}"), STATUS_OK);
    let steps: Vec<(u32, Vec<u8>)> = vec![
        (OP_HISTORY, req()),
        (OP_HISTORY, req()),
        (OP_HISTORY, hist_b.clone()),
        (OP_HISTORY, req()),
        (OP_LIVE_INIT, init),
        (OP_HISTORY, req()),
        (OP_LIVE_UPDATE, upd(1, 240)),
        (OP_HISTORY, hist_b),
        (OP_LIVE_UPDATE, upd(2, 300)),
        (OP_HISTORY, req()),
    ];
    for (k, (op, body)) in steps.iter().enumerate() {
        let want = if *op == OP_HISTORY {
            fresh(*op, body)
        } else {
            serde_json::to_vec(&live.invoke(*op, body).unwrap()).unwrap()
        };
        assert_eq!(g.invoke(*op, body), STATUS_OK, "step {k}");
        assert_eq!(g.result(), &want[..], "step {k} op {op}: wasm glue diverges from native runner semantics");
    }
}
