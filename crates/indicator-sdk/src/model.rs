//! Typed JSON documents exchanged between host, runner and library.
//!
//! Field names of [`InputBar`] intentionally reuse the established CVD bar
//! wire names (`po/ph/pl/price_close`, `apo/aph/apl/apc`, `cvd_usdt/ac`,
//! `delta_usdt/ad`, `bvb/abvb/svb/asvb`, `buy_volume_usdt/agb`,
//! `sell_volume_usdt/ags`, `trades`). Indicator outputs are never part of the
//! input.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------- errors

/// Public, bounded error document. Never contains host diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginError {
    pub code: String,
    pub message: String,
}

pub const MAX_ERROR_MESSAGE: usize = 512;

impl PluginError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut message: String = message.into();
        if message.len() > MAX_ERROR_MESSAGE {
            let mut cut = MAX_ERROR_MESSAGE;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
        }
        Self { code: code.into(), message }
    }
    pub fn invalid_input(m: impl Into<String>) -> Self {
        Self::new(codes::INVALID_INPUT, m)
    }
    pub fn invalid_params(m: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, m)
    }
    pub fn compute(m: impl Into<String>) -> Self {
        Self::new(codes::COMPUTE_FAILED, m)
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for PluginError {}

/// Stable public error codes.
pub mod codes {
    pub const INVALID_INPUT: &str = "invalid_input";
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const COMPUTE_FAILED: &str = "compute_failed";
    pub const PANIC: &str = "plugin_panic";
    pub const SEQUENCE_GAP: &str = "sequence_gap";
    pub const STALE_GENERATION: &str = "stale_generation";
    pub const UNKNOWN_SESSION: &str = "unknown_session";
    pub const LIMIT_EXCEEDED: &str = "limit_exceeded";
    pub const INVALID_OUTPUT: &str = "invalid_output";
    pub const PROTOCOL: &str = "protocol_error";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const TIMEOUT: &str = "timeout";
    pub const CRASHED: &str = "crashed";
    pub const INCOMPATIBLE: &str = "incompatible";
}

// ---------------------------------------------------------------- descriptor

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Descriptor {
    /// Stable identifier, `[a-z0-9_.-]{1,64}`.
    pub id: String,
    /// Semantic version of the indicator implementation.
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// ABI version the library was built for. Must equal [`crate::abi::ABI_VERSION`].
    pub abi: u32,
    pub context: ContextSpec,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// Canonical input fields actually consumed (see [`InputField`]).
    #[serde(default)]
    pub inputs: Vec<InputField>,
    pub outputs: Vec<OutputSpec>,
    /// Whether persistent live instances are supported.
    #[serde(default = "default_true")]
    pub live: bool,
}

fn default_true() -> bool {
    true
}

/// Bounded warm-up the host must provide before the first output bar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContextSpec {
    /// Default number of preceding bars required.
    pub bars: u32,
    /// Per-timeframe overrides keyed by `1m`,`5m`,`15m`,`30m`,`1h`.
    #[serde(default)]
    pub per_tf: BTreeMap<String, u32>,
}

