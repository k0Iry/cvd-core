//! Stable, C-compatible ABI (version 1) between the runner and an indicator
//! dynamic library.
//!
//! Only `repr(C)` structs, raw pointers, fixed-width integers and function
//! pointers cross this boundary. No `Vec`, `String`, trait objects or Rust
//! enums are exchanged. Every byte buffer handed to the host is owned by the
//! library and must be released with [`CvdPluginApiV1::free_buffer`] from the
//! same library.

use std::ffi::c_void;

/// ABI major version implemented by this SDK.
pub const ABI_VERSION: u32 = 1;

/// Exported symbol name (NUL-terminated) resolved by the runner.
pub const ENTRY_SYMBOL: &[u8] = b"cvd_plugin_entry_v1\0";

/// Successful call; output buffer holds a JSON document.
pub const STATUS_OK: i32 = 0;
/// Plugin rejected the request; output buffer holds a JSON [`crate::model::PluginError`].
pub const STATUS_ERROR: i32 = 1;
/// A Rust panic was caught inside the library boundary; buffer holds an error JSON.
pub const STATUS_PANIC: i32 = 2;
/// Null pointers / bad handle / unknown operation.
pub const STATUS_INVALID_ARGUMENT: i32 = 3;

/// Stateless historical-window computation (`HistoryRequest` body).
pub const OP_HISTORY: u32 = 1;
/// Initialise a persistent live instance (`LiveInit` body).
pub const OP_LIVE_INIT: u32 = 2;
/// Apply an append/replace delta to a live instance (`LiveUpdate` body).
pub const OP_LIVE_UPDATE: u32 = 3;

/// Library-owned byte buffer. `ptr` may be null when `len == 0`.
#[repr(C)]
#[derive(Debug)]
pub struct CvdBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl CvdBuffer {
    pub const fn empty() -> Self {
        Self { ptr: std::ptr::null_mut(), len: 0, cap: 0 }
    }

    /// Transfer ownership of a Rust vector to the foreign side.
    pub fn from_vec(v: Vec<u8>) -> Self {
        let mut v = std::mem::ManuallyDrop::new(v);
        Self { ptr: v.as_mut_ptr(), len: v.len(), cap: v.capacity() }
    }

    /// # Safety
    /// Must only be called by the library that produced the buffer, exactly once.
    pub unsafe fn into_vec(self) -> Option<Vec<u8>> {
        if self.ptr.is_null() {
            return None;
        }
        Some(Vec::from_raw_parts(self.ptr, self.len, self.cap))
    }

    /// # Safety
    /// Buffer must be live (not yet freed).
    pub unsafe fn as_slice(&self) -> &[u8] {
        if self.ptr.is_null() || self.len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(self.ptr, self.len)
        }
    }
}

/// Function table returned by `cvd_plugin_entry_v1`.
///
/// `struct_size` lets future minor revisions append fields: the runner rejects
/// tables smaller than the v1 layout.
#[repr(C)]
pub struct CvdPluginApiV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    /// Writes the JSON [`crate::model::Descriptor`] into `out`.
    pub describe: unsafe extern "C" fn(out: *mut CvdBuffer) -> i32,
    /// Creates an opaque instance from JSON parameters. On error, `out_error`
    /// receives an error JSON and `*out_handle` stays null.
    pub create: unsafe extern "C" fn(
        params: *const u8,
        params_len: usize,
        out_handle: *mut *mut c_void,
        out_error: *mut CvdBuffer,
    ) -> i32,
    /// Runs operation `op` (`OP_*`) with a JSON body. Output (result or error)
    /// is written into `out`.
    pub invoke: unsafe extern "C" fn(
        handle: *mut c_void,
        op: u32,
        input: *const u8,
        input_len: usize,
        out: *mut CvdBuffer,
    ) -> i32,
    /// Destroys an instance created by `create`. Null is ignored.
    pub destroy: unsafe extern "C" fn(handle: *mut c_void),
    /// Releases a buffer previously written by this library.
    pub free_buffer: unsafe extern "C" fn(buffer: CvdBuffer),
}

/// Signature of the exported entry point. Returns null when the requested host
/// ABI version is not supported.
pub type EntryFnV1 = unsafe extern "C" fn(host_abi_version: u32) -> *const CvdPluginApiV1;

pub const API_V1_SIZE: u32 = std::mem::size_of::<CvdPluginApiV1>() as u32;
