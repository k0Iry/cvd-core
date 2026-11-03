//! CVD indicator SDK (`cvd-wasm` v1).
//!
//! * [`plugin`] — the [`Indicator`] trait (domains [`Bars`] and [`Book`]) and
//!   [`export_indicator!`].
//! * [`abi`] — wire constants (statuses, operation codes).
//! * [`model`] — typed descriptors, canonical input bars and outputs.
//! * [`author`] — bar runtime (history/live windows, output validation).
//! * [`book`] — order-book snapshot/price-level contract.
//! * [`wasm`] — the shared `cvd-wasm` v1 runtime for every domain.
//!
//! See `docs/plugin-api-v1.md` for the normative contract.

pub mod abi;
pub mod author;
/// Live order-book plugin contract (`OP_BOOK`).
pub mod book;
pub mod model;
pub mod plugin;
/// Standalone browser WASM ABI (`cvd-wasm` v1) glue.
pub mod wasm;

pub use author::{ComputeContext, ComputePhase, SeriesSet};
pub use book::{BookSnapshot, PriceLevelRow, PriceLevels};
pub use model::*;
pub use plugin::{Bars, BarsInput, Book, Domain, Indicator};

/// SDK crate version, informational only (compatibility is decided by the
/// `cvd-wasm` ABI version).
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");
