//! Startup settings. No configuration file: built-in defaults match the
//! shipped container, with two optional environment overrides.
//!
//! - `CVD_HTTP_ADDR` (default `0.0.0.0:8080`)
//! - `CVD_STATIC_DIR` (default `static`; resolves to `/app/static` in the image)
//!
//! The former TOML file (`CVD_CONFIG`) is retired and no longer read.

use std::net::SocketAddr;
use std::path::PathBuf;

pub const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:8080";
pub const DEFAULT_STATIC_DIR: &str = "static";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub http_addr: SocketAddr,
    pub static_dir: PathBuf,
}

impl Settings {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let addr = get("CVD_HTTP_ADDR").unwrap_or_else(|| DEFAULT_HTTP_ADDR.into());
        let http_addr = addr.parse().map_err(|e| anyhow::anyhow!("invalid CVD_HTTP_ADDR {addr:?}: {e}"))?;
        let static_dir = get("CVD_STATIC_DIR").unwrap_or_else(|| DEFAULT_STATIC_DIR.into()).into();
        Ok(Self { http_addr, static_dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn with(pairs: &[(&str, &str)]) -> anyhow::Result<Settings> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Settings::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults_match_shipped_container() {
        let s = with(&[]).unwrap();
        assert_eq!(s.http_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(s.static_dir, PathBuf::from("static"));
    }

    #[test]
    fn env_overrides() {
        let s = with(&[("CVD_HTTP_ADDR", "127.0.0.1:9000"), ("CVD_STATIC_DIR", "/app/static")]).unwrap();
        assert_eq!(s.http_addr, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(s.static_dir, PathBuf::from("/app/static"));
    }

    #[test]
    fn invalid_address_fails() {
        let e = with(&[("CVD_HTTP_ADDR", "not-an-addr")]).unwrap_err().to_string();
        assert!(e.contains("CVD_HTTP_ADDR"), "{e}");
    }

    #[test]
    fn retired_config_file_is_not_read() {
        // A legacy CVD_CONFIG pointing at a missing file has no effect.
        let s = with(&[("CVD_CONFIG", "/nonexistent/cvd.toml")]).unwrap();
        assert_eq!(s, with(&[]).unwrap());
    }
}
