//! Wire constants shared by the SDK and the browser host: descriptor ABI
//! version, `cvd-wasm` call statuses and operation codes.

/// ABI major version implemented by this SDK.
pub const ABI_VERSION: u32 = 1;

/// Successful call; output buffer holds a JSON document.
pub const STATUS_OK: i32 = 0;
/// Plugin rejected the request; output buffer holds a JSON [`crate::model::PluginError`].
pub const STATUS_ERROR: i32 = 1;
/// Null pointers / bad handle / unknown operation. (`2` is unused.)
pub const STATUS_INVALID_ARGUMENT: i32 = 3;

/// Stateless historical-window computation (`HistoryRequest` body).
pub const OP_HISTORY: u32 = 1;
/// Initialise a persistent live instance (`LiveInit` body).
pub const OP_LIVE_INIT: u32 = 2;
/// Apply an append/replace delta to a live instance (`LiveUpdate` body).
pub const OP_LIVE_UPDATE: u32 = 3;
/// Order-book computation (`BookSnapshot` body, `PriceLevels` result). Only
/// valid for descriptors declaring the `order_book` input; browser WASM only.
pub const OP_BOOK: u32 = 4;
