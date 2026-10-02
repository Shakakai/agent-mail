#!/usr/bin/env python3
"""MCP smoke test: drive two `agent-mail mcp` servers over stdio JSON-RPC.

Requires test identities under /tmp/am-mcp-test/{a,b} with cross-allowlists
and B's daemon running (see scripts/mcp_smoke_setup.sh).

Usage: python3 scripts/mcp_smoke.py /path/to/agent-mail
"""

import json
import select
import subprocess
import sys
import time


class McpClient:
    def __init__(self, binary: str, config_dir: str, data_dir: str,
                 protocol_version: str = "2025-06-18", modern: bool = False):
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
        self.modern = modern
        self.protocol_version = protocol_version
        if modern:
            # 2026-07-28 lifecycle: no initialize; open with `discover` and
            # carry self-contained _meta on every request (including this one).
            self.init = self._request({"jsonrpc": "2.0", "id": self.next_id(),
                                       "method": "server/discover",
                                       "params": {"_meta": self.meta()}})
        else:
            self.init = self._request({"jsonrpc": "2.0", "id": self.next_id(), "method": "initialize", "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "smoke", "version": "0"},
            }})
            self._request({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def meta(self) -> dict:
        return {
            "io.modelcontextprotocol/protocolVersion": self.protocol_version,
            "io.modelcontextprotocol/clientCapabilities": {"resources": {}, "tools": {}},
        }

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

    def notify(self, method: str, params: dict):
        self.proc.stdin.write(json.dumps(
            {"jsonrpc": "2.0", "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()

    def wait_line(self, timeout: float):
        fd = self.proc.stdout.fileno()
        deadline = time.time() + timeout
        while time.time() < deadline:
            remaining = deadline - time.time()
            r, _, _ = select.select([fd], [], [], min(remaining, 2.0))
            if r:
                line = self.proc.stdout.readline()
                if line:
                    return json.loads(line)
        return None

    def wait_notification(self, method: str, timeout: float = 30.0) -> dict:
        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = self.wait_line(min(2.0, deadline - time.time()))
            if msg and msg.get("method") == method:
                return msg
        raise AssertionError(f"no notification {method!r} within {timeout}s")

    def wait_response(self, req_id, timeout: float = 10.0) -> dict:
        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = self.wait_line(min(2.0, deadline - time.time()))
            if msg and msg.get("id") == req_id:
                if "error" in msg:
                    raise RuntimeError(f"rpc error: {msg['error']}")
                return msg.get("result", {})
        raise AssertionError(f"no response for request {req_id} within {timeout}s")

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
                         "list_outbox", "list_threads", "read_message", "reply",
                         "send_message"], names

        # Give both embedded daemons a moment to come online and publish
        # discovery records; n0's free tier is rate-limited and flaky.
        time.sleep(8)
        sent = a.call("send_message", {"peer": "agent-b", "body": "hello via MCP"})
        print("A send:", sent["status"], sent["msg_key"][:12])

        # Immediate delivery is best-effort (offline or undiscovered peers
        # queue); what must hold is eventual delivery via the daemon retry.
        deadline = time.time() + 90
        inbox = []
        while time.time() < deadline:
            time.sleep(3)
            inbox = b.call("list_inbox", {"unread_only": True})
            if any(m["body"] == "hello via MCP" for m in inbox):
                break
        assert any(m["body"] == "hello via MCP" for m in inbox), \
            f"send was not delivered to B within 90s: {inbox}"
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

        # --- subscriptions: capabilities + resource surface ---
        caps = a.init.get("capabilities", {})
        assert caps.get("resources", {}).get("subscribe") is True, caps
        print("capabilities advertise resources.subscribe")

        res = a._request({"jsonrpc": "2.0", "id": a.next_id(), "method": "resources/list"})
        uris = [r["uri"] for r in res["resources"]]
        assert "agent-mail://inbox" in uris, uris
        content = a._request({"jsonrpc": "2.0", "id": a.next_id(), "method": "resources/read",
                              "params": {"uri": "agent-mail://inbox"}})
        assert "ack over MCP" in content["contents"][0]["text"]
        print("resources/list + resources/read ok")

        # --- legacy resources/subscribe (pre-2026-07-28 clients) ---
        a._request({"jsonrpc": "2.0", "id": a.next_id(), "method": "resources/subscribe",
                    "params": {"uri": "agent-mail://inbox"}})
        b.call("send_message", {"peer": "agent-a", "body": "legacy sub probe"})
        note = a.wait_notification("notifications/resources/updated", timeout=90)
        assert note["params"]["uri"] == "agent-mail://inbox", note
        print("legacy resources/subscribe: notification received")

        a._request({"jsonrpc": "2.0", "id": a.next_id(), "method": "resources/unsubscribe",
                    "params": {"uri": "agent-mail://inbox"}})

        # --- 2026-07-28 subscriptions/listen ---
        a28 = McpClient(binary, f"{base}/a/cfg", f"{base}/a/data",
                        protocol_version="2026-07-28", modern=True)
        try:
            listen_id = a28.next_id()
            a28.proc.stdin.write(json.dumps({
                "jsonrpc": "2.0", "id": listen_id, "method": "subscriptions/listen",
                "params": {
                    "notifications": {"resourceSubscriptions": ["agent-mail://inbox"]},
                    "_meta": a28.meta(),
                },
            }) + "\n")
            a28.proc.stdin.flush()
            ack = a28.wait_notification("notifications/subscriptions/acknowledged", timeout=10)
            accepted = ack["params"].get("notifications", {})
            assert accepted.get("resourceSubscriptions") == ["agent-mail://inbox"], ack
            assert ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"] == listen_id
            print("subscriptions/listen acknowledged:", accepted)

            b.call("send_message", {"peer": "agent-a", "body": "listen sub probe"})
            note = a28.wait_notification("notifications/resources/updated", timeout=90)
            assert note["params"]["uri"] == "agent-mail://inbox", note
            print("subscriptions/listen: notification received")

            # cancel the listen stream: rmcp cancels the in-flight listen and
            # (per stdio cancellation semantics) drops its response; the
            # subscription must simply stop producing notifications.
            a28.proc.stdin.write(json.dumps({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": listen_id, "reason": "test done"},
            }) + "\n")
            a28.proc.stdin.flush()
            time.sleep(1.5)
            print("subscriptions/listen cancelled")

            # after cancellation no further notifications arrive
            b.call("send_message", {"peer": "agent-a", "body": "post-cancel probe"})
            assert a28.wait_line(8.0) is None, "unexpected notification after cancel"
            print("no notifications after cancel")
        finally:
            a28.close()

        print("\nALL MCP SMOKE TESTS PASSED")
    finally:
        a.close()
        b.close()


if __name__ == "__main__":
    main()
