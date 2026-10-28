//! Test-only fixture (never shipped): declares ZERO warm-up context and
//! projects close per bar. Used to exercise empty-tail history pages.

use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

pub struct ZeroContext;

impl Indicator for ZeroContext {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "test.zero_context".into(),
            version: "0.0.1".into(),
            name: "Zero-context fixture".into(),
            description: String::new(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: 0, per_tf: Default::default() },
            params: vec![],
            inputs: vec![InputField::Price, InputField::AggregatePrice],
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

    fn new(_params: &ParamValues) -> Result<Self, PluginError> {
        Ok(Self)
    }

    fn compute(&mut self, ctx: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        let aggregate = matches!(ctx.scope.mode, Mode::Aggregate);
        let values = bars[from..].iter().map(|b| Some(if aggregate { b.apc } else { b.price_close })).collect();
        let mut s = BTreeMap::new();
        s.insert("close".into(), SeriesValues::Line { values });
        Ok(s)
    }
}

export_indicator!(ZeroContext);
