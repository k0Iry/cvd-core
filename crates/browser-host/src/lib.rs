//! Rust browser host for `cvd-wasm` v1 indicator plugins.
//!
//! Compiled to wasm32 and loaded by the thin trusted bridge in
//! `static/wasm-host/`. It never interprets plugin code: the browser's native
//! `WebAssembly` engine executes each plugin in its own Worker/memory, while
//! this crate validates module structure, the ABI lifecycle, memory regions,
//! descriptors and numerical outputs, and fences asynchronous responses.

pub mod contract;
pub mod fence;
#[doc(hidden)]
pub mod hostile_modules;
pub mod module_check;

#[cfg(target_arch = "wasm32")]
mod ffi;

use serde::Serialize;

/// Host-side error with a stable machine code (see CONTRACT.md §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostError {
    pub code: String,
    pub message: String,
}

impl HostError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Deadline defaults (milliseconds) enforced by the controlling thread.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct HostLimits {
    pub compile_ms: u32,
    pub instantiate_ms: u32,
    pub compute_ms: u32,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self { compile_ms: 10_000, instantiate_ms: 5_000, compute_ms: 5_000 }
    }
}
