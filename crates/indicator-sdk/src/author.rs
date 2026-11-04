//! Bar-domain runtime: [`Instance`] owns history/live window bookkeeping and
//! output validation for an [`crate::Indicator`] with `type Domain = Bars`.

use crate::abi::{OP_HISTORY, OP_LIVE_INIT, OP_LIVE_UPDATE};
use crate::model::*;
use crate::plugin::{Bars, BarsInput, Indicator};

/// Extra bars retained in a live window beyond the declared context so that
/// recent corrections (`Replace`) can still be applied.
pub const LIVE_REPLACE_SLACK: usize = 64;

/// Which host operation triggered a computation. Supplied explicitly so
/// authors never infer history-vs-live semantics from call counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComputePhase {
    /// Stateless history page (`OP_HISTORY`). `bars` is the full requested
    /// window including warm-up; output covers `bars[output_from..]`.
    History,
    /// First computation of a live session (`OP_LIVE_INIT`). `bars` is the
    /// live window truncated to `context + 1 + LIVE_REPLACE_SLACK`; output is
    /// the last bar only.
    LiveInit,
    /// Recompute after an applied append/replace delta (`OP_LIVE_UPDATE`);
    /// output covers bars from the earliest touched bar onward.
    LiveUpdate,
}

/// Information available to a computation.
#[derive(Debug, Clone)]
pub struct ComputeContext<'a> {
    pub scope: &'a Scope,
    pub revision: u64,
    pub params: &'a ParamValues,
    /// Declared warm-up for this scope's timeframe.
    pub context_bars: usize,
    /// Operation that triggered this computation.
    pub phase: ComputePhase,
}

/// Columns for `bars[output_from..]`, keyed by descriptor output key.
pub type SeriesSet = std::collections::BTreeMap<String, SeriesValues>;

struct LiveState {
    scope: Scope,
    revision: u64,
    seq: u64,
    window: Vec<InputBar>,
    cap: usize,
}

/// Opaque per-session plugin state.
pub struct Instance<P: Indicator<Domain = Bars>> {
    desc: Descriptor,
    params: ParamValues,
    inner: P,
    live: Option<LiveState>,
}

/// Canonical bar admission (strictly increasing positive times, finite
/// values). Public so the browser host applies the identical rule.
pub fn check_bars(bars: &[InputBar]) -> Result<(), PluginError> {
    if bars.windows(2).any(|w| w[0].time >= w[1].time) {
        return Err(PluginError::invalid_input("bar times must be strictly increasing"));
    }
    if bars.iter().any(|b| !b.is_finite() || b.time <= 0) {
        return Err(PluginError::invalid_input("bar contains non-finite value or invalid time"));
    }
    Ok(())
}

impl<P: Indicator<Domain = Bars>> Instance<P> {
    pub fn create(params_json: &[u8]) -> Result<Self, PluginError> {
        let desc = P::descriptor();
        let params = resolve_params(&desc, params_json)?;
        let inner = P::new(&params)?;
        Ok(Self { desc, params, inner, live: None })
    }

    fn run(
        &mut self,
        phase: ComputePhase,
        scope: &Scope,
        revision: u64,
        bars: &[InputBar],
        from: usize,
    ) -> Result<Output, PluginError> {
        let ctx = ComputeContext {
            scope,
            revision,
            params: &self.params,
            context_bars: self.desc.context.bars_for(&scope.tf) as usize,
            phase,
        };
        let series = self.inner.compute(BarsInput { ctx: &ctx, bars, output_from: from })?;
        let out = Output { times: bars[from..].iter().map(|b| b.time).collect(), series };
        out.validate(&self.desc, None)?;
        Ok(out)
    }

    pub fn history(&mut self, req: &HistoryRequest) -> Result<Output, PluginError> {
        if req.output_from > req.bars.len() {
            return Err(PluginError::invalid_input("output_from beyond bars"));
        }
        check_bars(&req.bars)?;
        self.run(ComputePhase::History, &req.scope, req.revision, &req.bars, req.output_from)
    }

    pub fn live_init(&mut self, init: LiveInit) -> Result<Output, PluginError> {
        if !self.desc.live {
            return Err(PluginError::invalid_input("indicator does not support live sessions"));
        }
        check_bars(&init.bars)?;
        let cap = self.desc.context.bars_for(&init.scope.tf) as usize + 1 + LIVE_REPLACE_SLACK;
        let mut window = init.bars;
        if window.len() > cap {
            window.drain(..window.len() - cap);
        }
        let from = window.len().saturating_sub(1);
        let out = self.run(ComputePhase::LiveInit, &init.scope, init.revision, &window, from)?;
        self.live = Some(LiveState { scope: init.scope, revision: init.revision, seq: 0, window, cap });
        Ok(out)
    }

    pub fn live_update(&mut self, upd: LiveUpdate) -> Result<Output, PluginError> {
        let st = self.live.as_mut().ok_or_else(|| PluginError::invalid_input("live session not initialised"))?;
        if upd.revision != st.revision {
            return Err(PluginError::new(codes::STALE_GENERATION, "revision mismatch"));
        }
        if upd.seq != st.seq + 1 {
            return Err(PluginError::new(codes::SEQUENCE_GAP, format!("expected seq {}, got {}", st.seq + 1, upd.seq)));
        }
        // Apply to a scratch copy so a rejected delta leaves state untouched.
        let mut window = st.window.clone();
        let mut touched: Vec<i64> = Vec::new();
        for op in upd.ops {
            match op {
                LiveOp::Append { bar } => {
                    check_bars(std::slice::from_ref(&bar))?;
                    if window.last().is_some_and(|l| l.time >= bar.time) {
                        return Err(PluginError::invalid_input("append must be after last bar"));
                    }
                    touched.push(bar.time);
                    window.push(bar);
                }
                LiveOp::Replace { bar } => {
                    check_bars(std::slice::from_ref(&bar))?;
                    let slot = window
                        .iter_mut()
                        .rev()
                        .find(|b| b.time == bar.time)
                        .ok_or_else(|| PluginError::invalid_input("replace target not in live window"))?;
                    *slot = bar;
                    touched.push(slot.time);
                }
            }
        }
        if window.len() > st.cap {
            window.drain(..window.len() - st.cap);
        }
        touched.sort_unstable();
        touched.dedup();
        let first = touched.first().copied();
        let from = match first {
            None => window.len(),
            Some(t) => window
                .iter()
                .position(|b| b.time == t)
                .ok_or_else(|| PluginError::invalid_input("delta exceeds live window"))?,
        };
        let scope = st.scope.clone();
        let revision = st.revision;
        let out = self.run(ComputePhase::LiveUpdate, &scope, revision, &window, from)?;
        let st = self.live.as_mut().expect("live state");
        st.window = window;
        st.seq = upd.seq;
        Ok(out)
    }

    pub fn invoke(&mut self, op: u32, input: &[u8]) -> Result<Output, PluginError> {
        let parse = |what: &str| PluginError::invalid_input(format!("malformed {what} body"));
        match op {
            OP_HISTORY => self.history(&serde_json::from_slice(input).map_err(|_| parse("history"))?),
            OP_LIVE_INIT => self.live_init(serde_json::from_slice(input).map_err(|_| parse("live init"))?),
            OP_LIVE_UPDATE => self.live_update(serde_json::from_slice(input).map_err(|_| parse("live update"))?),
            _ => Err(PluginError::invalid_input("unknown operation")),
        }
    }
}
