//! Normal-path protocol test fixture (descriptor id `sample.channel`).
//!
//! This library is a local host/runner test fixture exercising every output
//! kind; it is not the recommended starting point for plugin authors. New
//! authors should start from the public `example.range` example:
//! <https://github.com/k0Iry/cvd-indicators/tree/master/examples/range> and
//! <https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md>.
//!
//! * `sma` — simple moving average of close (line, price pane)
//! * `channel` — rolling high/low band (dual range, price pane)
//! * `delta` — projection of the established per-bar delta (histogram)
//! * `extremes` — rolling max/min close as segmented lines with tail markers
//!   and per-layer widths
//! * `total` — readout-only quote volume total (buy + sell), not drawn
//!
//! In `aggregate` mode the aggregate fields (`apc/aph/apl/ad`) are used.

use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

pub struct SampleChannel {
    length: usize,
}

const MAX_LEN: i64 = 500;

impl Indicator for SampleChannel {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "sample.channel".into(),
            version: "1.0.0".into(),
            name: "Sample channel".into(),
            description: "SDK example: SMA, high/low channel, delta histogram, rolling extremes".into(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: MAX_LEN as u32, per_tf: Default::default() },
            params: vec![ParamSpec {
                key: "length".into(),
                label: "Length".into(),
                kind: ParamKind::Int { default: 20, min: 1, max: MAX_LEN },
            }],
            inputs: vec![InputField::Price, InputField::AggregatePrice, InputField::Delta, InputField::AggregateDelta],
            outputs: vec![
                OutputSpec {
                    key: "sma".into(),
                    label: "SMA".into(),
                    pane: Pane::Price,
                    pane_group: None,
                    colors: vec!["#f5a623".into()],
                    widths: vec![],
                    readout_only: false,
                    kind: OutputKind::Line,
                },
                OutputSpec {
                    key: "channel".into(),
                    label: "Channel".into(),
                    pane: Pane::Price,
                    pane_group: None,
                    colors: vec!["rgba(80,140,255,0.15)".into()],
                    widths: vec![],
                    readout_only: false,
                    kind: OutputKind::DualRange,
                },
                OutputSpec {
                    key: "delta".into(),
                    label: "Delta".into(),
                    pane: Pane::Separate,
                    pane_group: Some("flow".into()),
                    colors: vec!["#26a69a".into(), "#ef5350".into()],
                    widths: vec![],
                    readout_only: false,
                    kind: OutputKind::Histogram,
                },
                OutputSpec {
                    key: "extremes".into(),
                    label: "Extremes".into(),
                    pane: Pane::Price,
                    pane_group: None,
                    colors: vec!["#ef5350".into(), "#26a69a".into()],
                    widths: vec![2, 1],
                    readout_only: false,
                    kind: OutputKind::SegmentedLines { layers: 2, tail_marker: true },
                },
                OutputSpec {
                    key: "total".into(),
                    label: "Total (quote)".into(),
                    pane: Pane::Separate,
                    pane_group: Some("flow".into()),
                    colors: vec![],
                    widths: vec![],
                    readout_only: true,
                    kind: OutputKind::Line,
                },
            ],
            live: true,
        }
    }

    fn new(params: &ParamValues) -> Result<Self, PluginError> {
        Ok(Self { length: params.int("length").unwrap_or(20) as usize })
    }

    fn compute(&mut self, ctx: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        let agg = ctx.scope.mode == Mode::Aggregate;
        let close = |b: &InputBar| if agg { b.apc } else { b.price_close };
        let high = |b: &InputBar| if agg { b.aph } else { b.ph };
        let low = |b: &InputBar| if agg { b.apl } else { b.pl };
        let n = self.length;
        let mut total = Vec::new();
        let (mut sma, mut up, mut lo, mut delta, mut mx, mut mn) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for i in from..bars.len() {
            delta.push(Some(if agg { bars[i].ad } else { bars[i].delta_usdt }));
            let b = &bars[i];
            total.push(Some(if agg { b.agb + b.ags } else { b.buy_volume_usdt + b.sell_volume_usdt }));
            if i + 1 < n {
                for v in [&mut sma, &mut up, &mut lo, &mut mx, &mut mn] {
                    v.push(None);
                }
                continue;
            }
            let w = &bars[i + 1 - n..=i];
            sma.push(Some(w.iter().map(close).sum::<f64>() / n as f64));
            up.push(Some(w.iter().map(high).fold(f64::MIN, f64::max)));
            lo.push(Some(w.iter().map(low).fold(f64::MAX, f64::min)));
            mx.push(Some(w.iter().map(close).fold(f64::MIN, f64::max)));
            mn.push(Some(w.iter().map(close).fold(f64::MAX, f64::min)));
        }
        let mut s = BTreeMap::new();
        s.insert("sma".into(), SeriesValues::Line { values: sma });
        s.insert("channel".into(), SeriesValues::DualRange { upper: up, lower: lo });
        s.insert("delta".into(), SeriesValues::Histogram { values: delta });
        s.insert("extremes".into(), SeriesValues::SegmentedLines { layers: vec![mx, mn] });
        s.insert("total".into(), SeriesValues::Line { values: total });
        Ok(s)
    }
}

export_indicator!(SampleChannel);