impl ContextSpec {
    pub fn bars_for(&self, tf: &str) -> u32 {
        self.per_tf.get(tf).copied().unwrap_or(self.bars)
    }
    pub fn max_bars(&self) -> u32 {
        self.per_tf.values().copied().fold(self.bars, u32::max)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum InputField {
    Price,
    AggregatePrice,
    Cvd,
    AggregateCvd,
    Delta,
    AggregateDelta,
    BaseVolume,
    AggregateBaseVolume,
    QuoteVolume,
    AggregateQuoteVolume,
    Trades,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamSpec {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(flatten)]
    pub kind: ParamKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParamKind {
    Int { default: i64, min: i64, max: i64 },
    Float { default: f64, min: f64, max: f64 },
    Bool { default: bool },
    Choice { default: String, options: Vec<String> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pane {
    /// Overlay on the price chart.
    Price,
    /// Plugin's own pane (shared by outputs with the same `pane_group`).
    Separate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputSpec {
    pub key: String,
    #[serde(default)]
    pub label: String,
    pub pane: Pane,
    #[serde(default)]
    pub pane_group: Option<String>,
    /// CSS colour hint(s); purely presentational.
    #[serde(default)]
    pub colors: Vec<String>,
    #[serde(flatten)]
    pub kind: OutputKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputKind {
    Line,
    Histogram,
    /// Filled band between `upper` and `lower`.
    DualRange,
    /// `layers` independent horizontal step lines. A `null` breaks a segment;
    /// `tail_marker` asks the renderer to label the last value of each layer.
    SegmentedLines {
        layers: u32,
        #[serde(default)]
        tail_marker: bool,
    },
}

// ---------------------------------------------------------------- params

/// Parameters after defaults/range validation against the descriptor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParamValues(pub Map<String, Value>);

impl ParamValues {
    pub fn int(&self, k: &str) -> Option<i64> {
        self.0.get(k).and_then(Value::as_i64)
    }
    pub fn float(&self, k: &str) -> Option<f64> {
        self.0.get(k).and_then(Value::as_f64)
    }
    pub fn bool(&self, k: &str) -> Option<bool> {
        self.0.get(k).and_then(Value::as_bool)
    }
    pub fn choice(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(Value::as_str)
    }
}

/// Fill defaults and validate raw JSON parameters. Unknown keys are rejected.
pub fn resolve_params(desc: &Descriptor, raw: &[u8]) -> Result<ParamValues, PluginError> {
    let raw: Map<String, Value> = if raw.iter().all(u8::is_ascii_whitespace) {
        Map::new()
    } else {
        serde_json::from_slice(raw)
            .map_err(|e| PluginError::invalid_params(format!("params must be a JSON object: {e}")))?
    };
    for k in raw.keys() {
        if !desc.params.iter().any(|p| &p.key == k) {
            return Err(PluginError::invalid_params(format!("unknown parameter `{k}`")));
        }
    }
    let mut out = Map::new();
    for p in &desc.params {
        let v = raw.get(&p.key);
        let resolved = match &p.kind {
            ParamKind::Int { default, min, max } => {
                let x = match v {
                    None => *default,
                    Some(v) => v
                        .as_i64()
                        .ok_or_else(|| PluginError::invalid_params(format!("`{}` must be an integer", p.key)))?,
                };
                if x < *min || x > *max {
                    return Err(PluginError::invalid_params(format!("`{}` out of range [{min}, {max}]", p.key)));
                }
                Value::from(x)
            }
            ParamKind::Float { default, min, max } => {
                let x = match v {
                    None => *default,
                    Some(v) => v
                        .as_f64()
                        .ok_or_else(|| PluginError::invalid_params(format!("`{}` must be a number", p.key)))?,
                };
                if !x.is_finite() || x < *min || x > *max {
                    return Err(PluginError::invalid_params(format!("`{}` out of range [{min}, {max}]", p.key)));
                }
                Value::from(x)
            }
            ParamKind::Bool { default } => Value::from(match v {
                None => *default,
                Some(v) => {
                    v.as_bool().ok_or_else(|| PluginError::invalid_params(format!("`{}` must be a boolean", p.key)))?
                }
            }),
            ParamKind::Choice { default, options } => {
                let x = match v {
                    None => default.clone(),
                    Some(v) => v
                        .as_str()
                        .ok_or_else(|| PluginError::invalid_params(format!("`{}` must be a string", p.key)))?
                        .to_string(),
                };
                if !options.contains(&x) {
                    return Err(PluginError::invalid_params(format!("`{}` not an allowed option", p.key)));
                }
                Value::from(x)
            }
        };
        out.insert(p.key.clone(), resolved);
    }
    Ok(ParamValues(out))
}

// ---------------------------------------------------------------- input

/// Canonical OHLC + flow bar delivered to indicators. Times are unix seconds of
/// bucket open (matching the chart backbone).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InputBar {
    pub time: i64,
    pub po: f64,
    pub ph: f64,
    pub pl: f64,
    pub price_close: f64,
    pub apo: f64,
    pub aph: f64,
    pub apl: f64,
    pub apc: f64,
    /// Established cumulative CVD (quote units) from persistence; not recomputed.
    pub cvd_usdt: f64,
    pub ac: f64,
    pub delta_usdt: f64,
    pub ad: f64,
    pub bvb: f64,
    pub abvb: f64,
    pub svb: f64,
    pub asvb: f64,
    pub buy_volume_usdt: f64,
    pub agb: f64,
    pub sell_volume_usdt: f64,
    pub ags: f64,
    pub trades: u64,
}

impl InputBar {
    pub fn is_finite(&self) -> bool {
        [
            self.po,
            self.ph,
            self.pl,
            self.price_close,
            self.apo,
            self.aph,
            self.apl,
            self.apc,
            self.cvd_usdt,
            self.ac,
            self.delta_usdt,
            self.ad,
            self.bvb,
            self.abvb,
            self.svb,
            self.asvb,
            self.buy_volume_usdt,
            self.agb,
            self.sell_volume_usdt,
            self.ags,
        ]
        .iter()
        .all(|v| v.is_finite())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Per-source (single exchange/market) view.
    Source,
    /// Aggregate view across the market category.
    Aggregate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Scope {
    pub exchange: String,
    pub market: String,
    pub symbol: String,
    pub tf: String,
    pub mode: Mode,
}

/// Body of `OP_HISTORY`. `bars[..output_from]` is warm-up only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryRequest {
    pub scope: Scope,
    pub revision: u64,
    pub output_from: usize,
    pub bars: Vec<InputBar>,
}

/// Body of `OP_LIVE_INIT`: bounded warm-up window (oldest first).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveInit {
    pub scope: Scope,
    pub revision: u64,
    pub bars: Vec<InputBar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LiveOp {
    /// New bucket strictly after the last known bar.
    Append { bar: InputBar },
    /// Correction of an existing bucket (usually the forming bar). Replaces,
    /// never accumulates.
    Replace { bar: InputBar },
}

/// Body of `OP_LIVE_UPDATE`. `seq` must be exactly previous `seq + 1`
/// (`LiveInit` sets 0).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveUpdate {
    pub revision: u64,
    pub seq: u64,
    pub ops: Vec<LiveOp>,
}

// ---------------------------------------------------------------- output

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SeriesValues {
    Line { values: Vec<Option<f64>> },
    Histogram { values: Vec<Option<f64>> },
    DualRange { upper: Vec<Option<f64>>, lower: Vec<Option<f64>> },
    SegmentedLines { layers: Vec<Vec<Option<f64>>> },
}

/// Result of history / live operations. All columns are aligned to `times`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Output {
    pub times: Vec<i64>,
    pub series: BTreeMap<String, SeriesValues>,
}

fn check_col(key: &str, col: &[Option<f64>], n: usize) -> Result<(), PluginError> {
    if col.len() != n {
        return Err(PluginError::new(codes::INVALID_OUTPUT, format!("`{key}` has {} values, expected {n}", col.len())));
    }
    if col.iter().flatten().any(|v| !v.is_finite()) {
        return Err(PluginError::new(codes::INVALID_OUTPUT, format!("`{key}` contains non-finite value")));
    }
    Ok(())
}

impl Output {
    /// Strictly validate against the descriptor and the expected time axis.
    pub fn validate(&self, desc: &Descriptor, expected_times: Option<&[i64]>) -> Result<(), PluginError> {
        if let Some(t) = expected_times {
            if self.times.as_slice() != t {
                return Err(PluginError::new(codes::INVALID_OUTPUT, "output times do not match requested bars"));
            }
        }
        if self.times.windows(2).any(|w| w[0] >= w[1]) {
            return Err(PluginError::new(codes::INVALID_OUTPUT, "output times must be strictly increasing"));
        }
        let n = self.times.len();
        if self.series.len() != desc.outputs.len() {
            return Err(PluginError::new(codes::INVALID_OUTPUT, "output series set differs from descriptor"));
        }
        for spec in &desc.outputs {
            let v = self
                .series
                .get(&spec.key)
                .ok_or_else(|| PluginError::new(codes::INVALID_OUTPUT, format!("missing output `{}`", spec.key)))?;
            match (&spec.kind, v) {
                (OutputKind::Line, SeriesValues::Line { values })
                | (OutputKind::Histogram, SeriesValues::Histogram { values }) => check_col(&spec.key, values, n)?,
                (OutputKind::DualRange, SeriesValues::DualRange { upper, lower }) => {
                    check_col(&spec.key, upper, n)?;
                    check_col(&spec.key, lower, n)?;
                }
                (OutputKind::SegmentedLines { layers: want, .. }, SeriesValues::SegmentedLines { layers }) => {
                    if layers.len() != *want as usize {
                        return Err(PluginError::new(
                            codes::INVALID_OUTPUT,
                            format!("`{}` layer count mismatch", spec.key),
                        ));
                    }
                    for l in layers {
                        check_col(&spec.key, l, n)?;
                    }
                }
                _ => {
                    return Err(PluginError::new(
                        codes::INVALID_OUTPUT,
                        format!("`{}` kind differs from descriptor", spec.key),
                    ))
                }
            }
        }
        Ok(())
    }
}

impl Descriptor {
    /// Structural validation performed by runner and host at load time.
    pub fn validate(&self) -> Result<(), PluginError> {
        let ok_id = !self.id.is_empty()
            && self.id.len() <= 64
            && self.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b));
        if !ok_id {
            return Err(PluginError::new(codes::INCOMPATIBLE, "invalid indicator id"));
        }
        if self.abi != crate::abi::ABI_VERSION {
            return Err(PluginError::new(codes::INCOMPATIBLE, "descriptor ABI version mismatch"));
        }
        if self.outputs.is_empty() || self.outputs.len() > 64 {
            return Err(PluginError::new(codes::INCOMPATIBLE, "descriptor must declare 1..=64 outputs"));
        }
        let mut keys = std::collections::BTreeSet::new();
        for o in &self.outputs {
            if o.key.is_empty() || !keys.insert(o.key.as_str()) {
                return Err(PluginError::new(codes::INCOMPATIBLE, "duplicate or empty output key"));
            }
            if let OutputKind::SegmentedLines { layers, .. } = o.kind {
                if layers == 0 || layers > 32 {
                    return Err(PluginError::new(codes::INCOMPATIBLE, "segmented_lines layers must be 1..=32"));
                }
            }
        }
        if self.context.max_bars() > crate::protocol::Limits::default().max_context_bars {
            return Err(PluginError::new(codes::INCOMPATIBLE, "declared context exceeds host maximum"));
        }
        let mut pkeys = std::collections::BTreeSet::new();
        for p in &self.params {
            if !pkeys.insert(p.key.as_str()) {
                return Err(PluginError::new(codes::INCOMPATIBLE, "duplicate parameter key"));
            }
        }
        Ok(())
    }
}
