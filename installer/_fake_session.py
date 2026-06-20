#!/usr/bin/env python3
"""Headless test client speaking SPEC §2 line-delim JSON-RPC to the broker.

Used by e2e-smoke.sh and ad-hoc debugging. Performs `session.register` on
connect (per SPEC §2 handshake) and exposes a tiny call-by-method API.
"""
from __future__ import annotations

import argparse
import json
import os
import socket
import sys
import threading
import uuid


class FakeSession:
    def __init__(self, sock_path: str, name: str):
        self.sock_path = sock_path
        self.name = name
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(20.0)
        self.s.connect(sock_path)
        self._buf = b""
        self._nid = 1
        self._lock = threading.Lock()
        self.session_id = None

    def _readline(self) -> str:
        while b"\n" not in self._buf:
            chunk = self.s.recv(65536)
            if not chunk:
                return ""
            self._buf += chunk
        line, _, self._buf = self._buf.partition(b"\n")
        return line.decode()

    def call(self, method: str, params: dict) -> dict:
        with self._lock:
            rid = self._nid
            self._nid += 1
            req = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
            self.s.sendall((json.dumps(req) + "\n").encode())
            while True:
                line = self._readline()
                if not line:
                    raise RuntimeError(f"broker closed during {method}")
                m = json.loads(line)
                if m.get("id") == rid:
                    if m.get("error"):
                        raise RuntimeError(f"{method} error: {m['error']}")
                    return m.get("result") or {}

    def register(self) -> dict:
        r = self.call(
            "session.register",
            {
                "client_name": self.name,
                "client_version": "0.1.0",
                "capabilities": ["tools", "events"],
            },
        )
        self.session_id = r.get("session_id")
        return r

    def close(self) -> None:
        try:
            self.call("session.unregister", {})
        except Exception:
            pass
        try:
            self.s.close()
        except Exception:
            pass


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--sock", default=os.path.expanduser("~/.one-for-all/broker.sock"))
    p.add_argument("--name", default=f"ofa-fake/{uuid.uuid4().hex[:8]}")
    p.add_argument("--script", required=True, help="JSON file: list of {method, params}")
    p.add_argument("--out", required=True)
    args = p.parse_args()

    with open(args.script) as f:
        steps = json.load(f)

    s = FakeSession(args.sock, args.name)
    s.register()
    results = []
    for i, step in enumerate(steps):
        try:
            r = s.call(step["method"], step.get("params", {}))
            results.append({"i": i, "method": step["method"], "ok": True, "result": r})
        except Exception as e:
            results.append({"i": i, "method": step["method"], "ok": False, "error": str(e)})
    with open(args.out, "w") as f:
        json.dump(results, f, indent=2)
    s.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
