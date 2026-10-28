//! Test-only library exporting an entry point with a wrong ABI version and a
//! truncated function table. The runner must reject it without calling any
//! function pointer.

#[repr(C)]
pub struct BadTable {
    abi_version: u32,
    struct_size: u32,
}

static TABLE: BadTable = BadTable { abi_version: 99, struct_size: 8 };

/// # Safety
/// Fixture only.
#[no_mangle]
pub unsafe extern "C" fn cvd_plugin_entry_v1(_host: u32) -> *const BadTable {
    &TABLE
}
