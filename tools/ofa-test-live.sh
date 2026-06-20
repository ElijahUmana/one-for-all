#!/usr/bin/env bash
# ofa-test-live.sh — exercise the live broker through the MCP server.
# Like e2e-smoke.sh but through the MCP boundary (LSP framing → mcp-server →
# broker). Used to verify the FULL stack a real `claude` session sees.

set -uo pipefail
trap 'rc=$?; [[ $rc -ne 0 ]] && echo "[ofa-test-live] line $LINENO rc=$rc" >&2' ERR

HOME_DIR="${HOME:?HOME must be set}"
BIN="${HOME_DIR}/.one-for-all/bin/one-for-all-mcp"
[[ -x "$BIN" ]] || { echo "missing $BIN; run installer/install.sh" >&2; exit 1; }

PASS=0; FAIL=0
PASS_C=$'\033[1;32m[PASS]\033[0m'
FAIL_C=$'\033[1;31m[FAIL]\033[0m'
pass(){ printf '%s %s\n' "$PASS_C" "$1"; PASS=$((PASS+1)); }
fail(){ printf '%s %s — %s\n' "$FAIL_C" "$1" "$2"; FAIL=$((FAIL+1)); }

python3 - "$BIN" <<'PY'
"""Drive one-for-all-mcp via LSP-framed JSON-RPC stdio."""
import json, os, subprocess, sys, threading, time

BIN = sys.argv[1]
proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

def send(method, params, rid):
    body = json.dumps({"jsonrpc":"2.0","id":rid,"method":method,"params":params}).encode()
    proc.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    proc.stdin.flush()

def recv():
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        b = proc.stdout.read(1)
        if not b: return None
        header += b
    length = 0
    for line in header.decode().split("\r\n"):
        if line.lower().startswith("content-length:"):
            length = int(line.split(":",1)[1].strip())
    body = proc.stdout.read(length)
    return json.loads(body)

results = []
def assertion(name, ok, detail=""):
    print(json.dumps({"name":name,"ok":ok,"detail":detail}))

try:
    send("initialize", {"protocolVersion":"2024-11-05","clientInfo":{"name":"ofa-test-live"}}, 1)
    r = recv()
    assertion("initialize returns serverInfo", bool(r and r.get("result",{}).get("serverInfo",{}).get("name")=="one-for-all"))

    send("tools/list", {}, 2)
    r = recv()
    tools = r.get("result",{}).get("tools",[]) if r else []
    assertion("tools/list returns ≥28 tools", len(tools) >= 28, f"got {len(tools)}")
    names = {t["name"] for t in tools}
    for needed in ("tab.open","page.snapshot","page.click","page.cookies",
                   "page.network_conditions","page.emulate"):
        assertion(f"tools/list includes {needed}", needed in names)

    # Bad-input path: tools/call with missing required field returns -32602.
    send("tools/call", {"name":"tab.open","arguments":{}}, 3)
    r = recv()
    err = r.get("error",{}) if r else {}
    assertion("tools/call validates input", err.get("code") == -32602, str(err))

finally:
    try: proc.stdin.close()
    except Exception: pass
    try: proc.wait(timeout=3)
    except subprocess.TimeoutExpired: proc.kill()
PY
