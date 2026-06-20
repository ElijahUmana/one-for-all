#!/usr/bin/env bash
# ofa-status — one-shot health snapshot of the one-for-all stack.
# Pass --json for machine-readable output.

set -euo pipefail

JSON_MODE=0
for arg in "$@"; do
    case "$arg" in
        --json) JSON_MODE=1 ;;
        -h|--help) sed -n '2,5p' "$0"; exit 0 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

HOME_DIR="${HOME:?HOME must be set}"
BRIDGE_DIR="${HOME_DIR}/.one-for-all"
SOCKET_PATH="${BRIDGE_DIR}.sock"
LABEL="io.github.elijahumana.one-for-all"
TARGET="gui/$(id -u)/${LABEL}"

bold(){ printf '\033[1m%s\033[0m\n' "$*"; }
row() { printf '  %-12s %s\n' "$1" "$2"; }

# Collect everything first so --json can serialize it as one blob.
state="?"; pid=""; uptime="-"
if launchctl print "$TARGET" >/dev/null 2>&1; then
    state="$(launchctl print "$TARGET" 2>/dev/null | awk -F'= ' '/state =/{print $2; exit}')"
    pid="$(launchctl print "$TARGET" 2>/dev/null   | awk -F'= ' '/pid =/{print $2; exit}')"
fi
if [[ -n "$pid" ]] && [[ "$pid" =~ ^[0-9]+$ ]]; then
    uptime="$(ps -o etime= -p "$pid" 2>/dev/null | tr -d '[:space:]' || true)"
    [[ -z "$uptime" ]] && uptime="-"
fi

socket_present="false"
[[ -S "$SOCKET_PATH" ]] && socket_present="true"

broker_json=""
if [[ -S "$SOCKET_PATH" ]] && command -v python3 >/dev/null 2>&1; then
    broker_json="$(python3 - <<'PY' 2>/dev/null || true
import json, socket, os
p = os.environ["HOME"] + "/.one-for-all/broker.sock"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(1.5)
try:
    s.connect(p)
    req = {"jsonrpc":"2.0","id":1,"method":"_internal.status","params":{}}
    s.sendall((json.dumps(req)+"\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(4096)
        if not chunk: break
        buf += chunk
    line = buf.decode().strip().splitlines()[0]
    resp = json.loads(line)
    r = resp.get("result") or {}
    print(json.dumps({
        "sessions": r.get("sessions"),
        "contexts": r.get("contexts"),
        "tabs":     r.get("tabs"),
        "chromium_pid": r.get("chromium_pid"),
        "chromium_rss": r.get("chromium_rss"),
    }))
except Exception:
    pass
PY
)"
fi

if [[ "$JSON_MODE" -eq 1 ]]; then
    if [[ -z "$broker_json" ]]; then broker_json='null'; fi
    jq -n \
        --arg state "$state" --arg pid "$pid" --arg uptime "$uptime" \
        --arg socket "$SOCKET_PATH" --arg socket_present "$socket_present" \
        --argjson broker "$broker_json" '
        {
            broker: { pid: $pid, uptime: $uptime, state: $state },
            socket: { path: $socket, present: ($socket_present == "true") },
            stats: $broker
        }'
    exit 0
fi

bold "one-for-all status"
row "broker" "pid=${pid:-none} uptime=${uptime} state=${state:-unknown}"
if [[ "$socket_present" == "true" ]]; then
    row "socket" "$SOCKET_PATH (present)"
else
    row "socket" "$SOCKET_PATH (MISSING)"
fi
if [[ -n "$broker_json" ]]; then
    row "sessions"  "$(jq -r '.sessions // "-"' <<<"$broker_json")"
    row "contexts"  "$(jq -r '.contexts // "-"' <<<"$broker_json")"
    row "tabs"      "$(jq -r '.tabs // "-"'     <<<"$broker_json")"
    row "chromium"  "pid=$(jq -r '.chromium_pid // "-"' <<<"$broker_json") rss=$(jq -r '.chromium_rss // "-"' <<<"$broker_json")"
elif [[ "$socket_present" == "true" ]]; then
    row "broker rpc" "no response"
fi
