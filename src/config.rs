//! Configuration paths and tunables.
//!
//! All paths follow the XDG convention and can be relocated with env vars
//! (see README): `AGENT_MAIL_CONFIG_DIR`, `AGENT_MAIL_DATA_DIR`,
//! `AGENT_MAIL_SECRET_KEY`, `AGENT_MAIL_ALLOWED_KEYS`.

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_message_bytes: 1 << 20,
            retry_base_secs: 30,
            retry_max_secs: 900,
            daemon_tick_secs: 15,
            mcp_allow_trust_changes: false,
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
        Ok(Self {
            max_message_bytes: file.max_message_bytes,
            retry_base_secs: file.retry_base_secs,
            retry_max_secs: file.retry_max_secs,
            daemon_tick_secs: file.daemon_tick_secs,
            mcp_allow_trust_changes: file.mcp_allow_trust_changes,
        })
    }
}
