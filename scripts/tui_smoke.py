#!/usr/bin/env python3
"""TUI smoke test: run `agent-mail tui` under a PTY, drive it with
keystrokes (help, allowlist, compose+send), and verify the composed message
arrives at the peer.

Setup: /tmp/am-tui-test/{a,b} identities, cross-allowlisted, B's daemon
running. Usage: python3 scripts/tui_smoke.py /path/to/agent-mail
"""

import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time
import fcntl
import re


def strip_ansi(b: bytes) -> str:
    return re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", b.decode(errors="replace"))


def squashed(b: bytes) -> str:
    """ANSI-stripped, whitespace-free: robust against cell-styled output."""
    return re.sub(r"\s+", "", strip_ansi(b))


def drive(binary: str, config_dir: str, data_dir: str):
    env = dict(os.environ)
    env["AGENT_MAIL_CONFIG_DIR"] = config_dir
    env["AGENT_MAIL_DATA_DIR"] = data_dir
    env["RUST_LOG"] = "off"
    env["TERM"] = "xterm-256color"

    pid, fd = pty.fork()
    if pid == 0:  # child
        os.execvpe(binary, [binary, "tui"], env)
    # give the PTY a real size (parent may not have a tty)
    winsz = struct.pack("HHHH", 30, 100, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, winsz)
    return pid, fd


def read_available(fd, timeout=1.0) -> bytes:
    out = b""
    end = time.time() + timeout
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                out += os.read(fd, 65536)
            except OSError:
                break
    return out


def send_keys(fd, s: str):
    os.write(fd, s.encode())


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/agent-mail"
    base = "/tmp/am-tui-test"

    pid, fd = drive(binary, f"{base}/b/cfg", f"{base}/b/data")
    try:
        initial = read_available(fd, 3.0)
        assert "threads" in squashed(initial), f"TUI did not render threads pane:\n{initial[:2000]!r}"
        print("1. TUI rendered threads pane")

        send_keys(fd, "?")
        help_out = read_available(fd, 1.0)
        assert "agent-mailTUIkeys" in squashed(help_out), "help did not open"
        print("2. help modal opens")
        send_keys(fd, "\x1b")  # Esc
        read_available(fd, 0.5)

        send_keys(fd, "a")
        allow_out = read_available(fd, 1.0)
        assert "allowedpeers" in squashed(allow_out), "allowlist did not open"
        print("3. allowlist modal opens (with rejected-attempts section)")
        send_keys(fd, "\x1b")
        read_available(fd, 0.5)

        # compose: c, type peer (allowlist name), Tab to body, type msg, Ctrl-S
        send_keys(fd, "c")
        time.sleep(0.5)
        read_available(fd, 0.3)
        send_keys(fd, "agent-a")
        send_keys(fd, "\t")
        send_keys(fd, "hello from the TUI!")
        time.sleep(0.3)
        send_keys(fd, "\x13")  # Ctrl-S
        send_keys(fd, "\x1b")  # close compose if still open
        print("4. compose sent (Ctrl-S); verifying delivery at peer...")
        # authoritative check: poll A's inbox for the message (delivery may
        # take a few seconds over the network)
        env = {"AGENT_MAIL_CONFIG_DIR": f"{base}/a/cfg",
               "AGENT_MAIL_DATA_DIR": f"{base}/a/data", "PATH": os.environ["PATH"]}
        deadline = time.time() + 45
        ok = False
        while time.time() < deadline:
            time.sleep(3)
            r = subprocess.run(
                [binary, "inbox", "--json"], capture_output=True, text=True, env=env,
            )
            out = r.stdout
            if "hello from the TUI!" in out:
                ok = True
                break
        assert ok, "message sent from TUI was not delivered within 45s"
        print("5. message delivered to peer inbox")
        read_available(fd, 0.5)

        send_keys(fd, "q")
        time.sleep(0.5)
        print("6. quit")
    finally:
        try:
            os.close(fd)
        except OSError:
            pass
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            pass


if __name__ == "__main__":
    main()
