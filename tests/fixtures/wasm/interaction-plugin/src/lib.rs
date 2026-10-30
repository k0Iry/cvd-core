//! TEST-ONLY host-owned PUBLIC protocol fixture for the self-contained core
//! interaction gate. Mirrors the public Apache-2.0 `cvd`/`flow` descriptors
//! (ids are deliberate aliases) so the real app/Worker/render/interaction path
//! runs from a clean core checkout. Not a product plugin; never shipped.
#[cfg(any(all(feature = "cvd", feature = "flow"), all(feature = "cvd", feature = "ctxdemo"), all(feature = "flow", feature = "ctxdemo")))]
compile_error!("enable exactly one of `cvd` / `flow` / `ctxdemo`");
#[cfg(feature = "cvd")]
mod cvd {
//! `cvd` — the public CVD line.
//!
//! Projects the host's **established** cumulative volume delta
//! (`cvd_usdt` in source mode, `ac` in aggregate mode). The host owns the
//! canonical cumulative series, its seed and its source/market boundaries;
//! this library never re-accumulates per-bar deltas, so it cannot drift from
//! persistence and needs no warm-up context.

use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

pub const ID: &str = "cvd";
pub const OUTPUT: &str = "cvd";

pub struct Cvd;

impl Indicator for Cvd {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: ID.into(),
            version: "0.0.0-test".into(),
            name: "TEST-ONLY CVD alias fixture".into(),
            description: "Established cumulative taker quote delta from the host (projection, not recomputed)".into(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: 0, per_tf: Default::default() },
            params: vec![],
            inputs: vec![InputField::Cvd, InputField::AggregateCvd],
            outputs: vec![OutputSpec {
                key: OUTPUT.into(),
                label: "CVD".into(),
                pane: Pane::Separate,
                pane_group: None,
                colors: vec!["#56ccf2".into()],
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

    fn compute(
        &mut self,
        ctx: &ComputeContext<'_>,
        bars: &[InputBar],
        from: usize,
    ) -> Result<SeriesSet, PluginError> {
        let aggregate = ctx.scope.mode == Mode::Aggregate;
        let values = bars[from..]
            .iter()
            .map(|b| Some(if aggregate { b.ac } else { b.cvd_usdt }))
            .collect();
        Ok(BTreeMap::from([(
            OUTPUT.to_string(),
            SeriesValues::Line { values },
        )]))
    }
}

#[cfg(target_arch = "wasm32")]
export_indicator!(Cvd);


}
#[cfg(any(feature = "flow", feature = "ctxdemo"))]
mod flow {
//! `flow` — per-bar taker flow, two alternative views in one `flow` pane.
//!
//! * `delta` (histogram): projection of the host's per-bar delta
//!   (`delta_usdt` / `ad`).
//! * `total` (readout-only line): `buy + sell` quote turnover, same
//!   availability as `buy_sell`; shown in the Flow readout, never drawn.
//! * `buy_sell` (dual_range around 0): `upper = buy`, `lower = -sell` from the
//!   quote buy/sell turnover (`buy_volume_usdt`/`sell_volume_usdt`, aggregate
//!   `agb`/`ags`). Emitted only when the components are non-negative and
//!   reconcile with the delta of the same scope; otherwise `null` (a gap, never
//!   a fake zero). A true zero bar stays zero. Aggregate mode reads only the
//!   aggregate fields — it never falls back to single-source values.
//!
//! Values are nominal USD/USDT/USDC taker quote turnover; no FX conversion.
//! Buy/Sell are not capital inflows/outflows.

use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

#[cfg(feature = "flow")]
pub const ID: &str = "flow";
#[cfg(feature = "flow")]
const NAME: &str = "TEST-ONLY Flow alias fixture";
#[cfg(feature = "flow")]
const CONTEXT_BARS: u32 = 0;
// Coverage variant: same public algorithm, distinct id, declared warm-up.
#[cfg(feature = "ctxdemo")]
pub const ID: &str = "ctxdemo";
#[cfg(feature = "ctxdemo")]
const NAME: &str = "TEST-ONLY public context-demand fixture";
#[cfg(feature = "ctxdemo")]
const CONTEXT_BARS: u32 = 2000;
pub const DELTA: &str = "delta";
pub const BUY_SELL: &str = "buy_sell";
pub const TOTAL: &str = "total";

/// Relative tolerance for `buy - sell == delta`, with an absolute floor.
pub const RECONCILE_RELATIVE: f64 = 1e-9;
pub const RECONCILE_ABSOLUTE: f64 = 1e-6;

pub fn reconciles(buy: f64, sell: f64, delta: f64) -> bool {
    if !(buy.is_finite() && sell.is_finite() && delta.is_finite()) || buy < 0.0 || sell < 0.0 {
        return false;
    }
    let scale = 1f64.max(buy.abs()).max(sell.abs()).max(delta.abs());
    ((buy - sell) - delta).abs() <= RECONCILE_ABSOLUTE.max(scale * RECONCILE_RELATIVE)
}

/// `(upper, lower)` for one bar, or `None` when unavailable.
pub fn buy_sell(buy: f64, sell: f64, delta: f64) -> Option<(f64, f64)> {
    reconciles(buy, sell, delta).then(|| (buy, if sell == 0.0 { 0.0 } else { -sell }))
}

pub struct Flow;

impl Indicator for Flow {
    fn descriptor() -> Descriptor {
        let spec = |key: &str, label: &str, kind, readout_only| OutputSpec {
            key: key.into(),
            label: label.into(),
            pane: Pane::Separate,
            pane_group: Some("flow".into()),
            colors: vec!["#27ae60".into(), "#eb5757".into()],
            widths: vec![],
            readout_only,
            kind,
        };
        Descriptor {
            id: ID.into(),
            version: "0.0.0-test".into(),
            name: NAME.into(),
            description:
                "Per-bar Delta, or Buy/Sell nominal taker quote turnover (no FX conversion)".into(),
            abi: abi::ABI_VERSION,
            context: ContextSpec {
                bars: CONTEXT_BARS,
                per_tf: Default::default(),
            },
            params: vec![],
            inputs: vec![
                InputField::Delta,
                InputField::AggregateDelta,
                InputField::QuoteVolume,
                InputField::AggregateQuoteVolume,
            ],
            // Declaration order = default UI mode: Delta first.
            outputs: vec![
                spec(DELTA, "Delta", OutputKind::Histogram, false),
                spec(BUY_SELL, "Buy/Sell", OutputKind::DualRange, false),
                // Not drawn: quote turnover total shown in the Flow readout in
                // both visual modes (same availability as Buy/Sell).
                spec(TOTAL, "Total", OutputKind::Line, true),
            ],
            live: true,
        }
    }

