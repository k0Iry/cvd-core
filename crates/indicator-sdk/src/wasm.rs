//! Standalone browser WASM ABI (`cvd-wasm` v1) for indicator modules.
//!
//! See `docs/plugin-wasm-v1.md`. Only `i32` scalars cross the
//! module boundary: no function pointers, no Rust types, no imports.
//!
//! One runtime ([`WasmGlue`]) serves every [`Indicator`] domain; the domain's
//! [`WasmSession`] ([`BarSession`], [`BookSession`]) supplies request
//! semantics. The glue is target independent so its behaviour is unit-tested
//! natively; [`crate::export_indicator!`] only emits the `extern "C"` exports on
//! `target_arch = "wasm32"`.

use crate::abi::{OP_BOOK, OP_HISTORY, OP_LIVE_INIT, OP_LIVE_UPDATE, STATUS_ERROR, STATUS_INVALID_ARGUMENT, STATUS_OK};
use crate::author::Instance;
use crate::book::BookSnapshot;
use crate::model::{codes, resolve_params, Descriptor, PluginError};
use crate::plugin::{Bars, Book, Domain, Indicator};

/// WASM ABI version returned by `cvd_wasm_abi_version`.
pub const WASM_ABI_VERSION: u32 = 1;
/// Maximum single input buffer the module will allocate for the host.
pub const MAX_INPUT_BYTES: usize = 32 << 20;
/// Maximum result the host accepts.
pub const MAX_OUTPUT_BYTES: usize = 32 << 20;
/// Maximum descriptor JSON.
pub const MAX_DESCRIPTOR_BYTES: usize = 256 << 10;
/// Maximum params JSON.
pub const MAX_PARAMS_BYTES: usize = 64 << 10;
/// Maximum module file size.
pub const MAX_MODULE_BYTES: usize = 16 << 20;
/// Maximum linear memory after any call.
pub const MAX_MEMORY_BYTES: u64 = 512 << 20;
/// Maximum initial memory pages declared by a module (16 MiB).
pub const MAX_INITIAL_PAGES: u64 = 256;
/// Maximum declared maximum pages (512 MiB).
pub const MAX_DECLARED_PAGES: u64 = 8192;

/// Required function exports and their `(params, results)` arity (all `i32`).
pub const REQUIRED_FUNCS: &[(&str, usize, usize)] = &[
    ("cvd_wasm_abi_version", 0, 1),
    ("cvd_alloc", 1, 1),
    ("cvd_dealloc", 2, 0),
    ("cvd_describe", 0, 1),
    ("cvd_create", 2, 1),
    ("cvd_invoke", 3, 1),
    ("cvd_destroy", 0, 0),
    ("cvd_result_ptr", 0, 1),
    ("cvd_result_len", 0, 1),
];
/// Optional linker-generated global exports that are tolerated.
pub const OPTIONAL_GLOBALS: &[&str] = &["__data_end", "__heap_base"];

/// Domain session adapter behind [`WasmGlue`]: one created plugin instance
/// plus its domain request semantics. Errors are the exact `PluginError` the
/// module reports; the glue owns statuses, caps and the result slot.
pub trait WasmSession: Sized {
    /// Domain admission of the descriptor at `cvd_describe`.
    /// Descriptor/domain agreement, enforced by both `describe` and `create`.
    fn descriptor_ok(d: &Descriptor) -> bool;
    fn create(params: &[u8]) -> Result<Self, PluginError>;
    /// Ops handled by this domain; anything else is `STATUS_INVALID_ARGUMENT`.
    fn accepts(op: u32) -> bool;
    /// Run one accepted op, returning the serialised result.
    fn invoke(&mut self, op: u32, input: &[u8]) -> Result<Vec<u8>, PluginError>;
}

/// Bar session. `OP_HISTORY`
/// always runs on a FRESH `Instance` built from the stored create-params and
/// dropped after the call; `OP_LIVE_*` run on the one persistent live
/// instance, which a History request never touches.
pub struct BarSession<P: Indicator<Domain = Bars>> {
    live: Instance<P>,
    params: Vec<u8>,
}

