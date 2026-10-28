//! Test-only fault-injection indicator (never shipped).
//!
//! Parameter `fault` chooses the behaviour on the `trigger`-th compute call
//! (1-based, counted per instance): `none | error | panic | abort | hang |
//! bad_output | stdout_noise | segfault`. `create_fail=true` fails `create`.

use cvd_indicator_sdk::*;
use std::collections::BTreeMap;

pub struct Fault {
    fault: String,
    trigger: i64,
    calls: i64,
}

impl Indicator for Fault {
    fn descriptor() -> Descriptor {
        Descriptor {
            id: "test.fault".into(),
            version: "0.0.1".into(),
            name: "Fault fixture".into(),
            description: String::new(),
            abi: abi::ABI_VERSION,
            context: ContextSpec { bars: 2, per_tf: Default::default() },
            params: vec![
                ParamSpec {
                    key: "fault".into(),
                    label: String::new(),
                    kind: ParamKind::Choice {
                        default: "none".into(),
                        options: ["none", "error", "panic", "abort", "hang", "bad_output", "stdout_noise", "segfault"]
                            .map(String::from)
                            .to_vec(),
                    },
                },
                ParamSpec {
                    key: "trigger".into(),
                    label: String::new(),
                    kind: ParamKind::Int { default: 1, min: 1, max: 1_000_000 },
                },
                ParamSpec { key: "create_fail".into(), label: String::new(), kind: ParamKind::Bool { default: false } },
            ],
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

    fn new(params: &ParamValues) -> Result<Self, PluginError> {
        if params.bool("create_fail") == Some(true) {
            return Err(PluginError::invalid_params("fixture create failure"));
        }
        Ok(Self {
            fault: params.choice("fault").unwrap_or("none").into(),
            trigger: params.int("trigger").unwrap_or(1),
            calls: 0,
        })
    }

    fn compute(&mut self, _ctx: &ComputeContext<'_>, bars: &[InputBar], from: usize) -> Result<SeriesSet, PluginError> {
        self.calls += 1;
        let values: Vec<Option<f64>> = bars[from..].iter().map(|b| Some(b.price_close)).collect();
        let mut s = BTreeMap::new();
        if self.calls == self.trigger {
            match self.fault.as_str() {
                "error" => return Err(PluginError::compute("fixture error")),
                "panic" => panic!("fixture panic"),
                "abort" => std::process::abort(),
                "hang" => loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                },
                "segfault" => unsafe {
                    std::ptr::null_mut::<u64>().write_volatile(1);
                },
                "bad_output" => {
                    s.insert("close".into(), SeriesValues::Line { values: vec![Some(1.0); values.len() + 1] });
                    return Ok(s);
                }
                "stdout_noise" => {
                    println!("garbage on stdout that must not corrupt the protocol");
                }
                _ => {}
            }
        }
        s.insert("close".into(), SeriesValues::Line { values });
        Ok(s)
    }
}

export_indicator!(Fault);
