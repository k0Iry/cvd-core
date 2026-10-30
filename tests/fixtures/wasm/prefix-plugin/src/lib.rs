//! PUBLIC generic window-origin-sensitive fixture (no private algorithm).
//! value[i] = scale * (close[0] + ... + close[i]) summed sequentially from the
//! FIRST bar of the input window the host sent. Its numbers therefore depend
//! on the request origin (trimmed_from), which is exactly what the lane-B
//! paging/warm-up witness must pin. Declares 50 context bars.
//! Feature `v2`: same id, version 2.0.0, rejects an ODD `scale` at create —
//! the default (2) passes, a migrated user value such as 3 is rejected.
use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

pub const CONTEXT: u32 = 50;

pub struct Prefix {
    scale: f64,
}

impl Indicator for Prefix {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "fixture.prefix".into(),
            version: if cfg!(feature = "v2") { "2.0.0" } else { "1.0.0" }.into(),
            name: "Prefix fixture".into(),
            description: "public window-origin-sensitive test fixture".into(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: CONTEXT, per_tf: Default::default() },
            params: vec![ParamSpec {
                key: "scale".into(),
                label: "Scale".into(),
                kind: ParamKind::Int { default: if cfg!(feature = "v2") { 2 } else { 1 }, min: 1, max: 100 },
            }],
            inputs: vec![InputField::Price],
            outputs: vec![OutputSpec {
                key: "sum".into(),
                label: "Prefix".into(),
                pane: Pane::Separate,
                pane_group: None,
                colors: vec![],
                widths: vec![],
                readout_only: false,
                kind: OutputKind::Line,
            }],
            live: false,
        }
    }
    fn new(p: &ParamValues) -> Result<Self, PluginError> {
        let scale = p.int("scale").unwrap_or(1);
        if cfg!(feature = "v2") && scale % 2 != 0 {
            return Err(PluginError::invalid_params("v2 requires an even scale"));
        }
        Ok(Prefix { scale: scale as f64 })
    }
    fn compute(&mut self, _: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        let mut acc = 0.0f64;
        let mut out = Vec::with_capacity(bars.len() - from);
        for (i, b) in bars.iter().enumerate() {
            acc += b.price_close;
            if i >= from {
                out.push(Some(self.scale * acc));
            }
        }
        let mut s = BTreeMap::new();
        s.insert("sum".into(), SeriesValues::Line { values: out });
        Ok(s)
    }
}

#[cfg(target_arch = "wasm32")]
export_indicator!(Prefix);
