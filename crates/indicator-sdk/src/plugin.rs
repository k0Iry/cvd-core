//! Canonical typed authoring contract shared by every data domain.
//!
//! Implement [`Indicator`] once, pick the domain ([`Bars`] or [`Book`]) and call
//! [`crate::export_indicator!`]. The domain fixes the typed input/output and the
//! session semantics; the SDK owns parameters, validation, result slots and
//! the `cvd-wasm` v1 exports for both domains through one runtime
//! ([`crate::wasm::WasmGlue`]).

use crate::author::{ComputeContext, SeriesSet};
use crate::book::{BookSnapshot, PriceLevels};
use crate::model::{Descriptor, InputBar, ParamValues, PluginError};
use crate::wasm::WasmSession;

/// A data domain: typed input/output plus its stateful session adapter.
pub trait Domain: 'static {
    /// Borrowed input handed to [`Indicator::compute`].
    type Input<'a>;
    /// Declarative result validated by the session.
    type Output;
    /// Session semantics for one created instance (see [`crate::wasm`]).
    type Session<P: Indicator<Domain = Self>>: WasmSession;
}

/// Candle/bar history and live sessions (`OP_HISTORY`, `OP_LIVE_*`).
pub enum Bars {}
/// Live single-source order-book snapshots (`OP_BOOK`, browser only).
pub enum Book {}

/// Input of a bar computation: columns are produced for
/// `bars[output_from..]`; earlier bars are warm-up.
#[derive(Debug, Clone, Copy)]
pub struct BarsInput<'a> {
    pub ctx: &'a ComputeContext<'a>,
    pub bars: &'a [InputBar],
    pub output_from: usize,
}

impl Domain for Bars {
    type Input<'a> = BarsInput<'a>;
    type Output = SeriesSet;
    type Session<P: Indicator<Domain = Self>> = crate::wasm::BarSession<P>;
}

impl Domain for Book {
    type Input<'a> = &'a BookSnapshot;
    type Output = PriceLevels;
    type Session<P: Indicator<Domain = Self>> = crate::wasm::BookSession<P>;
}

/// The single authoring trait for indicators of any domain.
pub trait Indicator: Sized + Send + 'static {
    type Domain: Domain;
    /// Static self-description. Must be deterministic.
    fn descriptor() -> Descriptor;
    /// Construct an instance from validated parameters.
    fn new(params: &ParamValues) -> Result<Self, PluginError>;
    /// Compute the domain output for one request.
    fn compute(
        &mut self,
        input: <Self::Domain as Domain>::Input<'_>,
    ) -> Result<<Self::Domain as Domain>::Output, PluginError>;
}

/// Export an [`Indicator`] as a `cvd-wasm` v1 module:
/// `export_indicator!(Bars: T)` or `export_indicator!(Book: T)`. The exports
/// are emitted on `wasm32` only; native builds drive
/// [`crate::author::Instance`] or [`crate::wasm::WasmGlue`] directly.
#[macro_export]
macro_rules! export_indicator {
    (Bars: $ty:ty) => {
        const _: fn() = || {
            fn bars<P: $crate::Indicator<Domain = $crate::Bars>>() {}
            bars::<$ty>();
        };
        #[cfg(target_arch = "wasm32")]
        $crate::__export_wasm!($ty);
    };
    (Book: $ty:ty) => {
        const _: fn() = || {
            fn book<P: $crate::Indicator<Domain = $crate::Book>>() {}
            book::<$ty>();
        };
        #[cfg(target_arch = "wasm32")]
        $crate::__export_wasm!($ty);
    };
}
