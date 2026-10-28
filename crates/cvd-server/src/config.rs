//! Operator configuration (TOML). Loaded from the path in `CVD_CONFIG`; when
//! unset the host runs with built-in defaults and **no** indicator libraries.
//!
//! Indicator libraries are discovered only from this file. Clients can select
//! installed indicator ids but can never supply a path or URL.
//!
//! ```toml
//! [server]
//! http_addr = "0.0.0.0:80"
//! static_dir = "static"
//!
//! [indicators]
//! runner = "/usr/local/bin/cvd-indicator-runner"
//! history_timeout_ms = 10000
//! live_timeout_ms = 2000
//! queue_depth = 64
//!
//! [[indicators.library]]
//! id = "sample.channel"            # must equal the library descriptor id
//! path = "/opt/cvd/indicators/libsample_indicator.so"
//! params = { length = 20 }         # operator-fixed parameters (JSON object)
//! modes = ["source", "aggregate"]  # which views to compute
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub indicators: IndicatorSection,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndicatorSection {
    #[serde(default)]
    pub runner: Option<PathBuf>,
    #[serde(default = "default_history_timeout")]
    pub history_timeout_ms: u64,
    #[serde(default = "default_live_timeout")]
    pub live_timeout_ms: u64,
    #[serde(default = "default_queue")]
    pub queue_depth: usize,
    #[serde(default)]
    pub library: Vec<LibraryConfig>,
}

impl Default for IndicatorSection {
    fn default() -> Self {
        Self {
            runner: None,
            history_timeout_ms: default_history_timeout(),
            live_timeout_ms: default_live_timeout(),
            queue_depth: default_queue(),
            library: Vec::new(),
        }
    }
}

fn default_history_timeout() -> u64 {
    10_000
}
fn default_live_timeout() -> u64 {
    2_000
}
fn default_queue() -> usize {
    64
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    pub id: String,
    pub path: PathBuf,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub params: Option<toml::Table>,
    #[serde(default = "default_modes")]
    pub modes: Vec<String>,
}

fn default_enabled() -> bool {
    true
}
fn default_modes() -> Vec<String> {
    vec!["source".into(), "aggregate".into()]
}

impl LibraryConfig {
    /// Canonical JSON for the operator parameters (encoded once, reused).
    pub fn params_json(&self) -> Vec<u8> {
        match &self.params {
            None => b"{}".to_vec(),
            Some(t) => serde_json::to_vec(t).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// `CVD_CONFIG` → file; unset → defaults (no indicators).
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var_os("CVD_CONFIG") {
            Some(p) => Self::load(Path::new(&p)),
            None => Ok(Self::default()),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        for l in &self.indicators.library {
            anyhow::ensure!(ids.insert(l.id.as_str()), "duplicate indicator id {}", l.id);
            anyhow::ensure!(l.path.is_absolute(), "indicator {} path must be absolute", l.id);
            for m in &l.modes {
                anyhow::ensure!(m == "source" || m == "aggregate", "indicator {} has invalid mode {m}", l.id);
            }
        }
        if self.indicators.library.iter().any(|l| l.enabled) {
            let r = self.indicators.runner.as_ref();
            anyhow::ensure!(r.is_some_and(|r| r.is_absolute()), "indicators.runner must be an absolute path");
        }
        anyhow::ensure!(self.indicators.queue_depth > 0, "queue_depth must be > 0");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_no_indicators() {
        let c = Config::parse("").unwrap();
        assert!(c.indicators.library.is_empty());
        assert_eq!(c.server.http_addr, "0.0.0.0:80");
    }

    #[test]
    fn library_config_parses_and_validates() {
        let c = Config::parse(
            r#"
            [indicators]
            runner = "/usr/bin/cvd-indicator-runner"
            [[indicators.library]]
            id = "sample.channel"
            path = "/opt/lib.so"
            params = { length = 7 }
            "#,
        )
        .unwrap();
        assert_eq!(c.indicators.library[0].params_json(), br#"{"length":7}"#);
        assert_eq!(c.indicators.library[0].modes, ["source", "aggregate"]);
        for bad in [
            "[indicators]\nrunner='/r'\n[[indicators.library]]\nid='a'\npath='rel.so'",
            "[[indicators.library]]\nid='a'\npath='/a.so'",
            "[indicators]\nrunner='/r'\n[[indicators.library]]\nid='a'\npath='/a.so'\n[[indicators.library]]\nid='a'\npath='/b.so'",
            "[indicators]\nrunner='/r'\n[[indicators.library]]\nid='a'\npath='/a.so'\nmodes=['x']",
            "[indicators]\nunknown=1",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }
}
