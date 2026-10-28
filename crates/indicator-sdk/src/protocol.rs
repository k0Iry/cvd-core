//! Strict length-prefixed frame protocol spoken on the runner's stdin/stdout.
//!
//! Header (40 bytes, little endian):
//! ```text
//! magic "CVDP" | u16 protocol version | u16 kind | u64 request_id
//! | u64 session | u64 generation | u32 params_len | u32 body_len
//! ```
//! followed by `params_len` bytes of JSON parameters and `body_len` bytes of
//! JSON body. The runner forwards both byte ranges to the library unchanged
//! (no re-serialisation); responses carry the library's output bytes verbatim.

use std::io::{self, Read, Write};

pub const FRAME_MAGIC: [u8; 4] = *b"CVDP";
pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum FrameKind {
    /// host → runner: handshake. Body: `{"protocol":1,"abi":1}`.
    Hello = 1,
    /// runner → host: handshake reply. Body: library descriptor JSON.
    Ready = 2,
    /// host → runner: stateless historical window (params + `HistoryRequest`).
    History = 10,
    /// host → runner: create persistent live session (params + `LiveInit`).
    LiveOpen = 11,
    /// host → runner: delta on a session (`LiveUpdate`). Params empty.
    LiveUpdate = 12,
    /// host → runner: destroy a session. Empty payload.
    LiveClose = 13,
    /// runner → host: success; body is the library `Output` JSON.
    Result = 20,
    /// runner → host: failure; body is a public `PluginError` JSON.
    Error = 21,
    /// host → runner: orderly exit.
    Shutdown = 30,
}

impl FrameKind {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Hello,
            2 => Self::Ready,
            10 => Self::History,
            11 => Self::LiveOpen,
            12 => Self::LiveUpdate,
            13 => Self::LiveClose,
            20 => Self::Result,
            21 => Self::Error,
            30 => Self::Shutdown,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: FrameKind,
    pub request_id: u64,
    pub session: u64,
    pub generation: u64,
    pub params_len: u32,
    pub body_len: u32,
}

impl FrameHeader {
    pub fn new(kind: FrameKind, request_id: u64, session: u64, generation: u64) -> Self {
        Self { kind, request_id, session, generation, params_len: 0, body_len: 0 }
    }

    pub fn payload_len(&self) -> usize {
        self.params_len as usize + self.body_len as usize
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&FRAME_MAGIC);
        b[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        b[6..8].copy_from_slice(&(self.kind as u16).to_le_bytes());
        b[8..16].copy_from_slice(&self.request_id.to_le_bytes());
        b[16..24].copy_from_slice(&self.session.to_le_bytes());
        b[24..32].copy_from_slice(&self.generation.to_le_bytes());
        b[32..36].copy_from_slice(&self.params_len.to_le_bytes());
        b[36..40].copy_from_slice(&self.body_len.to_le_bytes());
        b
    }

    /// Decode and validate magic, version, kind and size bound.
    pub fn decode(b: &[u8; HEADER_LEN], max_payload: usize) -> Result<Self, ProtocolError> {
        if b[0..4] != FRAME_MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let version = u16::from_le_bytes([b[4], b[5]]);
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::Version(version));
        }
        let kind_raw = u16::from_le_bytes([b[6], b[7]]);
        let kind = FrameKind::from_u16(kind_raw).ok_or(ProtocolError::Kind(kind_raw))?;
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let h = Self {
            kind,
            request_id: u64_at(8),
            session: u64_at(16),
            generation: u64_at(24),
            params_len: u32_at(32),
            body_len: u32_at(36),
        };
        if h.payload_len() > max_payload {
            return Err(ProtocolError::TooLarge(h.payload_len()));
        }
        Ok(h)
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    BadMagic,
    Version(u16),
    Kind(u16),
    TooLarge(usize),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::BadMagic => write!(f, "bad frame magic"),
            Self::Version(v) => write!(f, "unsupported protocol version {v}"),
            Self::Kind(k) => write!(f, "unknown frame kind {k}"),
            Self::TooLarge(n) => write!(f, "frame payload {n} exceeds limit"),
        }
    }
}
impl std::error::Error for ProtocolError {}
impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A decoded frame: `payload[..params_len]` = params, rest = body.
#[derive(Debug, Clone)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn params(&self) -> &[u8] {
        &self.payload[..self.header.params_len as usize]
    }
    pub fn body(&self) -> &[u8] {
        &self.payload[self.header.params_len as usize..]
    }
}

