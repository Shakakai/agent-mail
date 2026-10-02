//! CLI surface: `init`, `id`, `allow`, `daemon`, `send`, `inbox`, `read`,
//! `reply`. (M4 adds `mcp`; M5 adds `tui`.)

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use iroh::{Endpoint, endpoint::presets};

use crate::allowlist::{AllowList, PeerEntry};
use crate::client;
use crate::config::{Config, Paths};
use crate::store::Store;
use crate::{daemon, identity, mcp, tui};

#[derive(Parser)]
#[command(
    name = "agent-mail",
    version,
    about = "Peer-to-peer mail for AI agents (and humans) over IROH"
)]
struct Cli {
    /// One self-contained folder holding the secret key, allowlist,
    /// config, and mail store — everything this node needs. Use it to run
    /// multiple independent nodes on one machine. (Env: AGENT_MAIL_HOME)
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate identity, config, and an empty allowlist.
    Init,
    /// Print this agent's NodeId (share it with peers so they can allow you).
    Id,
    /// Manage the trust list (who may exchange mail with this agent).
    Allow {
        #[command(subcommand)]
        action: AllowAction,
    },
    /// Run the mail daemon (receives mail, retries the outbox).
    Daemon,
    /// Serve MCP over stdio (the agent interface). The mail daemon runs
    /// in-process for the lifetime of the MCP server and shuts down when
    /// the stdio session ends.
    Mcp,
    /// Human mail client (TUI).
    Tui,
    /// Send a message to an allowlisted peer.
    Send {
        /// Peer NodeId or allowlist name.
        peer: String,
        /// Message body.
        #[arg(short, long)]
        message: Option<String>,
        /// Read the body from stdin.
        #[arg(long)]
        stdin: bool,
        /// Dial via an explicit address ticket (JSON from `agent-mail addr`),
        /// bypassing discovery. Useful on LANs and for debugging.
        #[arg(long)]
        ticket: Option<String>,
        /// Output the queued message as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print this endpoint's full address (id + direct addrs + relay) as a JSON
    /// ticket that peers can use with `send --ticket`.
    Addr {
        #[arg(long)]
        json: bool,
    },
    /// List received messages.
    Inbox {
        /// Filter by peer NodeId or name.
        #[arg(long)]
        peer: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List outgoing messages awaiting delivery (queued or failed).
    Outbox {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read a message by its key (marks it read).
    Read {
        msg_key: String,
        #[arg(long)]
        json: bool,
    },
    /// Reply to a received message, continuing its thread.
    Reply {
        msg_key: String,
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AllowAction {
    /// Add a peer NodeId to the allowlist.
    Add {
        node_id: String,
        #[arg(short, long)]
        name: Option<String>,
        /// Mark this peer as a human (mail surfaced to human inboxes).
        #[arg(long)]
        human: bool,
    },
    List,
    Remove {
        /// Peer NodeId or name.
        query: String,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::resolve(cli.home)?;
    let config = Config::load(&paths.config_dir)?;
    match cli.command {
        Commands::Init => cmd_init(&paths),
        Commands::Id => cmd_id(&paths),
        Commands::Allow { action } => cmd_allow(&paths, action),
        Commands::Daemon => daemon::run(paths, config).await,
        Commands::Mcp => mcp::run(paths, config).await,
        Commands::Tui => tui::run(paths, config).await,
        Commands::Send {
            peer,
            message,
            stdin,
            ticket,
            json,
        } => cmd_send(&paths, &config, &peer, &message, stdin, ticket.as_deref(), json).await,
        Commands::Addr { json } => cmd_addr(&paths, json).await,
        Commands::Inbox { peer, json } => cmd_inbox(&paths, peer, json),
        Commands::Outbox { json } => cmd_outbox(&paths, json),
        Commands::Read { msg_key, json } => cmd_read(&paths, &msg_key, json),
        Commands::Reply {
            msg_key,
            message,
            stdin,
            json,
        } => cmd_reply(&paths, &config, &msg_key, &message, stdin, json).await,
    }
}

fn cmd_init(paths: &Paths) -> Result<()> {
    let sk = identity::load_or_generate(paths)?;
    std::fs::create_dir_all(&paths.config_dir)?;
    AllowList::load_or_create(&paths.allowed_keys)?;
    std::fs::create_dir_all(&paths.data_dir)?;
    let id = identity::node_id(&sk);
    println!("initialized agent-mail");
    println!("  home:        {}", paths.config_dir.display());
    println!("  secret key:  {}", paths.secret_key.display());
    println!("  allowlist:   {}", paths.allowed_keys.display());
    println!("  database:    {}", paths.db.display());
    println!("  node id:     {id}");
    println!();
    println!("run it:   agent-mail daemon --home {}", paths.config_dir.display());
    println!("share this node id with peers; they must allow it (and you theirs).");
    Ok(())
}

fn cmd_id(paths: &Paths) -> Result<()> {
    let sk = identity::load(paths)?;
    println!("{}", identity::node_id(&sk));
    Ok(())
}

fn cmd_allow(paths: &Paths, action: AllowAction) -> Result<()> {
    let mut list = AllowList::load_or_create(&paths.allowed_keys)?;
    match action {
        AllowAction::Add {
            node_id,
            name,
            human,
        } => {
            let display = name.clone().unwrap_or_else(|| node_id.clone());
            list.add(PeerEntry {
                node_id,
                name,
                human,
            })?;
            println!("added `{display}` to the allowlist");
        }
        AllowAction::List => {
            if list.entries.is_empty() {
                println!("allowlist is empty — no peers may connect");
            }
            for e in &list.entries {
                let kind = if e.human { "human" } else { "agent" };
                let name = e.name.as_deref().unwrap_or("");
                println!("{}\t{kind}\t{}", e.node_id, name);
            }
        }
        AllowAction::Remove { query } => {
            let removed = list.remove(&query)?;
            println!(
                "removed `{}` from the allowlist",
                removed.name.unwrap_or(removed.node_id)
            );
        }
    }
    Ok(())
}

fn read_body(message: &Option<String>, stdin: bool) -> Result<String> {
    if stdin {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else if let Some(m) = message {
        Ok(m.clone())
    } else {
        bail!("provide a body with -m/--message or --stdin")
    }
}

async fn cmd_addr(paths: &Paths, json: bool) -> Result<()> {
    let sk = identity::load(paths)?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(sk)
        .bind()
        .await
        .map_err(crate::util::de)?;
    endpoint.online().await;
    // give direct address discovery a moment to populate
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let addr = endpoint.addr();
    endpoint.close().await;
    if json {
        println!("{}", serde_json::to_string(&addr)?);
    } else {
        println!("id:    {}", addr.id);
        for a in &addr.addrs {
            println!("addr:  {a:?}");
        }
        println!();
        println!("ticket: {}", serde_json::to_string(&addr)?);
    }
    Ok(())
}

/// Resolve a peer query against the allowlist (outbound is allowlist-gated
/// too) and return its NodeId string.
fn resolve_peer(list: &AllowList, query: &str) -> Result<String> {
    let entry = list
        .resolve(query)
        .with_context(|| format!("`{query}` is not in the allowlist"))?;
    Ok(entry.node_id.clone())
}

async fn cmd_send(
    paths: &Paths,
    config: &Config,
    peer: &str,
    message: &Option<String>,
    stdin: bool,
    ticket: Option<&str>,
    json: bool,
) -> Result<()> {
    let sk = identity::load(paths)?;
    let me = identity::node_id(&sk);
    let list = AllowList::load(&paths.allowed_keys)?;
    let peer_id = resolve_peer(&list, peer)?;
    let body = read_body(message, stdin)?;
    if body.is_empty() {
        bail!("message body is empty");
    }
    if body.len() > config.max_message_bytes {
        bail!(
            "message body is {} bytes, exceeds the {}-byte limit",
            body.len(),
            config.max_message_bytes
        );
    }

    let store = Store::open(paths)?;
    let thread_id = ulid::Ulid::generate().to_string();

    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(sk.clone())
        .bind()
        .await
        .map_err(crate::util::de)?;
    let msg = store.enqueue_outgoing(
        &me,
        &peer_id,
        &thread_id,
        None,
        "text/plain",
        &body,
        config.retry_base_secs,
    )?;
    let result = if peer_id == me && ticket.is_none() {
        // Loopback: iroh can't dial our own NodeId; write into our inbox.
        store.record_incoming(&msg)?;
        Ok(crate::util::now_secs())
    } else {
        let target = match ticket {
            Some(t) => {
                let addr: iroh::EndpointAddr = serde_json::from_str(t)
                    .context("invalid --ticket (expected JSON from `agent-mail addr`)")?;
                if addr.id.to_string() != peer_id {
                    bail!("ticket id does not match peer {peer_id}");
                }
                addr
            }
            None => iroh::EndpointAddr::from(peer_id.parse::<iroh::EndpointId>()?),
        };
        client::deliver(&endpoint, target, &msg).await
    };
    endpoint.close().await;
    let msg_key = msg.id;

    match result {
        Ok(received_at) => {
            store.mark_delivered(&msg_key, received_at)?;
            print_send_result(&msg_key, &peer_id, true, json);
        }
        Err(e) => {
            store.mark_retry(&msg_key, 0, config.retry_base_secs, config.retry_max_secs)?;
            print_send_result(&msg_key, &peer_id, false, json);
            eprintln!("note: queued for retry by the daemon ({e:#})");
        }
    }
    Ok(())
}

fn print_send_result(msg_key: &str, peer: &str, delivered: bool, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({"msg_key": msg_key, "peer": peer, "delivered": delivered})
        );
    } else if delivered {
        println!("delivered to {peer} (msg {msg_key})");
    } else {
        println!("queued for {peer} (msg {msg_key})");
    }
}

fn cmd_outbox(paths: &Paths, json: bool) -> Result<()> {
    let store = Store::open(paths)?;
    let rows = store.list_outgoing()?;
    let (pending, failed) = store.outbox_counts();
    if json {
        println!(
            "{}",
            serde_json::json!({"pending": pending, "failed": failed, "messages": rows})
        );
        return Ok(());
    }
    println!("outbox: {pending} pending, {failed} failed");
    if rows.is_empty() {
        println!("nothing queued");
        return Ok(());
    }
    for r in &rows {
        let preview: String = r.body.chars().take(60).collect();
        let age = humanize(crate::util::now_secs().saturating_sub(r.created_at));
        println!(
            "{}  {}  attempts:{}  {age} old  {}",
            &r.status,
            &r.msg_key[..8.min(r.msg_key.len())],
            r.attempts,
            preview.replace('\n', " "),
        );
    }
    Ok(())
}

fn cmd_inbox(paths: &Paths, peer: Option<String>, json: bool) -> Result<()> {
    let store = Store::open(paths)?;
    let peer_id = match &peer {
        Some(p) => {
            let list = AllowList::load(&paths.allowed_keys)?;
            Some(resolve_peer(&list, p)?)
        }
        None => None,
    };
    let rows = store.list_inbox(peer_id.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("inbox is empty");
        return Ok(());
    }
    for r in &rows {
        let unread = if r.read_at.is_none() { "*" } else { " " };
        let preview: String = r.body.chars().take(60).collect();
        println!(
            "{unread} {}  {}  {} ago  {}",
            &r.msg_key[..8.min(r.msg_key.len())],
            r.from_id,
            humanize(crate::util::now_secs().saturating_sub(r.created_at)),
            preview.replace('\n', " "),
        );
    }
    Ok(())
}

fn cmd_read(paths: &Paths, msg_key: &str, json: bool) -> Result<()> {
    let store = Store::open(paths)?;
    let msg = store
        .get(msg_key)?
        .with_context(|| format!("no message `{msg_key}`"))?;
    if msg.direction == "in" {
        store.mark_read(&msg.msg_key)?;
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&msg)?);
    } else {
        println!("from:  {}", msg.from_id);
        println!("to:    {}", msg.to_id);
        println!("thread: {}", msg.thread_id);
        println!("at:    {}", msg.created_at);
        println!();
        println!("{}", msg.body);
    }
    Ok(())
}

async fn cmd_reply(
    paths: &Paths,
    config: &Config,
    msg_key: &str,
    message: &Option<String>,
    stdin: bool,
    json: bool,
) -> Result<()> {
    let sk = identity::load(paths)?;
    let me = identity::node_id(&sk);
    let store = Store::open(paths)?;
    let original = store
        .get(msg_key)?
        .with_context(|| format!("no message `{msg_key}`"))?;
    if original.direction != "in" {
        bail!("can only reply to received messages");
    }
    let body = read_body(message, stdin)?;
    if body.is_empty() {
        bail!("message body is empty");
    }
    if body.len() > config.max_message_bytes {
        bail!(
            "message body is {} bytes, exceeds the {}-byte limit",
            body.len(),
            config.max_message_bytes
        );
    }

    // For an inbound message, the wire `in_reply_to` is the sender's own
    // message id (the `remote_id` of our row), not our local row key.
    let reply_to = Some(original.remote_id.as_str());
    let thread_id = original.thread_id.clone();
    let peer_id = original.from_id.clone();
    let msg = store.enqueue_outgoing(
        &me,
        &peer_id,
        &thread_id,
        reply_to,
        "text/plain",
        &body,
        config.retry_base_secs,
    )?;

    let result = if peer_id == me {
        // Loopback: iroh can't dial our own NodeId; write into our inbox.
        store.record_incoming(&msg)?;
        Ok(crate::util::now_secs())
    } else {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(sk.clone())
            .bind()
            .await
            .map_err(crate::util::de)?;
        let r = client::deliver(
            &endpoint,
            iroh::EndpointAddr::from(peer_id.parse::<iroh::EndpointId>()?),
            &msg,
        )
        .await;
        endpoint.close().await;
        r
    };

    match result {
        Ok(received_at) => {
            store.mark_delivered(&msg.id, received_at)?;
            print_send_result(&msg.id, &peer_id, true, json);
        }
        Err(e) => {
            store.mark_retry(&msg.id, 0, config.retry_base_secs, config.retry_max_secs)?;
            print_send_result(&msg.id, &peer_id, false, json);
            eprintln!("note: queued for retry by the daemon ({e:#})");
        }
    }
    Ok(())
}

/// Compact relative age for inbox listings.
fn humanize(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}
