//! Self-contained synthetic hostile `cvd-wasm` v1 module for host tests.
//! It touches nothing outside its own linear memory: no imports exist. The
//! misbehaviour is selected by the params JSON passed to `cvd_create`:
//! `"loop"` (never returns), `"trap"` (unreachable), `"badptr"`/`"hugelen"`
//! (invalid result region), `"badout"` (well-formed JSON violating the axis).

#![allow(clippy::missing_safety_doc, static_mut_refs)]

use std::sync::atomic::{AtomicU32, Ordering};

static MODE: AtomicU32 = AtomicU32::new(0);
static mut RESULT: Vec<u8> = Vec::new();
static mut REGION: (usize, usize) = (0, 0);

const DESC: &str = r#"{"id":"hostile.fixture","version":"1.0.0","name":"Hostile fixture","abi":1,"context":{"bars":0},"outputs":[{"key":"v","label":"v","pane":"price","type":"line"}],"live":false}"#;

fn set(bytes: Vec<u8>) {
    // SAFETY: wasm32 module is single-threaded.
    unsafe {
        RESULT = bytes;
        let r = &*std::ptr::addr_of!(RESULT);
        REGION = (r.as_ptr() as usize, r.len());
    }
}

#[no_mangle]
pub extern "C" fn cvd_wasm_abi_version() -> u32 {
    1
}
#[no_mangle]
pub extern "C" fn cvd_alloc(len: usize) -> usize {
    cvd_indicator_sdk::wasm::raw::alloc_bytes(len)
}
#[no_mangle]
pub unsafe extern "C" fn cvd_dealloc(ptr: usize, len: usize) {
    if MODE.load(Ordering::SeqCst) == 6 {
        // Legal per ABI: the result is only valid until the next export call.
        let r = &mut *std::ptr::addr_of_mut!(RESULT);
        r.iter_mut().for_each(|b| *b = b'#');
    }
    cvd_indicator_sdk::wasm::raw::dealloc_bytes(ptr, len)
}
#[no_mangle]
pub extern "C" fn cvd_describe() -> i32 {
    set(DESC.as_bytes().to_vec());
    0
}
#[no_mangle]
pub unsafe extern "C" fn cvd_create(ptr: usize, len: usize) -> i32 {
    let p = cvd_indicator_sdk::wasm::raw::input(ptr, len).unwrap_or(&[]);
    let s = std::str::from_utf8(p).unwrap_or("");
    let m =
        ["loop", "trap", "badptr", "hugelen", "badout", "clobber", "grow"].iter().position(|k| s.contains(k)).map_or(0, |i| i as u32 + 1);
    MODE.store(m, Ordering::SeqCst);
    set(Vec::new());
    0
}
#[no_mangle]
pub extern "C" fn cvd_invoke(_op: u32, _ptr: usize, _len: usize) -> i32 {
    match MODE.load(Ordering::SeqCst) {
        1 => loop {
            std::hint::black_box(MODE.load(Ordering::Relaxed));
        },
        2 => std::process::abort(),
        3 => unsafe {
            REGION = (0xFFFF_FF00, 0x200);
            0
        },
        4 => unsafe {
            REGION = (16, 0x7FFF_FFFF);
            0
        },
        6 => {
            set(br#"{"times":[],"series":{"v":{"type":"line","values":[]}}}"#.to_vec());
            0
        }
        7 => {
            // memory.grow returns usize::MAX once the declared maximum is hit.
            #[cfg(target_arch = "wasm32")]
            while core::arch::wasm32::memory_grow(0, 1) != usize::MAX {}
            #[cfg(target_arch = "wasm32")]
            if core::arch::wasm32::memory_size(0) > 64 {
                std::process::abort(); // cap not enforced: surfaces as `trap`
            }
            set(br#"{"times":[],"series":{"v":{"type":"line","values":[]}}}"#.to_vec());
            0
        }
        _ => {
            set(br#"{"times":[1,2,3],"series":{"v":{"type":"line","values":[1.0]}}}"#.to_vec());
            0
        }
    }
}
#[no_mangle]
pub extern "C" fn cvd_destroy() {}
#[no_mangle]
pub extern "C" fn cvd_result_ptr() -> usize {
    unsafe { REGION.0 }
}
#[no_mangle]
pub extern "C" fn cvd_result_len() -> usize {
    unsafe { REGION.1 }
}