    fn new(_params: &ParamValues) -> Result<Self, PluginError> {
        Ok(Self)
    }

    fn compute(
        &mut self,
        ctx: &ComputeContext<'_>,
        bars: &[InputBar],
        from: usize,
    ) -> Result<SeriesSet, PluginError> {
        let aggregate = ctx.scope.mode == Mode::Aggregate;
        let n = bars.len() - from;
        let (mut delta, mut upper, mut lower, mut total) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        for b in &bars[from..] {
            let (buy, sell, d) = if aggregate {
                (b.agb, b.ags, b.ad)
            } else {
                (b.buy_volume_usdt, b.sell_volume_usdt, b.delta_usdt)
            };
            delta.push(Some(d));
            let pair = buy_sell(buy, sell, d);
            upper.push(pair.map(|p| p.0));
            lower.push(pair.map(|p| p.1));
            total.push(pair.map(|_| buy + sell));
        }
        Ok(BTreeMap::from([
            (DELTA.to_string(), SeriesValues::Histogram { values: delta }),
            (
                BUY_SELL.to_string(),
                SeriesValues::DualRange { upper, lower },
            ),
            (TOTAL.to_string(), SeriesValues::Line { values: total }),
        ]))
    }
}

#[cfg(target_arch = "wasm32")]
export_indicator!(Flow);


}
