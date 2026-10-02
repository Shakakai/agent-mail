#!/usr/bin/env python3
"""MCP smoke test: drive two `agent-mail mcp` servers over stdio JSON-RPC.

Requires test identities under /tmp/am-mcp-test/{a,b} with cross-allowlists
and B's daemon running (see scripts/mcp_smoke_setup.sh).

Usage: python3 scripts/mcp_smoke.py /path/to/agent-mail
"""

import json
import subprocess
import sys
import time


class McpClient:
    def __init__(self, binary: str, config_dir: str, data_dir: str):
        env = {
            "PATH": "/usr/bin:/bin:/usr/local/bin",
            "AGENT_MAIL_CONFIG_DIR": config_dir,
            "AGENT_MAIL_DATA_DIR": data_dir,
            "RUST_LOG": "off",
        }
        self.proc = subprocess.Popen(
            [binary, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
        )
        self._id = 0
        self._request({"jsonrpc": "2.0", "id": self.next_id(), "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"},
        }})
        self._request({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def next_id(self):
        self._id += 1
        return self._id

    def _request(self, msg: dict):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        if "id" not in msg:
            return None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server exited")
            resp = json.loads(line)
            if resp.get("id") == msg["id"]:
                if "error" in resp:
                    raise RuntimeError(f"rpc error: {resp['error']}")
                return resp.get("result", {})

    def call(self, tool: str, args: dict):
        result = self._request({"jsonrpc": "2.0", "id": self.next_id(), "method": "tools/call", "params": {
            "name": tool,
            "arguments": args,
        }})
        content = result["content"][0]
        if result.get("isError"):
            return {"error": content["text"]}
        return json.loads(content["text"])

    def close(self):
        try:
            self.proc.terminate()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/agent-mail"
    base = "/tmp/am-mcp-test"

    a = McpClient(binary, f"{base}/a/cfg", f"{base}/a/data")
    b = McpClient(binary, f"{base}/b/cfg", f"{base}/b/data")

    try:
        ident_a = a.call("get_identity", {})
        print("A identity:", ident_a["node_id"][:16], "...")

        tools = a._request({"jsonrpc": "2.0", "id": a.next_id(), "method": "tools/list"})
        names = sorted(t["name"] for t in tools["tools"])
        print("tools:", names)
        assert names == ["allow_add", "allow_list", "get_identity", "list_inbox",
                         "list_threads", "read_message", "reply", "send_message"], names

        sent = a.call("send_message", {"peer": "agent-b", "body": "hello via MCP"})
        print("A send:", sent["status"], sent["msg_key"][:12])
        assert sent["status"] == "delivered", sent

        time.sleep(1.5)
        inbox = b.call("list_inbox", {"unread_only": True})
        assert len(inbox) == 1 and inbox[0]["body"] == "hello via MCP", inbox
        key = inbox[0]["msg_key"]
        print("B inbox: 1 unread from", inbox[0]["from_id"][:12], "...")

        msg = b.call("read_message", {"msg_key": key})
        assert msg["read_at"] is not None, "read_message should mark read"
        print("B read ok, marked read")

        replied = b.call("reply", {"msg_key": key, "body": "ack over MCP"})
        print("B reply:", replied["status"], "(daemon will retry if queued)")

        # with n0 free-tier discovery being flaky, the reply may queue first;
        # poll A's inbox until the daemon's retry loop delivers it
        deadline = time.time() + 90
        ok = False
        while time.time() < deadline:
            time.sleep(3)
            inbox_a = a.call("list_inbox", {"unread_only": True})
            if any(m["body"] == "ack over MCP" for m in inbox_a):
                ok = True
                break
        assert ok, "reply was not delivered to A within 90s"
        print("A received reply (via daemon retry)")

        threads = a.call("list_threads", {})
        assert threads[0]["unread_count"] >= 1, threads
        print("A threads:", len(threads), "latest unread:", threads[0]["unread_count"])

        # trust gate: allow_add must be rejected by default (JSON-RPC error)
        try:
            a.call("allow_add", {"node_id": "x" * 64})
            raise AssertionError("allow_add should be rejected by default")
        except RuntimeError as e:
            assert "disabled" in str(e), e
        print("allow_add correctly gated by default")

        print("\nALL MCP SMOKE TESTS PASSED")
    finally:
        a.close()
        b.close()


if __name__ == "__main__":
    main()
