#!/usr/bin/env bash
# uninstall.sh — fully reverse install.sh. Default keeps logs (forensics).
# Use --purge to also remove binaries and logs.
#
# Never touches non-one-for-all keys in ~/.claude.json.

set -euo pipefail
trap 'rc=$?; echo "[uninstall] FAILED at line $LINENO (exit $rc)" >&2; exit $rc' ERR

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOME_DIR="${HOME:?HOME must be set}"
BIN_DIR="${HOME_DIR}/.one-for-all/bin"
BRIDGE_DIR="${HOME_DIR}/.one-for-all"
PLIST_DST="${HOME_DIR}/Library/LaunchAgents/io.github.elijahumana.one-for-all.plist"
SOCKET_PATH="${BRIDGE_DIR}.sock"
CLAUDE_JSON="${HOME_DIR}/.claude.json"
LABEL="io.github.elijahumana.one-for-all"
TARGET="gui/$(id -u)/${LABEL}"

PURGE=0
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=1 ;;
        -h|--help)
            sed -n '2,8p' "$0"; exit 0 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

log() { printf '\033[1;34m▶\033[0m %s\n' "$*"; }
ok()  { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn(){ printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }

log "stop launchd job"
launchctl bootout "$TARGET" 2>/dev/null || warn "job was not loaded"
ok "launchd job stopped"

log "remove plist"
rm -f "$PLIST_DST"
ok "plist removed"

log "remove socket (if dangling)"
rm -f "$SOCKET_PATH"
ok "socket cleared"

if [[ -f "$CLAUDE_JSON" ]]; then
    log "remove mcpServers[one-for-all] from ~/.claude.json (atomic)"
    tmp_json="$(mktemp "${CLAUDE_JSON}.XXXXXX")"
    trap 'rm -f "$tmp_json"' EXIT
    jq 'if .mcpServers? then .mcpServers |= del(."one-for-all") else . end' "$CLAUDE_JSON" > "$tmp_json"
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$tmp_json"
    # Other mcpServers must be unchanged.
    before="$(jq -r '.mcpServers // {} | del(."one-for-all") | keys | sort | .[]' "$CLAUDE_JSON" | tr '\n' ' ')"
    after="$(jq  -r '.mcpServers // {}                       | keys | sort | .[]' "$tmp_json"     | tr '\n' ' ')"
    [[ "$before" == "$after" ]] || { rm -f "$tmp_json"; echo "uninstall would drop other servers; aborting" >&2; exit 1; }
    mv -f "$tmp_json" "$CLAUDE_JSON"
    trap - EXIT
    ok "~/.claude.json updated"
fi

if [[ "$PURGE" -eq 1 ]]; then
    log "purge binaries and logs (--purge)"
    rm -f  "$BIN_DIR/one-for-all-mcp" "$BIN_DIR/one-for-all-broker"
    rm -f  "$BIN_DIR/ofa-tail" "$BIN_DIR/ofa-status" "$BIN_DIR/ofa-doctor"
    rm -rf "$BRIDGE_DIR/logs"
    ok "purge complete"
else
    warn "logs and binaries preserved; pass --purge to remove them"
fi

ok "one-for-all uninstalled"
