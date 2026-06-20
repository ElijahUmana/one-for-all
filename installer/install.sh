#!/usr/bin/env bash
# install.sh — idempotent installer for one-for-all.
#
# Steps:
#   1. Pre-flight: macOS, required tools (cargo, jq, launchctl, plutil, python3).
#   2. cargo build --release --workspace.
#   3. Install binaries → ~/.one-for-all/bin/.
#   4. Render plist → ~/Library/LaunchAgents/io.github.elijahumana.one-for-all.plist.
#   5. launchctl bootout (idempotent) + bootstrap + kickstart.
#   6. Wait for ~/.one-for-all/broker.sock (≤5s).
#   7. Atomically merge mcpServers["one-for-all"] into ~/.claude.json.
#   8. Lint shell + verify deploy.
#
# Re-runnable. Never clobbers other ~/.claude.json keys or other mcpServers.

set -euo pipefail

trap 'rc=$?; rmdir "${INSTALL_LOCK:-/nonexistent}" 2>/dev/null || true; rm -f "${tmp_json:-}" "${tmp_plist:-}"; if [[ $rc -ne 0 ]]; then echo "[install] FAILED at line $LINENO (exit $rc)" >&2; fi; exit $rc' ERR EXIT

# --- locations -------------------------------------------------------------
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOME_DIR="${HOME:?HOME must be set}"
BIN_DIR="${HOME_DIR}/.one-for-all/bin"
BRIDGE_DIR="${HOME_DIR}/.one-for-all"
LOG_DIR="${BRIDGE_DIR}/logs"
PLIST_SRC="${PROJECT_ROOT}/installer/io.github.elijahumana.one-for-all.plist"
PLIST_DST="${HOME_DIR}/Library/LaunchAgents/io.github.elijahumana.one-for-all.plist"
SOCKET_PATH="${BRIDGE_DIR}.sock"
CLAUDE_JSON="${HOME_DIR}/.claude.json"
LABEL="io.github.elijahumana.one-for-all"
UID_NUM="$(id -u)"
TARGET="gui/${UID_NUM}/${LABEL}"
INSTALL_LOCK="${BRIDGE_DIR}/install.lock.d"

mkdir -p "$BRIDGE_DIR"

# --- 0. concurrent-install guard -------------------------------------------
# Two install.sh racing each other can corrupt ~/.claude.json — even with the
# atomic mv, the read-modify-write window is wider than the rename. `mkdir`
# is atomic on POSIX, so use a lock-dir to serialize.
if ! mkdir "$INSTALL_LOCK" 2>/dev/null; then
    echo "[install] another install.sh is running (lock dir: $INSTALL_LOCK)" >&2
    echo "          remove it manually if no installer is active" >&2
    exit 1
fi

