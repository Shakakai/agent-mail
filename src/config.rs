//! Configuration paths and tunables.
//!
//! The simplest deployment is a single self-contained folder ("home"):
//! `agent-mail init --home ./alice` creates one, and every command accepts
//! `--home` (or the `AGENT_MAIL_HOME` env var) to use it. A home holds the
//! secret key, allowlist, config.toml, and the mail store together, which
//! is what lets one box run any number of independent nodes.
//!
//! Finer-grained overrides remain available: `AGENT_MAIL_CONFIG_DIR`,
//! `AGENT_MAIL_DATA_DIR`, `AGENT_MAIL_SECRET_KEY`, `AGENT_MAIL_ALLOWED_KEYS`.
//! Precedence: `--home` > `AGENT_MAIL_HOME` > separate config/data dirs > XDG.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub secret_key: PathBuf,
    pub allowed_keys: PathBuf,
    pub db: PathBuf,
}

impl Paths {
    /// Resolve paths for a run. `home` (from the global `--home` flag)
    /// unifies config and data into one folder; otherwise the env vars /
    /// XDG defaults apply.
    pub fn resolve(home: Option<PathBuf>) -> Result<Self> {
        if let Some(home) = home.or_else(|| std::env::var_os("AGENT_MAIL_HOME").map(PathBuf::from)) {
            let secret_key = std::env::var_os("AGENT_MAIL_SECRET_KEY")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("secret-key"));
            let allowed_keys = std::env::var_os("AGENT_MAIL_ALLOWED_KEYS")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("allowed-keys.toml"));
            let db = home.join("mail.db");
            return Ok(Self {
                config_dir: home.clone(),
                data_dir: home.clone(),
                secret_key,
                allowed_keys,
                db,
            });
        }
        Self::from_env()
    }

    pub fn from_env() -> Result<Self> {
        let config_dir = std::env::var_os("AGENT_MAIL_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|d| d.join("agent-mail")))
            .context("could not determine a config directory")?;
        let data_dir = std::env::var_os("AGENT_MAIL_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("agent-mail")))
            .context("could not determine a data directory")?;
        let secret_key = std::env::var_os("AGENT_MAIL_SECRET_KEY")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.join("secret-key"));
        let allowed_keys = std::env::var_os("AGENT_MAIL_ALLOWED_KEYS")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.join("allowed-keys.toml"));
        let db = data_dir.join("mail.db");
        Ok(Self {
            config_dir,
            data_dir,
            secret_key,
            allowed_keys,
            db,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum accepted wire frame size in bytes (default 1 MiB).
    pub max_message_bytes: usize,
    /// First outbox retry delay (default 30s), doubling per attempt.
    pub retry_base_secs: u64,
    /// Outbox retry delay cap (default 900s).
    pub retry_max_secs: u64,
    /// How often the daemon scans the outbox (default 15s).
    pub daemon_tick_secs: u64,
    /// Whether the MCP server may modify the trust list (default false).
    pub mcp_allow_trust_changes: bool,
    /// Which relay infrastructure to use (default: n0's free relays).
    pub relay: RelayCfg,
    /// Publish/resolve addressing via n0's pkarr/DNS discovery
    /// (default true; disable for ticket-only/LAN-only nodes).
    pub discovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayCfg {
    /// n0's production relays (the IROH default).
    N0,
    /// No relays: direct paths and explicit tickets only.
    Disabled,
    /// Your own relay server(s).
    Custom(Vec<String>),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_message_bytes: 1 << 20,
            retry_base_secs: 30,
            retry_max_secs: 900,
            daemon_tick_secs: 15,
            mcp_allow_trust_changes: false,
            relay: RelayCfg::N0,
            discovery: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ConfigFile {
    max_message_bytes: usize,
    retry_base_secs: u64,
    retry_max_secs: u64,
    daemon_tick_secs: u64,
    mcp_allow_trust_changes: bool,
    relay: String,
    relay_urls: Vec<String>,
    discovery: bool,
}

impl Default for ConfigFile {
    fn default() -> Self {
        let c = Config::default();
        Self {
            max_message_bytes: c.max_message_bytes,
            retry_base_secs: c.retry_base_secs,
            retry_max_secs: c.retry_max_secs,
            daemon_tick_secs: c.daemon_tick_secs,
            mcp_allow_trust_changes: false,
            relay: "n0".to_string(),
            relay_urls: Vec::new(),
            discovery: true,
        }
    }
}

impl Config {
    pub fn load(config_dir: &std::path::Path) -> Result<Self> {
        let path = config_dir.join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: ConfigFile = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        let relay = match file.relay.as_str() {
            "n0" => RelayCfg::N0,
            "disabled" => RelayCfg::Disabled,
            "custom" => {
                if file.relay_urls.is_empty() {
                    anyhow::bail!(
                        "config.toml: relay = \"custom\" requires at least one relay_urls entry"
                    );
                }
                RelayCfg::Custom(file.relay_urls)
            }
            other => anyhow::bail!(
                "config.toml: relay must be \"n0\", \"disabled\", or \"custom\" (got `{other}`)"
            ),
        };
        Ok(Self {
            max_message_bytes: file.max_message_bytes,
            retry_base_secs: file.retry_base_secs,
            retry_max_secs: file.retry_max_secs,
            daemon_tick_secs: file.daemon_tick_secs,
            mcp_allow_trust_changes: file.mcp_allow_trust_changes,
            relay,
            discovery: file.discovery,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("am-cfg-test-{name}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tmp_dir("defaults");
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.max_message_bytes, 1 << 20);
        assert_eq!(c.retry_base_secs, 30);
        assert_eq!(c.daemon_tick_secs, 15);
        assert!(!c.mcp_allow_trust_changes);
        assert_eq!(c.relay, RelayCfg::N0);
        assert!(c.discovery);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn full_parse_custom_relay_no_discovery() {
        let dir = tmp_dir("custom");
        std::fs::write(
            dir.join("config.toml"),
            r#"max_message_bytes = 2048
retry_base_secs = 10
retry_max_secs = 60
daemon_tick_secs = 5
mcp_allow_trust_changes = true
relay = "custom"
relay_urls = ["https://relay.example.com."]
discovery = false
"#,
        )
        .unwrap();
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.max_message_bytes, 2048);
        assert_eq!(c.retry_base_secs, 10);
        assert_eq!(c.retry_max_secs, 60);
        assert_eq!(c.daemon_tick_secs, 5);
        assert!(c.mcp_allow_trust_changes);
        assert_eq!(c.relay, RelayCfg::Custom(vec!["https://relay.example.com.".into()]));
        assert!(!c.discovery);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn relay_disabled_parses() {
        let dir = tmp_dir("disabled");
        std::fs::write(dir.join("config.toml"), "relay = \"disabled\"\n").unwrap();
        let c = Config::load(&dir).unwrap();
        assert_eq!(c.relay, RelayCfg::Disabled);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn invalid_relay_value_errors() {
        let dir = tmp_dir("bad");
        std::fs::write(dir.join("config.toml"), "relay = \"bogus\"\n").unwrap();
        assert!(Config::load(&dir).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn custom_relay_requires_urls() {
        let dir = tmp_dir("nourls");
        std::fs::write(dir.join("config.toml"), "relay = \"custom\"\n").unwrap();
        let err = Config::load(&dir).unwrap_err().to_string();
        assert!(err.contains("relay_urls"), "error should name relay_urls: {err}");
        std::fs::remove_dir_all(dir).ok();
    }
}
