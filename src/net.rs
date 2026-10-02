//! Endpoint construction: one place that turns `Config` into a bound IROH
//! endpoint with the chosen relay and discovery behavior. Every process that
//! dials or accepts mail (daemon, interactive sends, MCP server) goes through
//! here so networking policy can't drift between paths.

use anyhow::{Context, Result};
use iroh::{Endpoint, SecretKey, RelayMode, endpoint::presets};

use crate::config::{Config, RelayCfg};

/// Bind an endpoint with the node's persistent key and the configured
/// relay/discovery policy.
pub async fn bind_endpoint(config: &Config, sk: &SecretKey) -> Result<Endpoint> {
    // N0 carries the crypto provider + n0 pkarr/DNS discovery; Minimal is
    // crypto provider only (for discovery = false).
    let mut builder = if config.discovery {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal)
    };
    match &config.relay {
        RelayCfg::N0 => {} // whatever the preset set (n0 production relays)
        RelayCfg::Disabled => {
            builder = builder.relay_mode(RelayMode::Disabled);
        }
        RelayCfg::Custom(urls) => {
            let mut relay_urls = Vec::new();
            for u in urls {
                relay_urls.push(
                    u.parse::<iroh::RelayUrl>()
                        .with_context(|| format!("invalid relay URL `{u}`"))?,
                );
            }
            builder = builder.relay_mode(RelayMode::custom(relay_urls));
        }
    }
    builder
        .secret_key(sk.clone())
        .bind()
        .await
        .map_err(crate::util::de)
}

/// Wait until the endpoint is meaningfully "online". With relays enabled
/// this means a home-relay connection exists (IROH's `online()`). With
/// relays disabled there is no relay to connect to, so we just give direct
/// address discovery a moment to populate.
pub async fn wait_online(config: &Config, endpoint: &Endpoint) {
    match config.relay {
        RelayCfg::Disabled => {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        _ => endpoint.online().await,
    }
}

/// Human-readable summary of the networking policy, for logs/status.
pub fn describe(config: &Config) -> String {
    let relay = match &config.relay {
        RelayCfg::N0 => "n0 relays".to_string(),
        RelayCfg::Disabled => "relays disabled".to_string(),
        RelayCfg::Custom(urls) => format!("custom relay(s): {}", urls.join(", ")),
    };
    let discovery = if config.discovery {
        "n0 discovery"
    } else {
        "discovery off (tickets/LAN)"
    };
    format!("{relay}; {discovery}")
}
