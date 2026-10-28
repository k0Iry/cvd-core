//! The SDK passes the triggering operation explicitly in `ComputeContext`.

use cvd_indicator_sdk::author::Instance;
use cvd_indicator_sdk::*;
use std::sync::Mutex;

static SEEN: Mutex<Vec<(ComputePhase, usize, usize)>> = Mutex::new(Vec::new());

struct Probe;
impl Indicator for Probe {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "probe".into(),
            version: "1.0.0".into(),
            name: "Probe".into(),
            description: "phase probe".into(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: 3, per_tf: Default::default() },
            params: vec![],
            inputs: vec![InputField::Price],
            outputs: vec![OutputSpec {
                key: "x".into(),
                label: "X".into(),
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
        Ok(Probe)
    }
    fn compute(&mut self, ctx: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        SEEN.lock().unwrap().push((ctx.phase, bars.len(), from));
        let v = bars[from..].iter().map(|b| Some(b.price_close)).collect();
        Ok([("x".to_string(), SeriesValues::Line { values: v })].into())
    }
}

fn bar(t: i64) -> InputBar {
    InputBar { time: t, po: 1.0, ph: 1.0, pl: 1.0, price_close: t as f64, ..Default::default() }
}

#[test]
fn phase_is_explicit_per_operation() {
    let scope =
        Scope { exchange: "e".into(), market: "m".into(), symbol: "s".into(), tf: "1m".into(), mode: Mode::Source };
    let mut a = Instance::<Probe>::create(b"{}").unwrap();
    let bars: Vec<_> = (1..=10).map(bar).collect();
    let h =
        a.history(&HistoryRequest { scope: scope.clone(), revision: 1, output_from: 3, bars: bars.clone() }).unwrap();
    assert_eq!(h.times, (4..=10).collect::<Vec<_>>());
    // A second history call is still History, not "live" by call count.
    a.history(&HistoryRequest { scope: scope.clone(), revision: 1, output_from: 0, bars: bars[..2].to_vec() }).unwrap();
    let i = a.live_init(LiveInit { scope: scope.clone(), revision: 2, bars: bars.clone() }).unwrap();
    assert_eq!(i.times, vec![10]);
    let u = a
        .live_update(LiveUpdate {
            revision: 2,
            seq: 1,
            ops: vec![LiveOp::Replace { bar: bar(10) }, LiveOp::Append { bar: bar(11) }],
        })
        .unwrap();
    assert_eq!(u.times, vec![10, 11]);
    // Fresh instance whose first call is LIVE_INIT reports LiveInit, not History.
    let mut b = Instance::<Probe>::create(b"{}").unwrap();
    b.live_init(LiveInit { scope, revision: 1, bars }).unwrap();
    let seen = SEEN.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (ComputePhase::History, 10, 3),
            (ComputePhase::History, 2, 0),
            (ComputePhase::LiveInit, 10, 9),
            (ComputePhase::LiveUpdate, 11, 9),
            (ComputePhase::LiveInit, 10, 9),
        ]
    );
}