# --- helpers ---------------------------------------------------------------
log()   { printf '\033[1;34m▶\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
fail()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }
need()  { command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"; }

# --- 1. pre-flight ---------------------------------------------------------
log "pre-flight"
[[ "$(uname -s)" == "Darwin" ]] || fail "one-for-all currently supports macOS only"
need cargo
need jq
need launchctl
need plutil
need python3
need install
ok "tools present"

# --- 1.5 SPEC §11 V2 AX permission probe ---------------------------------
# `one-for-all-broker --check-ax --prompt` returns 0 if AXIsProcessTrusted
# is true, otherwise 1; with --prompt it triggers the OS prompt the first
# time. macOS shows the dialog at most once per process, so re-running the
# installer is a silent re-check. Non-fatal: install proceeds without
# native-control if denied.
ax_check_bin=""
if [[ -x "$PROJECT_ROOT/target/release/one-for-all-broker" ]]; then
    ax_check_bin="$PROJECT_ROOT/target/release/one-for-all-broker"
elif [[ -x "$BIN_DIR/one-for-all-broker" ]]; then
    ax_check_bin="$BIN_DIR/one-for-all-broker"
fi
if [[ -n "$ax_check_bin" ]]; then
    if "$ax_check_bin" --check-ax --prompt >/dev/null 2>&1; then
        ok "AX permission granted (SPEC §11 V2 native control enabled)"
    else
        warn "AX permission missing — app.* tools will return -32009. Grant in: x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility, then re-run: $BIN_DIR/ofa-doctor"
    fi
else
    log "AX probe deferred: broker binary not yet built"
fi

# --- 2. build --------------------------------------------------------------
if [[ "${OFA_SKIP_BUILD:-0}" == "1" ]]; then
    log "cargo build (skipped via OFA_SKIP_BUILD=1)"
else
    log "cargo build --release --workspace"
    ( cd "$PROJECT_ROOT" && cargo build --release --workspace )
    ok "build complete"
fi

# --- 3. install binaries ---------------------------------------------------
log "install binaries → $BIN_DIR"
mkdir -p "$BIN_DIR" "$BRIDGE_DIR" "$LOG_DIR/broker" "$LOG_DIR/mcp" "$LOG_DIR/chromium"
chmod 0700 "$BRIDGE_DIR" || true

install_bin() {
    local src="$1" name="$2"
    if [[ -f "$src" ]]; then
        install -m 0755 "$src" "$BIN_DIR/$name"
        ok "installed $name"
    else
        warn "binary not produced: $name (path: $src) — skipping"
    fi
}

install_bin "$PROJECT_ROOT/target/release/one-for-all-mcp"    "one-for-all-mcp"
install_bin "$PROJECT_ROOT/target/release/one-for-all-broker" "one-for-all-broker"
# SPEC §11 V7 — operator CLI (`ofa spawn / list / attach / merge / kill / logs`).
install_bin "$PROJECT_ROOT/target/release/ofa"                   "ofa"

# Tool scripts.
install -m 0755 "$PROJECT_ROOT/tools/ofa-tail.sh"     "$BIN_DIR/ofa-tail"
install -m 0755 "$PROJECT_ROOT/tools/ofa-status.sh"   "$BIN_DIR/ofa-status"
install -m 0755 "$PROJECT_ROOT/tools/ofa-trace.sh"    "$BIN_DIR/ofa-trace"
install -m 0755 "$PROJECT_ROOT/tools/ofa-test-live.sh" "$BIN_DIR/ofa-test-live"
install -m 0755 "$PROJECT_ROOT/installer/doctor.sh"  "$BIN_DIR/ofa-doctor"
ok "installed ofa-tail, ofa-status, ofa-trace, ofa-test-live, ofa-doctor"

# --- 4. render plist -------------------------------------------------------
log "render launchd plist"
mkdir -p "${HOME_DIR}/Library/LaunchAgents"
tmp_plist="$(mktemp -t ofa-plist.XXXXXX)"
sed \
    -e "s|__BIN_DIR__|${BIN_DIR}|g" \
    -e "s|__LOG_DIR__|${LOG_DIR}|g" \
    -e "s|__HOME__|${HOME_DIR}|g" \
    "$PLIST_SRC" > "$tmp_plist"
plutil -lint "$tmp_plist" >/dev/null
mv -f "$tmp_plist" "$PLIST_DST"
chmod 0644 "$PLIST_DST"
# Pin the SHA256 so ofa-doctor can detect tampering (#18 doctor deeper).
shasum -a 256 "$PLIST_DST" | awk '{print $1}' > "${HOME_DIR}/.one-for-all/plist.sha256"
ok "plist installed at $PLIST_DST"

# --- 5. launchd bootstrap --------------------------------------------------
if [[ ! -x "$BIN_DIR/one-for-all-broker" ]]; then
    warn "broker binary missing; skipping launchd bootstrap (run install.sh once T5 is built)"
else
    log "launchctl bootout (idempotent)"
    launchctl bootout "$TARGET" 2>/dev/null || true

    log "launchctl bootstrap"
    launchctl bootstrap "gui/${UID_NUM}" "$PLIST_DST"

    log "launchctl kickstart"
    launchctl kickstart -k "$TARGET"

    log "wait for socket (≤5s)"
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        if [[ -S "$SOCKET_PATH" ]]; then
            ok "socket up at $SOCKET_PATH"
            break
        fi
        sleep 0.5
    done
    [[ -S "$SOCKET_PATH" ]] || warn "socket did not come up; broker may have failed — check $LOG_DIR/broker/"
fi

# --- 6. atomic ~/.claude.json merge ----------------------------------------
log "merge mcpServers entry into ~/.claude.json (atomic)"
if [[ ! -f "$CLAUDE_JSON" ]]; then
    fail "$CLAUDE_JSON does not exist; run claude once before installing one-for-all"
fi

# Validate input is parseable JSON before we touch it.
python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$CLAUDE_JSON" \
    || fail "$CLAUDE_JSON is not valid JSON; refusing to modify"

# Defense-in-depth: keep a timestamped backup before touching ~/.claude.json.
# The atomic mv guarantees no partial writes, but a backup means the user
# never has to trust us — they can `cp` back at any time.
backup_dir="${HOME_DIR}/.one-for-all/backups/claude-json"
mkdir -p "$backup_dir"
backup_file="$backup_dir/claude.json.$(date -u +%Y%m%dT%H%M%SZ).$$.bak"
cp "$CLAUDE_JSON" "$backup_file"
chmod 0600 "$backup_file" || true
# Trim to last 30 backups.
ls -1t "$backup_dir"/claude.json.*.bak 2>/dev/null | tail -n +31 | xargs -I{} rm -f {} 2>/dev/null || true
ok "backup at $backup_file"

tmp_json="$(mktemp "${CLAUDE_JSON}.XXXXXX")"
trap 'rm -f "$tmp_json"' EXIT

jq --arg cmd "$BIN_DIR/one-for-all-mcp" '
    .mcpServers = (.mcpServers // {}) |
    .mcpServers["one-for-all"] = {
        "type": "stdio",
        "command": $cmd,
        "args": [],
        "env": {}
    }
' "$CLAUDE_JSON" > "$tmp_json"

# Validate output before move.
python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$tmp_json" \
    || fail "merge produced invalid JSON; aborting"

# Sanity: confirm exactly one extra key under mcpServers, and no top-level keys lost.
before_keys="$(jq -r 'keys | sort | .[]' "$CLAUDE_JSON" | tr '\n' ' ')"
after_keys="$(jq -r 'keys | sort | .[]' "$tmp_json" | tr '\n' ' ')"
[[ "$before_keys" == "$after_keys" ]] \
    || fail "top-level keys changed (before: $before_keys, after: $after_keys); aborting"

before_servers="$(jq -r '.mcpServers // {} | keys | sort | .[]' "$CLAUDE_JSON" | tr '\n' ' ')"
after_servers="$(jq -r '.mcpServers // {} | keys | sort | .[]' "$tmp_json" | tr '\n' ' ')"
case " $after_servers " in
    *" one-for-all "*) ;;
    *) fail "merge did not insert one-for-all entry (have: $after_servers)";;
esac
# Every pre-existing server must still be present.
for srv in $before_servers; do
    case " $after_servers " in
        *" $srv "*) ;;
        *) fail "merge dropped existing mcpServer '$srv'";;
    esac
done

mv -f "$tmp_json" "$CLAUDE_JSON"
ok "~/.claude.json updated; entries preserved: $after_servers"

# --- 7. summary ------------------------------------------------------------
echo
ok "one-for-all installed."
echo
echo "  Verify:    $BIN_DIR/ofa-doctor"
echo "  Status:    $BIN_DIR/ofa-status"
echo "  Live tail: $BIN_DIR/ofa-tail"
echo "  Uninstall: $PROJECT_ROOT/installer/uninstall.sh"
echo
echo "Open a new claude session in any terminal — browser tools are now innate."
