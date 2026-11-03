//! Safe, self-contained mutable-state fixture. Every `compute` call increments
//! a per-instance counter which is folded into the output, so any reuse of an
//! instance across requests (e.g. History on the persistent live instance)
//! becomes numerically visible. Built natively (Rust oracle) and for wasm32.
use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

pub struct Stateful {
    calls: u64,
}

impl Indicator for Stateful {
    type Domain = Bars;
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "fixture.stateful".into(),
            version: "1.0.0".into(),
            name: "Stateful fixture".into(),
            description: String::new(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: 0, per_tf: Default::default() },
            params: vec![],
            inputs: vec![InputField::Price],
            outputs: vec![OutputSpec {
                key: "v".into(),
                label: "v".into(),
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
        Ok(Stateful { calls: 0 })
    }
    fn compute(&mut self, BarsInput { bars, output_from: from, .. }: BarsInput<'_>) -> Result<SeriesSet, PluginError> {
        self.calls += 1;
        let k = self.calls as f64 * 1000.0;
        let mut s = BTreeMap::new();
        s.insert(
            "v".into(),
            SeriesValues::Line { values: bars[from..].iter().map(|b| Some(b.price_close + k)).collect() },
        );
        Ok(s)
    }
}

export_indicator!(Bars: Stateful);