impl<P: Indicator<Domain = Bars>> WasmSession for BarSession<P> {
    fn descriptor_ok(d: &Descriptor) -> bool {
        !d.is_book()
    }
    fn create(params: &[u8]) -> Result<Self, PluginError> {
        Ok(Self { live: Instance::<P>::create(params)?, params: params.to_vec() })
    }
    fn accepts(op: u32) -> bool {
        matches!(op, OP_HISTORY | OP_LIVE_INIT | OP_LIVE_UPDATE)
    }
    fn invoke(&mut self, op: u32, input: &[u8]) -> Result<Vec<u8>, PluginError> {
        let r = if op == OP_HISTORY {
            // Stateless: fresh instance per request, live instance untouched.
            Instance::<P>::create(&self.params).and_then(|mut fresh| fresh.invoke(op, input))
        } else {
            self.live.invoke(op, input)
        };
        r.and_then(|o| {
            serde_json::to_vec(&o).map_err(|_| PluginError::new(codes::INVALID_OUTPUT, "output serialisation failed"))
        })
    }
}

/// Book session: one plugin instance persisting across `OP_BOOK` snapshots.
pub struct BookSession<P: Indicator<Domain = Book>> {
    inst: P,
}

impl<P: Indicator<Domain = Book>> WasmSession for BookSession<P> {
    fn descriptor_ok(d: &Descriptor) -> bool {
        d.is_book()
    }
    fn create(params: &[u8]) -> Result<Self, PluginError> {
        let d = P::descriptor();
        d.validate()?;
        Ok(Self { inst: P::new(&resolve_params(&d, params)?)? })
    }
    fn accepts(op: u32) -> bool {
        op == OP_BOOK
    }
    fn invoke(&mut self, _op: u32, input: &[u8]) -> Result<Vec<u8>, PluginError> {
        let b = serde_json::from_slice::<BookSnapshot>(input)
            .map_err(|_| PluginError::invalid_input("malformed book body"))?;
        b.check()?;
        let out = self.inst.compute(&b)?;
        out.validate(&b)?;
        serde_json::to_vec(&out).map_err(|_| PluginError::new(codes::INVALID_OUTPUT, "serialisation failed"))
    }
}

type SessionOf<P> = <<P as Indicator>::Domain as Domain>::Session<P>;

/// Per-module-instantiation glue for any [`Indicator`]: at most one session plus
/// the plugin-owned result slot.
pub struct WasmGlue<P: Indicator> {
    inst: Option<SessionOf<P>>,
    result: Vec<u8>,
}

impl<P: Indicator> Default for WasmGlue<P> {
    fn default() -> Self {
        Self::new()
    }
}

fn err_json(e: &PluginError) -> Vec<u8> {
    serde_json::to_vec(e).unwrap_or_default()
}

impl<P: Indicator> WasmGlue<P> {
    pub const fn new() -> Self {
        Self { inst: None, result: Vec::new() }
    }

    pub fn result(&self) -> &[u8] {
        &self.result
    }

    pub fn describe(&mut self) -> i32 {
        let d = P::descriptor();
        match serde_json::to_vec(&d) {
            Ok(v) if v.len() <= MAX_DESCRIPTOR_BYTES && SessionOf::<P>::descriptor_ok(&d) => {
                self.result = v;
                STATUS_OK
            }
            _ => {
                self.result = err_json(&PluginError::new(codes::INCOMPATIBLE, "descriptor serialisation failed"));
                STATUS_ERROR
            }
        }
    }

    pub fn create(&mut self, params: &[u8]) -> i32 {
        self.result.clear();
        if self.inst.is_some() || params.len() > MAX_PARAMS_BYTES {
            return STATUS_INVALID_ARGUMENT;
        }
        // Same descriptor/domain admission as `describe`, before any constructor.
        if !SessionOf::<P>::descriptor_ok(&P::descriptor()) {
            self.result = err_json(&PluginError::new(codes::INCOMPATIBLE, "descriptor does not match domain"));
            return STATUS_ERROR;
        }
        match SessionOf::<P>::create(params) {
            Ok(s) => {
                self.inst = Some(s);
                STATUS_OK
            }
            Err(e) => {
                self.result = err_json(&e);
                STATUS_ERROR
            }
        }
    }

