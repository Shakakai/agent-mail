#!/bin/bash
# Set up two test identities with cross-allowlists and B's daemon running.
# Usage: scripts/mcp_smoke_setup.sh /path/to/agent-mail
set -euo pipefail
BIN="${1:-target/debug/agent-mail}"
BASE=/tmp/am-mcp-test
pkill -f "agent-mail daemon" 2>/dev/null || true
rm -rf "$BASE" && mkdir -p "$BASE"/{a,b}/{cfg,data}
for X in a b; do
  AGENT_MAIL_CONFIG_DIR="$BASE/$X/cfg" AGENT_MAIL_DATA_DIR="$BASE/$X/data" "$BIN" init >/dev/null
done
NODE_A=$(AGENT_MAIL_CONFIG_DIR="$BASE/a/cfg" AGENT_MAIL_DATA_DIR="$BASE/a/data" "$BIN" id)
NODE_B=$(AGENT_MAIL_CONFIG_DIR="$BASE/b/cfg" AGENT_MAIL_DATA_DIR="$BASE/b/data" "$BIN" id)
AGENT_MAIL_CONFIG_DIR="$BASE/a/cfg" "$BIN" allow add "$NODE_B" -n agent-b >/dev/null
AGENT_MAIL_CONFIG_DIR="$BASE/b/cfg" "$BIN" allow add "$NODE_A" -n agent-a >/dev/null
for X in a b; do
  AGENT_MAIL_CONFIG_DIR="$BASE/$X/cfg" AGENT_MAIL_DATA_DIR="$BASE/$X/data" RUST_LOG=off "$BIN" daemon >/dev/null 2>&1 &
  echo $! > "$BASE/$X.pid"
done
sleep 5
echo "setup complete: A=$NODE_A B=$NODE_B (B daemon pid $(cat "$BASE/b.pid"))"
