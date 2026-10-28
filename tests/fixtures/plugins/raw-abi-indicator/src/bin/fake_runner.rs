//! Test-only hostile "runner": answers the Hello handshake with a crafted
//! Error frame selected by `--library <mode>`. Used to prove the host bounds,
//! allowlists and sanitises handshake errors regardless of runner content.
use cvd_indicator_sdk::protocol::{FrameHeader, FrameKind, HEADER_LEN};
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.iter().position(|a| a == "--library").and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
    let mut stdin = std::io::stdin().lock();
    let mut hb = [0u8; HEADER_LEN];
    if stdin.read_exact(&mut hb).is_err() {
        return;
    }
    let h = FrameHeader::decode(&hb, 1 << 20).expect("hello header");
    let mut skip = vec![0u8; h.payload_len()];
    let _ = stdin.read_exact(&mut skip);
    let body: Vec<u8> = match mode.as_str() {
        "secret" => br#"{"code":"incompatible","message":"/opt/secret/hunter2 INTERNAL"}"#.to_vec(),
        "unknown_code" => br#"{"code":"hunter2_secret_code","message":"pretend"}"#.to_vec(),
        "malformed" => b"not json /opt/secret hunter2".to_vec(),
        "huge" => {
            let pad = "secret".repeat(20_000);
            format!(r#"{{"code":"incompatible","message":"{pad}"}}"#).into_bytes()
        }
        "unicode" => {
            format!(r#"{{"code":"incompatible","message":"{}"}}"#, "\u{1F512}hunter2".repeat(200)).into_bytes()
        }
        _ => br#"{"code":"incompatible","message":"plain"}"#.to_vec(),
    };
    let mut r = FrameHeader::new(FrameKind::Error, h.request_id, h.session, h.generation);
    r.body_len = body.len() as u32;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&r.encode());
    let _ = out.write_all(&body);
    let _ = out.flush();
}
