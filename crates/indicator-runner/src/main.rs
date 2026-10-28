//! `cvd-indicator-runner --library <path>`
//!
//! Loads exactly one indicator dynamic library and serves the framed protocol
//! (see `cvd_indicator_sdk::protocol`) on stdin/stdout. The process boundary
//! contains crashes, aborts and hangs of library code; it is *not* a security
//! sandbox (the library runs with this process's OS privileges).
//!
//! Library-originated writes to fd 1 are redirected to stderr so that only the
//! runner can emit protocol frames.

use cvd_indicator_sdk::abi::*;
use cvd_indicator_sdk::model::{codes, diagnostic_text, Descriptor, PluginError};
use cvd_indicator_sdk::protocol::{read_frame, write_frame, Frame, FrameHeader, FrameKind, Limits};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::os::fd::FromRawFd;
use std::process::ExitCode;

/// Hard cap on a library-produced error document, checked before the buffer
/// is sliced, copied or decoded (independent of the response frame limit).
const MAX_ABI_ERROR_BYTES: usize = 4096;
/// Bytes of library error text retained for operator stderr diagnostics.
const MAX_DIAGNOSTIC_BYTES: usize = 256;

struct Library {
    _dl: *mut c_void,
    api: &'static CvdPluginApiV1,
}

