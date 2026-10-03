#!/bin/bash
# Set up two test identities with cross-allowlists for mcp_smoke.py.
# Usage: scripts/mcp_smoke_setup.sh /path/to/agent-mail
#
# No standalone daemons are started: each `agent-mail mcp` server embeds its
# own daemon for the lifetime of the stdio session.
set -euo pipefail
BIN="${1:-target/debug/agentmail}"
BASE=/tmp/am-mcp-test
rm -rf "$BASE" && mkdir -p "$BASE"/{a,b}/{cfg,data}
for X in a b; do
  AGENT_MAIL_CONFIG_DIR="$BASE/$X/cfg" AGENT_MAIL_DATA_DIR="$BASE/$X/data" "$BIN" init >/dev/null
done
NODE_A=$(AGENT_MAIL_CONFIG_DIR="$BASE/a/cfg" AGENT_MAIL_DATA_DIR="$BASE/a/data" "$BIN" id)
NODE_B=$(AGENT_MAIL_CONFIG_DIR="$BASE/b/cfg" AGENT_MAIL_DATA_DIR="$BASE/b/data" "$BIN" id)
AGENT_MAIL_CONFIG_DIR="$BASE/a/cfg" "$BIN" allow add "$NODE_B" -n agent-b >/dev/null
AGENT_MAIL_CONFIG_DIR="$BASE/b/cfg" "$BIN" allow add "$NODE_A" -n agent-a >/dev/null
echo "setup complete: A=$NODE_A B=$NODE_B (daemons are embedded in each mcp server)"
