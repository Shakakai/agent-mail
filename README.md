# agent-mail

**Peer-to-peer mail for AI agents (and the humans they work with).**

agent-mail is a CLI tool + local MCP server that lets an AI agent exchange
messages with other agents (and with humans) over [IROH](https://iroh.computer)
— a public-key-based, peer-to-peer QUIC networking stack. No central server,
no accounts, no DNS entries to manage: agents find each other by ed25519
public key, and IROH handles NAT traversal, hole punching, and relay fallback
automatically.

## Status

**All milestones M2–M5 implemented and verified** (Rust, iroh 1.3):
identity, allowlist (inbound + outbound gating), `agent-mail/3` protocol
(hello/send/ack/error), SQLite inbox/outbox with at-least-once delivery and
daemon retry, CLI, stdio MCP server + agent skill, and the ratatui TUI. M6 (extensions: mailbox capability, attachments, multi-device)
remains future work.

Verified against the live n0 network: two agents on one machine exchanging
mail over QUIC (direct and/or relayed), offline queue delivering when the
peer's daemon comes online, and allowlist rejects logging unauthorized
NodeIds.

### Lessons baked into the code

- **The endpoint must be bound with your `SecretKey`**
  (`Endpoint::builder(preset).secret_key(sk).bind()`). `Endpoint::bind(preset)`
  alone generates an *ephemeral* identity — discovery records, relay
  connections, and TLS identity all use it, so peers dial a key that does
  not exist and everything times out with no useful error.
- **NodeIds display as hex** in iroh 1.3 (`PublicKey`'s `Display`); z-base-32
  (`to_z32()`) is only used inside pkarr/DNS records.
- **n0's free public discovery (dns.iroh.link) has no uptime guarantee and
  is rate-limited.** It flaps; publishes and lookups fail intermittently
  for minutes at a time. Fine for dev — for production, self-host
  `iroh-dns-server` or use explicit tickets on trusted networks.
- QUIC send streams must be `finish()`ed explicitly; the framing layer does
  not do it for you.
- **iroh refuses to dial your own NodeId** ("Connecting to ourself is not
  supported"). Sends addressed to ourselves short-circuit: the message is
  written straight into the local inbox (`ops::send_message`, CLI
  send/reply, and the daemon's outbox retry all handle loopback), which is
  also how the TUI and an agent on the same installation talk to each
  other.


---

## Why IROH

Every IROH endpoint has a globally unique **NodeId** — the public half of an
ed25519 keypair. All connections are:

- **Authenticated**: you always know exactly which public key you're talking
  to, verified by the QUIC/TLS handshake itself (no certificates, no PKI).
- **End-to-end encrypted**: traffic is encrypted to the recipient's key.
- **NAT-traversed**: IROH discovers relay servers and direct paths, punches
  holes through NATs, and falls back to relaying when direct paths fail.

This makes trust management trivially simple: **an agent is allowed to talk to
exactly the public keys in its allowlist, and no one else.** There is no
"unauthenticated connection" state to defend.

The transport pieces we use directly:

| IROH concept | What we use it for |
|---|---|
| `Endpoint` | One long-lived node per agent (holds identity + all connections) |
| ALPN | Protocol multiplexing — `agent-mail/3` for mail |
| QUIC streams | One bidirectional stream per mail exchange |
| Discovery (n0 DNS / pkarr) | Resolving a NodeId to current dialing info |
| Relays | Connectivity when both peers are behind NAT; hole-punch assist |

---

## Architecture at a glance

```
┌──────────────┐   QUIC + agent-mail/3   ┌──────────────┐
│   Agent A    │ ◄─────────────────────► │   Agent B    │
│  (this tool) │   direct, or via relay  │  (this tool) │
└──────┬───────┘                         └──────┬───────┘
       │                                        │
  ┌────┴────┐                              ┌────┴────┐
  │ MCP srv │ ◄── stdio ── pi / Claude    │ MCP srv │
  │ daemon  │                             │ daemon  │
  │ SQLite  │                             │ SQLite  │
  └─────────┘                             └─────────┘
```

Each installation runs a **daemon** (the `agent-mail daemon` command or a
background service) that owns the IROH endpoint, receives mail, retries the
outbox, and exposes everything else through a local SQLite store. Agents and
humans interact with it through the CLI or the MCP server — they never touch
the network directly.

---

## Files & configuration

**Multiple nodes on one box.** Every command accepts a global `--home DIR`
flag (env: `AGENT_MAIL_HOME`) that points at one self-contained folder
holding everything a node needs — secret key, allowlist, config, and mail
store:

```sh
agent-mail init --home ./alice        # create a new private key + folder
agent-mail daemon --home ./alice      # run it (same flag reattaches later)
agent-mail init --home ./bob && agent-mail daemon --home ./bob   # a second node
```

Without `--home`, paths follow the XDG convention and can be relocated with
env overrides:

| Path | Override env var | Purpose |
|---|---|---|
| `~/.config/agent-mail/` | `AGENT_MAIL_CONFIG_DIR` | Config root |
| `~/.config/agent-mail/secret-key` | `AGENT_MAIL_SECRET_KEY` (path) | ed25519 secret key, `0600` |
| `~/.config/agent-mail/allowed-keys.toml` | `AGENT_MAIL_ALLOWED_KEYS` (path) | Trust: who may talk to me |
| `~/.config/agent-mail/config.toml` | — | Tuning: message size, retry backoff, relay & discovery |

All config knobs (defaults shown):

```toml
max_message_bytes       = 1048576   # body limit
retry_base_secs         = 30        # outbox backoff base
retry_max_secs          = 900       # outbox backoff cap
daemon_tick_secs        = 15        # outbox scan interval
mcp_allow_trust_changes = false     # MCP allow_add gate
relay                   = "n0"      # "n0" | "disabled" | "custom"
relay_urls              = []        # required when relay = "custom"
discovery               = true      # n0 pkarr/DNS publish+resolve
```
| `~/.local/share/agent-mail/mail.db` | `AGENT_MAIL_DATA_DIR` | SQLite: inbox, outbox, threads |

### Identity (`secret-key`)

One ed25519 keypair per agent. On first run a key is generated and written
with `0600` permissions. **The NodeId derived from this key *is* the agent's
address** — the only thing you ever exchange with a peer.

### Allowlist (`allowed-keys.toml`)

The trust model. Both inbound *and* outbound connections are restricted to
keys listed here:

```toml
# ~/.config/agent-mail/allowed-keys.toml
# Every entry is an agent (or human client) this agent may exchange mail with.

[[peer]]
node_id = "q7f3…zbase32…"   # IROH NodeId (z-base-32, as shown by `agent-mail id`)
name = "research-agent"      # Free-form label, shown in `inbox` etc.
human = false                # `true` = messages may be surfaced to a human UI

[[peer]]
node_id = "k2d9…zbase32…"
name = "todd"
human = true
```

- Inbound: connections from NodeIds not in this file are **rejected at the
  ALPN/protocol layer** before any mail envelope is exchanged.
- Outbound: `send` refuses to dial NodeIds not in this file.
- The file is re-read per connection, so edits take effect immediately —
  no daemon restart required.
- `AGENT_MAIL_ALLOWED_KEYS=/path/to/file.toml` overrides the location
  (useful for testing, containers, and running multiple agents on one box).

---

## The agent-mail protocol (v3)

A small, strict protocol on top of IROH's raw QUIC API. Design goals:
debuggable by a human with `agent-mail debug`, forward-evolvable, and strict
about identity.

### Transport

- **ALPN:** `agent-mail/3` (versioned; incompatible changes bump the version).
  v1 carried an `audience` field (removed in v2); v2 dropped attachments
  (added in v3, base64 inline, 20 MiB per attachment).
- **Connections:** one QUIC connection per peer pair, kept open while the
  daemon runs. Dial-on-demand; the accepting side uses IROH's `Router` with a
  protocol handler for the ALPN.
- **Streams:** one bidirectional QUIC stream per exchange. The initiator opens
  the stream, writes one frame, **half-closes** (`finish()`), and reads one
  response frame. This keeps interleaving trivial: stream = request/response
  pair. QUIC handles concurrency.

### Framing

```
┌─────────────────────┬───────────────────────────┐
│ length : u32le      │ payload : JSON, UTF-8     │
│ (max 1 MiB default) │                           │
└─────────────────────┴───────────────────────────┘
```

Length-prefixed JSON. JSON (rather than CBOR) for v1 because every agent
can read it, `agent-mail debug` can dump raw frames, and 1 MiB covers the
target use cases. A future `attachments` extension will move large payloads
to content-addressed transfer (`iroh-blobs`) without changing the envelope.

### Envelope

Every frame is a JSON object with a `v` version and a `type`:

```json
{
  "v": 1,
  "type": "send",
  "id": "01J8Z2K4QW...",          
  ...
}
```

`id` is a ULID (time-ordered, unique) identifying the **frame**; separate
from the message id it carries.

### Handshake

Immediately after the QUIC connection is established, **both sides** open a
stream and send a `hello` frame (direction is symmetric — either peer may
dial first, e.g. after a NAT event):

```json
{ "v": 2, "type": "hello", "id": "<ulid>",
  "agent": { "name": "agent-mail", "version": "0.1.0" },
  "caps": ["mail"],
  "since": 1727740800 }
```

- `caps` advertises optional protocol extensions (`mailbox`, `attachments`).
- A peer whose NodeId fails the allowlist check is dropped **here** — before
  any `hello` is answered.

### Message types

#### `send` — deliver a message

```json
{ "v": 2, "type": "send", "id": "<frame-ulid>",
  "msg": {
    "id": "<msg-ulid>",
    "thread_id": "<msg-ulid of thread root>",
    "in_reply_to": "<msg-ulid | null>",
    "from": "<sender-node-id-z32>",
    "to": "<recipient-node-id-z32>",
    "created_at": 1727740800,
    "content_type": "text/plain",
    "body": "Found the bug. Fix is on branch fix/parser-crash.",
    "attachments": [
      { "name": "crash.log", "content_type": "text/plain",
        "size": 4210, "data_base64": "…" }
    ]
  } }
```

- `from`/`to` are the wire-level IROH NodeIds. If either doesn't match the
  actual QUIC connection peers, the receiver **must** reject with `error`.
- `content_type` starts as `text/plain` and `application/json`; extensible.
- The sender half-closes the stream after the frame and awaits the response.

#### `ack` / `error` — response frames

```json
{ "v": 2, "type": "ack", "id": "<frame-ulid>", "of": "<msg-ulid>",
  "received_at": 1727740801 }
```

```json
{ "v": 2, "type": "error", "id": "<frame-ulid>", "of": "<msg-ulid|null>",
  "code": "not_allowed | identity_mismatch | too_large | malformed | internal",
  "message": "human-readable detail" }
```

`ack` means *persisted to the recipient's inbox* — that is the delivery
guarantee boundary. Once `ack` is received, the sender moves the message
from outbox to sent. This gives at-least-once delivery: a crash between the
recipient's persist and the sender's outbox-clear can produce a duplicate,
which recipients dedupe on `msg.id`.

### Delivery model

**IROH relays do not store data.** A message to an offline peer cannot be
delivered until that peer comes online. agent-mail handles this with a
persistent **outbox + retry**:

1. `send` validates the allowlist and the size limit (bodies must fit in
   one wire frame, 1 MiB by default), writes the message to the local
   outbox, then attempts delivery.
2. On success (`ack`), the message moves to `sent`.
3. On connection failure, the daemon retries with exponential backoff +
   jitter (30s → 15min cap). Messages survive daemon restarts.
4. After 10 failed attempts the message is marked `failed` — it leaves the
   retry loop but stays visible: `agent-mail outbox`, the TUI status bar
   (`outbox:N (M failed)`), and the MCP `list_outbox` tool all surface it,
   so exhaustion is never silent.
4. `send --deadline <when>` can fail fast instead, for interactive use.

**Optional extension — `mailbox` capability** (flagged in `hello.caps`):
any trusted peer can volunteer to store mail for offline peers it knows
about. Recipients connect and pull. This is deliberately *not* in v1's core
path — it adds real trust/flood complexity — but the ALPN is reserved:
`agent-mail/mailbox/1`, with `deposit` / `list` / `fetch` / `delete` frames.

---

## CLI design

```
agent-mail id                      # print my NodeId (to hand to a peer)
agent-mail init                    # generate identity, config, empty allowlist
agent-mail allow add <node-id> [-n name] [--human]
agent-mail allow list | remove

agent-mail send <node-id-or-name> [-s subject-ish] [-m body] [--stdin]
agent-mail inbox [--human] [--json]
agent-mail read <msg-id> [--json]
agent-mail reply <msg-id> [-m body] [--stdin]

agent-mail daemon                  # run the mail daemon (foreground)
agent-mail mcp                     # serve MCP over stdio (for agents)
agent-mail tui                     # human mail client
agent-mail tui                     # human mail client (M5)
agent-mail addr [--json]           # print my full address ticket (for --ticket)
agent-mail debug dump <frame...>   # decode raw frames (planned)
```

Everything is also available via MCP, which is the primary interface for
agents; the CLI and TUI are for humans and for scripting.

## TUI

`agent-mail tui` (or `agent-mail` with no subcommand when attached to a
TTY) launches the human mail client:

- **Three-pane layout**: thread list → conversation view → compose — in the
  spirit of mutt/aerc, keyboard-driven, mouse optional.
- **Inbox**: unread markers, per-peer grouping, live refresh as the daemon
  delivers.
- **Reading**: full thread view with my-key vs. peer color coding, raw
  message inspection (`v` key — dumps the stored row as JSON).
- **Composing**: inline editor for the peer address and body, with
  tab-completion of allowlisted peer names.
- **Trust management**: allowlist editor (add/remove peers, paste a NodeId,
  see pending inbound attempts that were rejected).
- **Status bar**: own NodeId (for sharing), connection state per peer
  (direct/relay/offline), outbox backlog count.

Crates: `ratatui` + `crossterm` + `tokio`, talking to the same daemon/store
as the CLI and MCP server.

## MCP server

`agent-mail mcp` serves a stdio MCP server exposing:

| Tool | Purpose |
|---|---|
| `get_identity` | My NodeId, to give to other agents |
| `send_message` | Send mail to an allowlisted peer (queues if offline) |
| `list_inbox` | List inbox (filter: unread, peer) |
| `read_message` | Full message by id |
| `reply` | Reply into a thread |
| `list_threads` | Conversation overview |
| `list_outbox` | Pending + failed outgoing mail (nothing is silently dropped) |
| `allow_add` / `allow_list` | Manage the trust list (gated by config) |

The MCP server is a thin adapter over the same core the CLI uses; it talks
to the running daemon through the SQLite store + a local control socket, so
an agent never holds network code or keys itself.

The MCP server **embeds the daemon**: it starts in-process when the server
starts and shuts down automatically when the stdio session ends. An agent's
mail endpoint is therefore online exactly while its agent harness is
connected — no separately managed daemon process is needed (a standalone
`agent-mail daemon` remains available for always-on mail).

## Agent skills

A skill (e.g. for pi / Claude Code) ships with the tool so an agent knows
the mail workflow exists and how to use it:

- **identity etiquette** — share NodeId when asked, verify peers by NodeId
- **messaging** — send, check inbox before/after delegating work, use
  threads, keep bodies focused
- **trust hygiene** — never add keys it wasn't told to add; treat NodeIds
  as the only trusted identity

---

## Technical build plan

**Language: Rust** (primary recommendation). IROH is Rust-first; the
`iroh` crate gives us endpoints, the protocol Router, and first-class
discovery/relay support, with mature examples. The MCP server uses `rmcp`
(stdio). SQLite via `rusqlite` (bundled).

Single binary `agent-mail` (one crate, modules — split later only if
needed):

```
src/
  main.rs        # entry: logging, path resolution
  cli.rs         # clap CLI: all subcommands, global --home flag
  identity.rs    # key generation/loading, NodeId formatting
  allowlist.rs   # toml allowlist, re-read per connection, atomic saves
  proto.rs       # envelope types, frame codec (u32le + JSON), error codes
  daemon.rs      # Endpoint + Router, inbox writer, outbox retry, shutdown
  store.rs       # rusqlite schema: inbox, outbox, threads; unit tests
  ops.rs         # shared send/read/reply operations (CLI + MCP + TUI)
  mcp.rs         # rmcp stdio server with embedded daemon
  tui.rs         # ratatui client: panes, compose, allowlist editor
```

Key dependencies: `iroh`, `clap`, `serde`/`serde_json`, `toml`, `rusqlite`,
`rmcp`, `ulid`, `tokio`, `tracing`, `ratatui`, `crossterm`.

### Milestones

1. **M1 — spec sign-off** (this document).
2. **M2 — connectivity**: ✅ done. Identity, allowlist, endpoint + ALPN
   handler, `hello` handshake, delivery across the public n0 relays.
3. **M3 — mail**: ✅ done. Envelope types, send/ack/error, SQLite
   inbox/outbox, retry loop, full `send`/`inbox`/`read`/`reply` CLI,
   `addr`/`--ticket` for explicit dialing.
4. **M4 — MCP + skills**: ✅ done. `agent-mail mcp` (rmcp 3, stdio) exposes
   `get_identity`, `send_message`, `list_inbox`, `read_message`, `reply`,
   `list_threads`, `allow_list`, and a config-gated `allow_add`
   (`mcp_allow_trust_changes`, default off). **Subscriptions** (new mail
   notifications): the server advertises the `agent-mail://inbox` resource
   (`resources/list`/`resources/read`) with `resources.subscribe: true`, and
   notifies subscribers via `notifications/resources/updated`. Both
   subscription styles are supported: legacy `resources/subscribe` /
   `resources/unsubscribe` (2025-xx clients) and 2026-07-28
   `subscriptions/listen` with `SubscriptionFilter.resourceSubscriptions`
   (ack via `notifications/subscriptions/acknowledged`; cancel with
   `notifications/cancelled`). A background watcher polls the store for new
   inbound rows and fans out. Shared ops live in `ops.rs`; skill ships at
   `.pi/skills/agent-mail/SKILL.md`; `scripts/mcp_smoke.py` drives a
   two-agent JSON-RPC conversation plus both subscription paths end to end.
5. **M5 — TUI**: ✅ done. `agent-mail tui`: three-pane keyboard-driven
   client (thread list, conversation with me/peer color coding, status
   bar), compose/reply modal (Tab fields, Ctrl-S send), allowlist editor
   showing rejected inbound attempts (daemon records them), raw JSON view,
   2s live refresh. `scripts/tui_smoke.py` drives the real TUI over a PTY,
   composes a message, and verifies delivery.
6. **M6 (later) — extensions**: `mailbox` capability, attachments via
   `iroh-blobs`, multi-device identity.

---

## Decisions (locked)

1. **Language: Rust** — IROH is Rust-first; we use the `iroh` crate directly
   (endpoint, protocol Router, discovery, relays).
2. **Offline delivery: outbox-retry for v1** — no mailbox servers. Messages
   to offline peers persist in the outbox and retry with backoff until
   acked. The `mailbox` capability stays reserved as a later extension.
3. **Human surface: full TUI** — the CLI ships with a full-featured
   terminal UI so humans can comfortably read and reply to mail.

## Open questions

None blocking. TBD during implementation: exact TUI widget set (see M5),
relay default (n0 public relays vs. self-hosted), and service install
targets (launchd + systemd).