/// Blocking read. `Ok(None)` on clean EOF before a header.
pub fn read_frame<R: Read>(r: &mut R, max_payload: usize) -> Result<Option<Frame>, ProtocolError> {
    let mut hb = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        let n = r.read(&mut hb[got..])?;
        if n == 0 {
            return if got == 0 { Ok(None) } else { Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()) };
        }
        got += n;
    }
    let header = FrameHeader::decode(&hb, max_payload)?;
    let mut payload = vec![0u8; header.payload_len()];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame { header, payload }))
}

/// Blocking write of one frame (params and body slices are written verbatim).
pub fn write_frame<W: Write>(w: &mut W, mut header: FrameHeader, params: &[u8], body: &[u8]) -> io::Result<()> {
    header.params_len = u32::try_from(params.len()).map_err(|_| io::Error::other("params too large"))?;
    header.body_len = u32::try_from(body.len()).map_err(|_| io::Error::other("body too large"))?;
    w.write_all(&header.encode())?;
    w.write_all(params)?;
    w.write_all(body)?;
    w.flush()
}

/// Resource limits shared by host and runner.
///
/// Sizing rationale (measured with the canonical bar JSON, ~560 bytes/bar):
/// a 1 600-bar history page plus the 8 192-bar maximum declared warm-up is
/// ~5.5 MB of request; a 32-layer segmented output over the same 9 792 bars is
/// ~6–8 MB. Defaults keep ≥2× headroom over that worst case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_context_bars: u32,
    pub max_bars_per_request: usize,
    pub max_sessions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_bytes: 16 << 20,
            max_response_bytes: 32 << 20,
            max_context_bars: 8192,
            max_bars_per_request: 16_384,
            max_sessions: 512,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip_and_rejections() {
        let mut h = FrameHeader::new(FrameKind::LiveUpdate, 7, 9, 3);
        h.params_len = 2;
        h.body_len = 5;
        let b = h.encode();
        assert_eq!(FrameHeader::decode(&b, 100).unwrap(), h);
        assert!(matches!(FrameHeader::decode(&b, 6), Err(ProtocolError::TooLarge(7))));
        let mut bad = b;
        bad[0] = b'X';
        assert!(matches!(FrameHeader::decode(&bad, 100), Err(ProtocolError::BadMagic)));
        let mut bad = b;
        bad[4] = 2;
        assert!(matches!(FrameHeader::decode(&bad, 100), Err(ProtocolError::Version(2))));
        let mut bad = b;
        bad[6] = 99;
        assert!(matches!(FrameHeader::decode(&bad, 100), Err(ProtocolError::Kind(99))));
    }

    #[test]
    fn frame_io_round_trip_and_truncation() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameHeader::new(FrameKind::History, 1, 0, 1), b"{}", b"{\"a\":1}").unwrap();
        let f = read_frame(&mut buf.as_slice(), 1024).unwrap().unwrap();
        assert_eq!(f.params(), b"{}");
        assert_eq!(f.body(), b"{\"a\":1}");
        assert!(read_frame(&mut &buf[..buf.len() - 1], 1024).is_err());
        assert!(read_frame(&mut &buf[..10], 1024).is_err());
        assert!(read_frame(&mut &[][..], 1024).unwrap().is_none());
    }
}
