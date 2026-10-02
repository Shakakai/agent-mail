//! agent-mail: peer-to-peer mail for AI agents (and humans) over IROH.
//!
//! Implements the `agent-mail/1` protocol described in the project README:
//! length-prefixed JSON frames over IROH QUIC streams, an allowlist-based
//! trust model, and a persistent outbox with retry for offline peers.

mod allowlist;
mod cli;
mod client;
mod config;
mod daemon;
mod identity;
mod mcp;
mod ops;
mod proto;
mod store;
mod tui;
mod util;

use config::Paths;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_mail=info".into()),
        )
        .init();
    let paths = Paths::from_env()?;
    cli::run(paths).await
}
