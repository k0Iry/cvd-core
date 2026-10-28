//! Safe Rust authoring interface and the `export_indicator!` macro.
//!
//! Authors implement [`Indicator`]; the SDK owns all unsafe ABI glue, panic
//! containment, parameter validation, live window bookkeeping and output
//! validation.

use crate::abi::*;
use crate::model::*;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

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

/// Implement this trait and call `export_indicator!(MyType)`.
pub trait Indicator: Sized + Send + 'static {
    /// Static self-description. Must be deterministic.
    fn descriptor() -> Descriptor;

    /// Construct an instance from validated parameters.
    fn new(params: &ParamValues) -> Result<Self, PluginError>;

    /// Compute output columns for `bars[output_from..]`; earlier bars are
    /// warm-up. Each returned column must have `bars.len() - output_from`
    /// entries. Called for history windows and, in live mode, on the bounded
    /// live window after each delta (only touched bars are requested).
    fn compute(
        &mut self,
        ctx: &ComputeContext<'_>,
        bars: &[InputBar],
        output_from: usize,
    ) -> Result<SeriesSet, PluginError>;
}

struct LiveState {
    scope: Scope,
    revision: u64,
    seq: u64,
    window: Vec<InputBar>,
    cap: usize,
}

/// Opaque per-handle state (boxed behind the ABI handle).
pub struct Instance<T: Indicator> {
    desc: Descriptor,
    params: ParamValues,
    inner: T,
    live: Option<LiveState>,
}

fn check_bars(bars: &[InputBar]) -> Result<(), PluginError> {
    if bars.windows(2).any(|w| w[0].time >= w[1].time) {
        return Err(PluginError::invalid_input("bar times must be strictly increasing"));
    }
    if bars.iter().any(|b| !b.is_finite() || b.time <= 0) {
        return Err(PluginError::invalid_input("bar contains non-finite value or invalid time"));
    }
    Ok(())
}

impl<T: Indicator> Instance<T> {
    pub fn create(params_json: &[u8]) -> Result<Self, PluginError> {
        let desc = T::descriptor();
        let params = resolve_params(&desc, params_json)?;
        let inner = T::new(&params)?;
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
        let series = self.inner.compute(&ctx, bars, from)?;
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
        let out = {
            let ctx_bars = self.desc.context.bars_for(&scope.tf) as usize;
            let ctx = ComputeContext {
                scope: &scope,
                revision,
                params: &self.params,
                context_bars: ctx_bars,
                phase: ComputePhase::LiveUpdate,
            };
            let series = self.inner.compute(&ctx, &window, from)?;
            let out = Output { times: window[from..].iter().map(|b| b.time).collect(), series };
            out.validate(&self.desc, None)?;
            out
        };
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

// ---------------------------------------------------------------- ABI glue

fn panic_error() -> Vec<u8> {
    serde_json::to_vec(&PluginError::new(codes::PANIC, "indicator panicked")).unwrap_or_default()
}

unsafe fn write_out(out: *mut CvdBuffer, bytes: Vec<u8>) {
    if !out.is_null() {
        out.write(CvdBuffer::from_vec(bytes));
    }
}

unsafe fn input_slice<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        Some(&[])
    } else if ptr.is_null() {
        None
    } else {
        Some(std::slice::from_raw_parts(ptr, len))
    }
}

#[doc(hidden)]
pub unsafe extern "C" fn describe<T: Indicator>(out: *mut CvdBuffer) -> i32 {
    if out.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    match catch_unwind(|| serde_json::to_vec(&T::descriptor())) {
        Ok(Ok(v)) => {
            write_out(out, v);
            STATUS_OK
        }
        Ok(Err(_)) => STATUS_ERROR,
        Err(_) => {
            write_out(out, panic_error());
            STATUS_PANIC
        }
    }
}

#[doc(hidden)]
pub unsafe extern "C" fn create<T: Indicator>(
    params: *const u8,
    len: usize,
    out_handle: *mut *mut c_void,
    out_err: *mut CvdBuffer,
) -> i32 {
    if out_handle.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    out_handle.write(std::ptr::null_mut());
    let Some(params) = input_slice(params, len) else { return STATUS_INVALID_ARGUMENT };
    match catch_unwind(|| Instance::<T>::create(params)) {
        Ok(Ok(inst)) => {
            out_handle.write(Box::into_raw(Box::new(inst)) as *mut c_void);
            STATUS_OK
        }
        Ok(Err(e)) => {
            write_out(out_err, serde_json::to_vec(&e).unwrap_or_default());
            STATUS_ERROR
        }
        Err(_) => {
            write_out(out_err, panic_error());
            STATUS_PANIC
        }
    }
}

#[doc(hidden)]
pub unsafe extern "C" fn invoke<T: Indicator>(
    handle: *mut c_void,
    op: u32,
    input: *const u8,
    len: usize,
    out: *mut CvdBuffer,
) -> i32 {
    if handle.is_null() || out.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    let Some(input) = input_slice(input, len) else { return STATUS_INVALID_ARGUMENT };
    let inst = &mut *(handle as *mut Instance<T>);
    let r = catch_unwind(AssertUnwindSafe(|| {
        inst.invoke(op, input).and_then(|o| {
            serde_json::to_vec(&o).map_err(|_| PluginError::new(codes::INVALID_OUTPUT, "output serialisation failed"))
        })
    }));
    match r {
        Ok(Ok(v)) => {
            write_out(out, v);
            STATUS_OK
        }
        Ok(Err(e)) => {
            write_out(out, serde_json::to_vec(&e).unwrap_or_default());
            STATUS_ERROR
        }
        Err(_) => {
            // State may be inconsistent after a panic; drop live window so the
            // host must re-open the session.
            inst.live = None;
            write_out(out, panic_error());
            STATUS_PANIC
        }
    }
}

#[doc(hidden)]
pub unsafe extern "C" fn destroy<T: Indicator>(handle: *mut c_void) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(handle as *mut Instance<T>))));
    }
}

#[doc(hidden)]
pub unsafe extern "C" fn free_buffer(buf: CvdBuffer) {
    let _ = catch_unwind(AssertUnwindSafe(|| drop(buf.into_vec())));
}

/// Export an [`Indicator`] implementation as `cvd_plugin_entry_v1`.
///
/// ```ignore
/// cvd_indicator_sdk::export_indicator!(MyIndicator);
/// ```
#[macro_export]
macro_rules! export_indicator {
    ($ty:ty) => {
        #[doc(hidden)]
        static __CVD_PLUGIN_API_V1: $crate::abi::CvdPluginApiV1 = $crate::abi::CvdPluginApiV1 {
            abi_version: $crate::abi::ABI_VERSION,
            struct_size: $crate::abi::API_V1_SIZE,
            describe: $crate::author::describe::<$ty>,
            create: $crate::author::create::<$ty>,
            invoke: $crate::author::invoke::<$ty>,
            destroy: $crate::author::destroy::<$ty>,
            free_buffer: $crate::author::free_buffer,
        };

        /// # Safety
        /// Called by the CVD indicator runner only.
        #[no_mangle]
        pub unsafe extern "C" fn cvd_plugin_entry_v1(host_abi_version: u32) -> *const $crate::abi::CvdPluginApiV1 {
            if host_abi_version != $crate::abi::ABI_VERSION {
                return ::std::ptr::null();
            }
            &__CVD_PLUGIN_API_V1
        }
    };
}
