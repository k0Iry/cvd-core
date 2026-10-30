//! Operator configuration (TOML). Loaded from the path in `CVD_CONFIG`; when
//! unset the host runs with built-in defaults.
//!
//! ```toml
//! [server]
//! http_addr = "0.0.0.0:80"
//! static_dir = "static"
//! ```
//!
//! Indicators are no longer executed by the server: visitors import `.wasm`
//! plugins locally in their browser (docs/plugin-wasm-v1.md). A legacy
//! `[indicators]` section (runner path, libraries, timeouts) from an older
//! deployment is still ACCEPTED so an existing container keeps ingesting after
//! an image update, but it is never loaded: startup prints an explicit warning
//! for the section and for every ignored library. Any other unknown key is
//! still rejected (startup fails). Market sources are compiled in
//! (`types::INSTRUMENTS`) and cannot be selected by this file, so a legacy
//! config can never make the server start with different sources.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerSection,
    /// Retired server-side indicator configuration; kept only to report it.
    #[serde(default, rename = "indicators")]
    legacy_indicators: Option<toml::Table>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    #[serde(default = "default_addr")]
    pub http_addr: String,
    #[serde(default = "default_static")]
    pub static_dir: PathBuf,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self { http_addr: default_addr(), static_dir: default_static() }
    }
}

fn default_addr() -> String {
    "0.0.0.0:80".into()
}
fn default_static() -> PathBuf {
    "static".into()
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// `CVD_CONFIG` → file; unset → defaults.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var_os("CVD_CONFIG") {
            Some(p) => Self::load(Path::new(&p)),
            None => Ok(Self::default()),
        }
    }

    /// Explicit warnings for retired settings that are present but ignored.
    pub fn legacy_warnings(&self) -> Vec<String> {
        let Some(t) = &self.legacy_indicators else { return Vec::new() };
        let mut out = vec!["config [indicators] is retired and IGNORED: the server no longer runs indicator plugins; \
             users import .wasm plugins in their browser (docs/deployment.md). Remove the section and any /plugins \
             mount when convenient; base data, PostgreSQL and outbox are unaffected."
            .to_string()];
        if let Some(libs) = t.get("library").and_then(|v| v.as_array()) {
            for l in libs {
                let id = l.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let path = l.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                out.push(format!("ignored retired indicator library id={id} path={path} (not loaded)"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.server.http_addr, "0.0.0.0:80");
        assert!(c.legacy_warnings().is_empty());
    }

    #[test]
    fn legacy_indicator_config_is_accepted_ignored_and_reported() {
        let c = Config::parse(
            r#"
            [server]
            http_addr = "0.0.0.0:8080"
            [indicators]
            runner = "/usr/local/bin/cvd-indicator-runner"
            history_timeout_ms = 10000
            [[indicators.library]]
            id = "sample.channel"
            path = "/plugins/lib.so"
            params = { length = 7 }
            "#,
        )
        .unwrap();
        assert_eq!(c.server.http_addr, "0.0.0.0:8080");
        let w = c.legacy_warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("IGNORED"));
        assert!(w[1].contains("sample.channel") && w[1].contains("/plugins/lib.so"));
    }

    #[test]
    fn shipped_legacy_public_config_still_parses() {
        // The pre-retirement artifacts/cvd.toml template (config/cvd.public.toml at base).
        let legacy = "[server]\nhttp_addr='0.0.0.0:8080'\nstatic_dir='/app/static'\n[indicators]\nrunner='/usr/local/bin/cvd-indicator-runner'\nhistory_timeout_ms=10000\nlive_timeout_ms=2000\nqueue_depth=64\n[[indicators.library]]\nid='cvd'\npath='/plugins/libcvd_indicator_cvd.so'\nenabled=true\nmodes=['source','aggregate']\n";
        let c = Config::parse(legacy).unwrap();
        assert_eq!(c.server.static_dir, PathBuf::from("/app/static"));
        assert_eq!(c.legacy_warnings().len(), 2);
    }

    #[test]
    fn unknown_keys_still_rejected() {
        for bad in ["[server]\nunknown=1", "[sources]\nx=1", "http_addr='x'"] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }
}
