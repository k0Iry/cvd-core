//! Pre-compilation structural validation of an untrusted plugin module
//! (`cvd-wasm` v1 profile). Runs on the raw bytes BEFORE the browser compiles
//! or instantiates anything, so a start function, import or memory
//! declaration cannot bypass policy.

use crate::HostError;
use cvd_indicator_sdk::wasm::{
    MAX_DECLARED_PAGES, MAX_INITIAL_PAGES, MAX_MODULE_BYTES, OPTIONAL_GLOBALS, REQUIRED_FUNCS,
};

/// Parser work bounds (in addition to the byte limit): vector counts larger
/// than these are rejected before any per-entry work.
pub const MAX_TYPES: u32 = 4096;
pub const MAX_TYPE_ARITY: usize = 64;
pub const MAX_FUNCTIONS: u32 = 100_000;
pub const MAX_EXPORTS: u32 = 64;
pub const MAX_TABLES: u32 = 1;
/// Maximum declared `max` (and `initial`) elements of the single funcref table.
pub const MAX_TABLE_ELEMS: u64 = 65_536;

fn count(s: &mut Reader<'_>, max: u32, what: &str) -> Result<u32, HostError> {
    let n = s.u32()?;
    if n > max {
        return Err(HostError::new("resource_limit", format!("{what} count {n} exceeds profile limit {max}")));
    }
    Ok(n)
}
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleReport {
    pub size: usize,
    pub initial_pages: u64,
    pub max_pages: u64,
    pub table: Option<(u64, u64)>,
    pub functions: u32,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

fn malformed(m: &str) -> HostError {
    HostError::new("malformed_module", m)
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, HostError> {
        let v = *self.b.get(self.p).ok_or_else(|| malformed("unexpected end"))?;
        self.p += 1;
        Ok(v)
    }
    fn leb(&mut self, max_bits: u32) -> Result<u64, HostError> {
        let mut r: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift >= max_bits {
                return Err(malformed("leb128 too long"));
            }
            r |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        if max_bits < 64 && r >> max_bits != 0 {
            return Err(malformed("leb128 overflow"));
        }
        Ok(r)
    }
    fn u32(&mut self) -> Result<u32, HostError> {
        Ok(self.leb(32)? as u32)
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], HostError> {
        let end =
            self.p.checked_add(n).filter(|e| *e <= self.b.len()).ok_or_else(|| malformed("length out of bounds"))?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn name(&mut self) -> Result<&'a str, HostError> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.bytes(n)?).map_err(|_| malformed("invalid utf-8 name"))
    }
    fn done(&self) -> bool {
        self.p >= self.b.len()
    }
}

fn reject(code: &str, m: impl Into<String>) -> HostError {
    HostError::new(code, m)
}

/// Validate a module against the frozen v1 profile.
pub fn validate_module(bytes: &[u8]) -> Result<ModuleReport, HostError> {
    if bytes.len() > MAX_MODULE_BYTES {
        return Err(reject("module_too_large", "module exceeds 16 MiB"));
    }
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return Err(reject("bad_magic", "not a WebAssembly module"));
    }
    if bytes[4..8] != [1, 0, 0, 0] {
        return Err(reject("unsupported_feature", "only core module version 1 is supported"));
    }
    let mut r = Reader { b: bytes, p: 8 };
    let mut types: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut func_types: Vec<u32> = Vec::new();
    let mut memories: Vec<(u64, Option<u64>)> = Vec::new();
    let mut exports: Vec<(String, u8, u32)> = Vec::new();
    let mut tables: Vec<(u64, u64)> = Vec::new();
    let mut last_rank = 0u16;
    while !r.done() {
        let id = r.byte()?;
        let len = r.u32()? as usize;
        let body = r.bytes(len)?;
        if id != 0 {
            // Canonical order 1..=9, datacount (12), 10, 11; each at most once.
            let rank = if id == 12 { 19 } else { u16::from(id) * 2 };
            if rank <= last_rank {
                return Err(malformed("sections out of order or duplicated"));
            }
            last_rank = rank;
        }
        let mut s = Reader { b: body, p: 0 };
        match id {
            0 => {}
            1 => {
                let n = count(&mut s, MAX_TYPES, "type")?;
                for _ in 0..n {
                    if s.byte()? != 0x60 {
                        return Err(reject("unsupported_feature", "non-function type (GC/rec types)"));
                    }
                    let np = s.u32()? as usize;
                    if np > MAX_TYPE_ARITY {
                        return Err(HostError::new("resource_limit", "function type arity exceeds profile"));
                    }
                    let p = s.bytes(np)?.to_vec();
                    let nr = s.u32()? as usize;
                    if nr > MAX_TYPE_ARITY {
                        return Err(HostError::new("resource_limit", "function type arity exceeds profile"));
                    }
                    let q = s.bytes(nr)?.to_vec();
                    types.push((p, q));
                }
            }
            2 => {
                if s.u32()? > 0 {
                    return Err(reject("forbidden_import", "plugin modules must not import anything"));
                }
            }
            3 => {
                let n = count(&mut s, MAX_FUNCTIONS, "function")?;
                for _ in 0..n {
                    func_types.push(s.u32()?);
                }
            }
            4 => {
                let n = count(&mut s, MAX_TABLES, "table")?;
                for _ in 0..n {
                    if s.byte()? != 0x70 {
                        return Err(reject("bad_table", "only funcref tables are allowed"));
                    }
                    if s.byte()? != 0x01 {
                        return Err(reject("bad_table", "table must declare a maximum (no growable/shared/64-bit tables)"));
                    }
                    let min = s.leb(32)?;
                    let max = s.leb(32)?;
                    if max < min || max > MAX_TABLE_ELEMS {
                        return Err(reject("bad_table", "table limits exceed host profile"));
                    }
                    tables.push((min, max));
                }
            }
            6 | 9 | 10 | 11 | 12 => {}
            5 => {
                let n = count(&mut s, 1, "memory")?;
                for _ in 0..n {
                    let flags = s.byte()?;
                    if flags & 0x02 != 0 {
                        return Err(reject("bad_memory", "shared memory is not allowed"));
                    }
                    if flags & 0x04 != 0 {
                        return Err(reject("bad_memory", "memory64 is not allowed"));
                    }
                    if flags & !0x01 != 0 {
                        return Err(reject("bad_memory", "unsupported memory flags"));
                    }
                    let min = s.leb(32)?;
                    let max = if flags & 1 != 0 { Some(s.leb(32)?) } else { None };
                    memories.push((min, max));
                }
            }
            7 => {
                let n = count(&mut s, MAX_EXPORTS, "export")?;
                for _ in 0..n {
                    let name = s.name()?.to_string();
                    let kind = s.byte()?;
                    let idx = s.u32()?;
                    exports.push((name, kind, idx));
                }
            }
            8 => return Err(reject("forbidden_start", "start functions are not allowed")),
            _ => return Err(reject("unsupported_feature", format!("section id {id} not in profile"))),
        }
        if matches!(id, 1 | 2 | 3 | 4 | 5 | 7) && !s.done() {
            return Err(malformed("trailing bytes in section"));
        }
    }
    if memories.len() != 1 {
        return Err(reject("bad_memory", "exactly one defined memory is required"));
    }
    let (min, max) = memories[0];
    // An explicit maximum is REQUIRED: the browser then enforces it on every
    // `memory.grow`, so an in-call growth loop cannot exceed the profile.
    let Some(max) = max else {
        return Err(reject("bad_memory", "memory must declare a maximum <= 8192 pages (link with --max-memory)"));
    };
    if min > MAX_INITIAL_PAGES || max > MAX_DECLARED_PAGES || max < min {
        return Err(reject("bad_memory", "memory limits exceed host profile"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (name, kind, idx) in &exports {
        if !seen.insert(name.as_str()) {
            return Err(malformed("duplicate export"));
        }
        match (name.as_str(), kind) {
            ("memory", 2) if *idx == 0 => {}
            (n, 3) if OPTIONAL_GLOBALS.contains(&n) => {}
            (n, 0) if REQUIRED_FUNCS.iter().any(|f| f.0 == n) => {
                let (_, np, nr) = REQUIRED_FUNCS.iter().find(|f| f.0 == n).copied().unwrap_or_default();
                let ti = func_types
                    .get(*idx as usize)
                    .ok_or_else(|| reject("bad_signature", format!("{n}: not a defined function")))?;
                let (p, q) = types.get(*ti as usize).ok_or_else(|| malformed("type index out of range"))?;
                if p.len() != np || q.len() != nr || p.iter().chain(q.iter()).any(|t| *t != 0x7f) {
                    return Err(reject("bad_signature", format!("{n}: signature differs from contract")));
                }
            }
            _ => return Err(reject("unexpected_export", format!("export `{name}` is not in the contract"))),
        }
    }
    if !seen.contains("memory") {
        return Err(reject("missing_export", "memory"));
    }
    for (n, _, _) in REQUIRED_FUNCS {
        if !seen.contains(n) {
            return Err(reject("missing_export", *n));
        }
    }
    Ok(ModuleReport {
        size: bytes.len(),
        initial_pages: min,
        max_pages: max,
        table: tables.first().copied(),
        functions: func_types.len() as u32,
    })
}
