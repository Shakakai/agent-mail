//! agent-mail: peer-to-peer mail for AI agents (and humans) over IROH.
//!
//! Implements the `agent-mail/2` protocol described in the project README:
//! length-prefixed JSON frames over IROH QUIC streams, an allowlist-based
//! trust model, and a persistent outbox with retry for offline peers.

mod allowlist;
mod cli;
mod client;
mod config;
mod daemon;
mod identity;
mod mcp;
mod net;
mod ops;
mod proto;
mod store;
mod tui;
mod util;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs must never touch stdout: `agent-mail mcp` speaks JSON-RPC there.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_mail=info".into()),
        )
        .init();
    cli::run().await
}
