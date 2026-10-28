//! CVD indicator SDK (ABI v1, protocol v1).
//!
//! * [`abi`] — stable C ABI exported by indicator dynamic libraries.
//! * [`model`] — typed descriptors, canonical input bars and outputs.
//! * [`protocol`] — strict framed runner protocol.
//! * [`author`] — safe [`author::Indicator`] trait and [`export_indicator!`].
//!
//! See `docs/plugin-api-v1.md` for the normative contract.

pub mod abi;
pub mod author;
pub mod model;
pub mod protocol;

pub use author::{ComputeContext, Indicator, SeriesSet};
pub use model::*;

/// SDK crate version, informational only (compatibility is decided by ABI and
/// protocol versions).
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");