    pub fn invoke(&mut self, op: u32, input: &[u8]) -> i32 {
        self.result.clear();
        if !SessionOf::<P>::accepts(op) || input.len() > MAX_INPUT_BYTES {
            return STATUS_INVALID_ARGUMENT;
        }
        let Some(s) = self.inst.as_mut() else { return STATUS_INVALID_ARGUMENT };
        match s.invoke(op, input) {
            Ok(v) => {
                self.result = v;
                STATUS_OK
            }
            Err(e) => {
                self.result = err_json(&e);
                STATUS_ERROR
            }
        }
    }

    pub fn destroy(&mut self) {
        self.inst = None;
        self.result.clear();
    }
}

/// Raw allocation helpers used by the exported `cvd_alloc`/`cvd_dealloc`.
pub mod raw {
    use std::alloc::{alloc, dealloc, Layout};

    /// Returns 0 for `len == 0`, oversize or allocation failure.
    pub fn alloc_bytes(len: usize) -> usize {
        if len == 0 || len > super::MAX_INPUT_BYTES {
            return 0;
        }
        match Layout::from_size_align(len, 1) {
            // SAFETY: non-zero size layout.
            Ok(l) => unsafe { alloc(l) as usize },
            Err(_) => 0,
        }
    }

    /// # Safety
    /// `ptr/len` must come from a previous successful [`alloc_bytes`] call.
    pub unsafe fn dealloc_bytes(ptr: usize, len: usize) {
        if ptr == 0 || len == 0 || len > super::MAX_INPUT_BYTES {
            return;
        }
        if let Ok(l) = Layout::from_size_align(len, 1) {
            dealloc(ptr as *mut u8, l);
        }
    }

    /// # Safety
    /// `ptr/len` must describe readable module memory (host contract).
    pub unsafe fn input<'a>(ptr: usize, len: usize) -> Option<&'a [u8]> {
        if len == 0 {
            Some(&[])
        } else if ptr == 0 {
            None
        } else {
            Some(std::slice::from_raw_parts(ptr as *const u8, len))
        }
    }
}

/// Emits the `cvd-wasm` v1 exports for a [`Indicator`] of any domain. Internal:
/// use [`crate::export_indicator!`].
#[doc(hidden)]
#[macro_export]
macro_rules! __export_wasm {
    ($ty:ty) => {
        #[doc(hidden)]
        mod __cvd_wasm_exports {
            use super::*;
            use $crate::wasm::{raw, WasmGlue, WASM_ABI_VERSION};
            static GLUE: ::std::sync::Mutex<WasmGlue<$ty>> = ::std::sync::Mutex::new(WasmGlue::new());
            fn glue() -> ::std::sync::MutexGuard<'static, WasmGlue<$ty>> {
                GLUE.lock().unwrap_or_else(|p| p.into_inner())
            }
            #[no_mangle]
            pub extern "C" fn cvd_wasm_abi_version() -> u32 {
                WASM_ABI_VERSION
            }
            #[no_mangle]
            pub extern "C" fn cvd_alloc(len: usize) -> usize {
                raw::alloc_bytes(len)
            }
            #[no_mangle]
            pub unsafe extern "C" fn cvd_dealloc(ptr: usize, len: usize) {
                raw::dealloc_bytes(ptr, len)
            }
            #[no_mangle]
            pub extern "C" fn cvd_describe() -> i32 {
                glue().describe()
            }
            #[no_mangle]
            pub unsafe extern "C" fn cvd_create(ptr: usize, len: usize) -> i32 {
                match raw::input(ptr, len) {
                    Some(p) => glue().create(p),
                    None => $crate::abi::STATUS_INVALID_ARGUMENT,
                }
            }
            #[no_mangle]
            pub unsafe extern "C" fn cvd_invoke(op: u32, ptr: usize, len: usize) -> i32 {
                match raw::input(ptr, len) {
                    Some(p) => glue().invoke(op, p),
                    None => $crate::abi::STATUS_INVALID_ARGUMENT,
                }
            }
            #[no_mangle]
            pub extern "C" fn cvd_destroy() {
                glue().destroy()
            }
            #[no_mangle]
            pub extern "C" fn cvd_result_ptr() -> usize {
                glue().result().as_ptr() as usize
            }
            #[no_mangle]
            pub extern "C" fn cvd_result_len() -> usize {
                glue().result().len()
            }
        }
    };
}