fn dl_error() -> String {
    // SAFETY: dlerror returns a thread-local C string or null.
    unsafe {
        let p = libc::dlerror();
        if p.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// Returns (library, descriptor JSON) or a public error (raw details → stderr).
fn load(path: &str, max_descriptor: usize) -> Result<(Library, Vec<u8>), PluginError> {
    let incompatible = |public: &str, local: String| {
        eprintln!("cvd-indicator-runner: {public}: {}", diagnostic_text(local.as_bytes(), 512));
        PluginError::new(codes::INCOMPATIBLE, public)
    };
    let c = CString::new(path).map_err(|_| incompatible("library load failed", "path contains NUL".into()))?;
    // SAFETY: loading an operator-configured library; its constructors run here.
    let dl = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if dl.is_null() {
        return Err(incompatible("library load failed", dl_error()));
    }
    let sym = unsafe { libc::dlsym(dl, ENTRY_SYMBOL.as_ptr().cast()) };
    if sym.is_null() {
        return Err(incompatible("entry symbol cvd_plugin_entry_v1 missing", dl_error()));
    }
    // SAFETY: symbol is documented to have EntryFnV1 signature.
    let entry: EntryFnV1 = unsafe { std::mem::transmute::<*mut c_void, EntryFnV1>(sym) };
    let api = unsafe { entry(ABI_VERSION) };
    if api.is_null() {
        return Err(incompatible("library rejected host ABI version", format!("host abi {ABI_VERSION}")));
    }
    // SAFETY: non-null pointer to a static table; header fields read first.
    let (abi_version, size) = unsafe { ((*api).abi_version, (*api).struct_size) };
    if abi_version != ABI_VERSION || size < API_V1_SIZE {
        return Err(incompatible("ABI version or table size mismatch", format!("abi {abi_version} size {size}")));
    }
    let api: &'static CvdPluginApiV1 = unsafe { &*api };
    let mut buf = CvdBuffer::empty();
    let status = unsafe { (api.describe)(&mut buf) };
    // Bound the library-owned buffer before slicing; always free it.
    let len = buf.len;
    let bytes = if len <= max_descriptor { unsafe { buf.as_slice().to_vec() } } else { Vec::new() };
    unsafe { (api.free_buffer)(buf) };
    if len > max_descriptor {
        return Err(incompatible("descriptor too large", format!("{len} bytes")));
    }
    if status != STATUS_OK {
        return Err(incompatible("describe failed", format!("status {status}")));
    }
    let desc: Descriptor =
        serde_json::from_slice(&bytes).map_err(|e| incompatible("descriptor is not valid JSON", e.to_string()))?;
    desc.validate().map_err(|e| incompatible("descriptor rejected", e.message.clone()))?;
    Ok((Library { _dl: dl, api }, bytes))
}

struct Session {
    handle: *mut c_void,
    generation: u64,
}

struct Runner {
    lib: Library,
    descriptor: Vec<u8>,
    sessions: HashMap<u64, Session>,
    limits: Limits,
}

enum Reply {
    Ok(Vec<u8>),
    Err(PluginError),
}

impl Runner {
    fn create(&self, params: &[u8]) -> Result<*mut c_void, PluginError> {
        let mut handle = std::ptr::null_mut();
        let mut err = CvdBuffer::empty();
        let status = unsafe { (self.lib.api.create)(params.as_ptr(), params.len(), &mut handle, &mut err) };
        let e = if status == STATUS_OK { None } else { Some(self.status_error("create", status, &err)) };
        unsafe { (self.lib.api.free_buffer)(err) };
        match e {
            None if !handle.is_null() => Ok(handle),
            None => Err(PluginError::public_for(codes::PROTOCOL)),
            Some(e) => {
                if !handle.is_null() {
                    unsafe { (self.lib.api.destroy)(handle) };
                }
                Err(e)
            }
        }
    }

    /// Maps a non-OK status and its (still library-owned, unfreed) buffer to a
    /// public error with a fixed message. The buffer length is checked before
    /// any slice/copy/decode; bounded library text goes to stderr only.
    fn status_error(&self, op: &str, status: i32, buf: &CvdBuffer) -> PluginError {
        match status {
            STATUS_PANIC => PluginError::public_for(codes::PANIC),
            STATUS_ERROR => {
                if buf.len > MAX_ABI_ERROR_BYTES {
                    eprintln!("cvd-indicator-runner: {op}: oversized error buffer ({} bytes) rejected", buf.len);
                    return PluginError::public_for(codes::COMPUTE_FAILED);
                }
                let bytes = unsafe { buf.as_slice() };
                match serde_json::from_slice::<PluginError>(bytes) {
                    Ok(e) => {
                        let code =
                            (e.code.len() <= codes::MAX_CODE_LEN).then(|| codes::library_allowed(&e.code)).flatten();
                        eprintln!(
                            "cvd-indicator-runner: {op}: library error code={} message={}",
                            diagnostic_text(e.code.as_bytes(), codes::MAX_CODE_LEN),
                            diagnostic_text(e.message.as_bytes(), MAX_DIAGNOSTIC_BYTES)
                        );
                        PluginError::public_for(code.unwrap_or(codes::COMPUTE_FAILED))
                    }
                    Err(_) => {
                        eprintln!("cvd-indicator-runner: {op}: unreadable error document ({} bytes)", bytes.len());
                        PluginError::public_for(codes::COMPUTE_FAILED)
                    }
                }
            }
            STATUS_INVALID_ARGUMENT => PluginError::public_for(codes::PROTOCOL),
            s => {
                eprintln!("cvd-indicator-runner: {op}: unknown ABI status {s}");
                PluginError::public_for(codes::PROTOCOL)
            }
        }
    }

    /// Invoke; returns (status, output bytes). Output bytes are copied once
    /// into the reply and the library buffer is freed by the library.
    fn invoke(&self, handle: *mut c_void, op: u32, body: &[u8]) -> (i32, Result<Vec<u8>, PluginError>) {
        let mut out = CvdBuffer::empty();
        let status = unsafe { (self.lib.api.invoke)(handle, op, body.as_ptr(), body.len(), &mut out) };
        let len = out.len;
        let r = if status == STATUS_OK {
            if len > self.limits.max_response_bytes {
                Err(PluginError::new(codes::LIMIT_EXCEEDED, "indicator output exceeds response limit"))
            } else {
                Ok(unsafe { out.as_slice().to_vec() })
            }
        } else {
            Err(self.status_error("invoke", status, &out))
        };
        unsafe { (self.lib.api.free_buffer)(out) };
        (status, r)
    }

    fn destroy(&mut self, id: u64) {
        if let Some(s) = self.sessions.remove(&id) {
            unsafe { (self.lib.api.destroy)(s.handle) };
        }
    }

    fn handle(&mut self, f: &Frame) -> Reply {
        let h = f.header;
        match h.kind {
            FrameKind::History => {
                let handle = match self.create(f.params()) {
                    Ok(x) => x,
                    Err(e) => return Reply::Err(e),
                };
                let (_, r) = self.invoke(handle, OP_HISTORY, f.body());
                unsafe { (self.lib.api.destroy)(handle) };
                match r {
                    Ok(v) => Reply::Ok(v),
                    Err(e) => Reply::Err(e),
                }
            }
            FrameKind::LiveOpen => {
                self.destroy(h.session);
                if self.sessions.len() >= self.limits.max_sessions {
                    return Reply::Err(PluginError::new(codes::LIMIT_EXCEEDED, "too many live sessions"));
                }
                let handle = match self.create(f.params()) {
                    Ok(x) => x,
                    Err(e) => return Reply::Err(e),
                };
                match self.invoke(handle, OP_LIVE_INIT, f.body()) {
                    (_, Ok(v)) => {
                        self.sessions.insert(h.session, Session { handle, generation: h.generation });
                        Reply::Ok(v)
                    }
                    (_, Err(e)) => {
                        unsafe { (self.lib.api.destroy)(handle) };
                        Reply::Err(e)
                    }
                }
            }
            FrameKind::LiveUpdate => {
                let Some(s) = self.sessions.get(&h.session) else {
                    return Reply::Err(PluginError::new(codes::UNKNOWN_SESSION, "live session not open"));
                };
                if s.generation != h.generation {
                    return Reply::Err(PluginError::new(codes::STALE_GENERATION, "session generation mismatch"));
                }
                let handle = s.handle;
                match self.invoke(handle, OP_LIVE_UPDATE, f.body()) {
                    (_, Ok(v)) => Reply::Ok(v),
                    (status, Err(e)) => {
                        if status == STATUS_PANIC {
                            self.destroy(h.session);
                        }
                        Reply::Err(e)
                    }
                }
            }
            FrameKind::LiveClose => {
                self.destroy(h.session);
                Reply::Ok(Vec::new())
            }
            FrameKind::Hello => Reply::Ok(self.descriptor.clone()),
            _ => Reply::Err(PluginError::new(codes::PROTOCOL, "unexpected frame kind")),
        }
    }
}

fn reply<W: Write>(w: &mut W, req: FrameHeader, r: Reply) -> std::io::Result<()> {
    let (kind, body) = match r {
        Reply::Ok(v) => (if req.kind == FrameKind::Hello { FrameKind::Ready } else { FrameKind::Result }, v),
        Reply::Err(e) => (FrameKind::Error, serde_json::to_vec(&e).unwrap_or_default()),
    };
    write_frame(w, FrameHeader::new(kind, req.request_id, req.session, req.generation), &[], &body)
}

fn parse_args() -> Result<(String, Limits), String> {
    let mut lib = None;
    let mut limits = Limits::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--library" => lib = Some(val()?),
            "--max-request-bytes" => limits.max_request_bytes = val()?.parse().map_err(|_| "bad number")?,
            "--max-response-bytes" => limits.max_response_bytes = val()?.parse().map_err(|_| "bad number")?,
            "--max-sessions" => limits.max_sessions = val()?.parse().map_err(|_| "bad number")?,
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    Ok((lib.ok_or("--library is required")?, limits))
}

fn main() -> ExitCode {
    let (path, limits) = match parse_args() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("cvd-indicator-runner: {e}");
            return ExitCode::from(2);
        }
    };
    // Die with the host: never outlive the supervising process (Linux).
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }
    // Reserve the original stdout for protocol frames; point fd 1 at stderr so
    // stray library prints cannot corrupt the protocol stream.
    // F_DUPFD_CLOEXEC: descendants the library may exec never inherit the
    // protocol write end.
    let proto_fd = unsafe { libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3) };
    if proto_fd < 0 || unsafe { libc::dup2(2, 1) } < 0 {
        eprintln!("cvd-indicator-runner: cannot isolate stdout");
        return ExitCode::from(2);
    }
    let mut out = BufWriter::new(unsafe { File::from_raw_fd(proto_fd) });
    let mut input = BufReader::new(std::io::stdin().lock());

    // Handshake first so the host always gets a framed answer, even when the
    // library cannot be loaded.
    let hello = match read_frame(&mut input, limits.max_request_bytes) {
        Ok(Some(f)) if f.header.kind == FrameKind::Hello => f,
        Ok(_) => return ExitCode::from(2),
        Err(e) => {
            eprintln!("cvd-indicator-runner: handshake: {e}");
            return ExitCode::from(2);
        }
    };
    let proto_ok = serde_json::from_slice::<serde_json::Value>(hello.body())
        .ok()
        .and_then(|v| Some(v.get("protocol")?.as_u64()? == 1 && v.get("abi")?.as_u64()? == ABI_VERSION as u64))
        .unwrap_or(false);
    if !proto_ok {
        let _ = reply(
            &mut out,
            hello.header,
            Reply::Err(PluginError::new(codes::INCOMPATIBLE, "unsupported protocol/abi")),
        );
        return ExitCode::from(3);
    }
    let (lib, descriptor) = match load(&path, limits.max_response_bytes) {
        Ok(x) => x,
        Err(e) => {
            let _ = reply(&mut out, hello.header, Reply::Err(e));
            return ExitCode::from(3);
        }
    };
    let mut runner = Runner { lib, descriptor, sessions: HashMap::new(), limits };
    if reply(&mut out, hello.header, Reply::Ok(runner.descriptor.clone())).is_err() {
        return ExitCode::from(1);
    }
    loop {
        let frame = match read_frame(&mut input, limits.max_request_bytes) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                // Framing is unrecoverable: tell the host (best effort) and exit.
                eprintln!("cvd-indicator-runner: protocol: {e}");
                let h = FrameHeader::new(FrameKind::Error, 0, 0, 0);
                let body =
                    serde_json::to_vec(&PluginError::new(codes::PROTOCOL, "malformed frame")).unwrap_or_default();
                let _ = write_frame(&mut out, h, &[], &body);
                return ExitCode::from(4);
            }
        };
        if frame.header.kind == FrameKind::Shutdown {
            break;
        }
        let r = runner.handle(&frame);
        if reply(&mut out, frame.header, r).is_err() {
            return ExitCode::from(1);
        }
    }
    let ids: Vec<u64> = runner.sessions.keys().copied().collect();
    for id in ids {
        runner.destroy(id);
    }
    // The library stays mapped until process exit; no dlclose (no hot unload).
    ExitCode::SUCCESS
}
