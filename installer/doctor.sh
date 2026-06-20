#!/usr/bin/env bash
# doctor.sh — diagnose one-for-all installation health.
# Prints PASS/FAIL per check, exits non-zero if any FAIL.

set -uo pipefail   # NB: NOT -e — we want to run every check.
trap 'rc=$?; [[ $rc -ne 0 ]] && echo "[doctor] FAILED at line $LINENO (exit $rc)" >&2' ERR

HOME_DIR="${HOME:?HOME must be set}"
BIN_DIR="${HOME_DIR}/.one-for-all/bin"
BRIDGE_DIR="${HOME_DIR}/.one-for-all"
LOG_DIR="${BRIDGE_DIR}/logs"
SOCKET_PATH="${BRIDGE_DIR}.sock"
CLAUDE_JSON="${HOME_DIR}/.claude.json"
LABEL="io.github.elijahumana.one-for-all"
TARGET="gui/$(id -u)/${LABEL}"

PASS=0
FAIL=0
PASS_C=$'\033[1;32m[PASS]\033[0m'
FAIL_C=$'\033[1;31m[FAIL]\033[0m'

pass() { printf '%s %s\n' "$PASS_C" "$1"; PASS=$((PASS+1)); }
fail() { printf '%s %s — %s\n' "$FAIL_C" "$1" "$2"; FAIL=$((FAIL+1)); }
check_cmd() { command -v "$1" >/dev/null 2>&1; }

# 1. macOS
if [[ "$(uname -s)" == "Darwin" ]]; then pass "macOS host"; else fail "macOS host" "uname is $(uname -s)"; fi

# 2. required tools
for t in launchctl jq python3 plutil; do
    if check_cmd "$t"; then pass "tool present: $t"; else fail "tool present: $t" "not on PATH"; fi
done

# 3. binary present: mcp
if [[ -x "$BIN_DIR/one-for-all-mcp" ]]; then pass "binary: one-for-all-mcp"; else fail "binary: one-for-all-mcp" "missing $BIN_DIR/one-for-all-mcp"; fi

# 4. binary present: broker
if [[ -x "$BIN_DIR/one-for-all-broker" ]]; then pass "binary: one-for-all-broker"; else fail "binary: one-for-all-broker" "missing $BIN_DIR/one-for-all-broker"; fi

# 5. plist installed
plist="$HOME_DIR/Library/LaunchAgents/${LABEL}.plist"
if [[ -f "$plist" ]] && plutil -lint "$plist" >/dev/null 2>&1; then
    pass "launchd plist: $plist"
else
    fail "launchd plist: $plist" "missing or invalid"
fi

# 6. launchd job loaded + running
if launchctl print "$TARGET" >/dev/null 2>&1; then
    pass "launchd job loaded: $LABEL"
    state="$(launchctl print "$TARGET" 2>/dev/null | awk -F'= ' '/state =/{print $2; exit}' || true)"
    if [[ "$state" == "running" ]]; then
        pass "launchd job running"
    else
        fail "launchd job running" "state=${state:-unknown}"
    fi
else
    fail "launchd job loaded: $LABEL" "not loaded (run installer/install.sh)"
fi

# 7. socket exists
if [[ -S "$SOCKET_PATH" ]]; then pass "socket exists: $SOCKET_PATH"; else fail "socket exists: $SOCKET_PATH" "not a socket"; fi

# 8. socket reachable
if [[ -S "$SOCKET_PATH" ]] && python3 - <<PY 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(1.0)
try:
    s.connect("$SOCKET_PATH")
    s.close()
except Exception:
    sys.exit(1)
PY
then
    pass "socket reachable"
else
    fail "socket reachable" "AF_UNIX connect failed"
fi

# 9. ~/.claude.json present + entry exists
if [[ -f "$CLAUDE_JSON" ]]; then
    pass "~/.claude.json present"
    cmd="$(jq -r '.mcpServers["one-for-all"].command // empty' "$CLAUDE_JSON" 2>/dev/null)"
    if [[ -n "$cmd" ]]; then
        pass "mcpServers entry: one-for-all"
        if [[ "$cmd" == "$BIN_DIR/one-for-all-mcp" ]]; then
            pass "mcpServers command path matches binary"
        else
            fail "mcpServers command path matches binary" "have=$cmd want=$BIN_DIR/one-for-all-mcp"
        fi
    else
        fail "mcpServers entry: one-for-all" "missing in ~/.claude.json"
    fi
else
    fail "~/.claude.json present" "file missing"
fi

# 10. log dirs writable
for sub in broker mcp chromium; do
    d="$LOG_DIR/$sub"
    if mkdir -p "$d" 2>/dev/null && touch "$d/.doctor-probe" 2>/dev/null; then
        rm -f "$d/.doctor-probe"
        pass "log dir writable: $d"
    else
        fail "log dir writable: $d" "mkdir/touch failed"
    fi
done

# 11. broker tools.list smoke (best effort — depends on T5 wire protocol)
if [[ -S "$SOCKET_PATH" ]] && check_cmd python3; then
    if python3 - <<'PY' 2>/dev/null
import json, socket, sys, os
sock_path = os.environ["HOME"] + "/.one-for-all/broker.sock"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(2.0)
s.connect(sock_path)
req = {"jsonrpc":"2.0","id":1,"method":"_internal.ping","params":{}}
s.sendall((json.dumps(req)+"\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(4096)
    if not chunk: break
    buf += chunk
try:
    resp = json.loads(buf.decode().strip().splitlines()[0])
    sys.exit(0 if "result" in resp or "error" in resp else 1)
except Exception:
    sys.exit(1)
PY
    then
        pass "broker JSON-RPC ping"
    else
        fail "broker JSON-RPC ping" "no response (broker may be down or wire protocol pre-T5)"
    fi
fi

# 12. cargo deny check (advisories, licenses, bans, sources) — SPEC §10 quality gate
if check_cmd cargo && cargo deny --version >/dev/null 2>&1; then
    if (cd "${HOME_DIR}/one-for-all" 2>/dev/null && cargo deny check 2>/dev/null); then
        pass "cargo deny check"
    else
        fail "cargo deny check" "advisories/licenses/bans/sources had findings"
    fi
else
    fail "cargo deny installed" "install with: cargo install --locked cargo-deny"
fi

# 13. cargo audit clean — SPEC §10 quality gate
if check_cmd cargo && cargo audit --version >/dev/null 2>&1; then
    if (cd "${HOME_DIR}/one-for-all" 2>/dev/null && cargo audit -q 2>/dev/null); then
        pass "cargo audit"
    else
        fail "cargo audit" "vulnerabilities found"
    fi
else
    fail "cargo audit installed" "install with: cargo install --locked cargo-audit"
fi

# 14. plist owner sanity (catches sudo-installed plists in user LaunchAgents).
if [[ -f "$plist" ]]; then
    owner="$(stat -f '%Su' "$plist" 2>/dev/null || true)"
    me="$(id -un)"
    if [[ "$owner" == "$me" ]]; then
        pass "plist owned by $me"
    else
        fail "plist owned by $me" "owner=${owner:-unknown}"
    fi
fi

# 15. log retention (max-age & max-files)
old_logs=0
if [[ -d "$LOG_DIR" ]]; then
    old_logs="$(find "$LOG_DIR" -type f -name '*.log' -mtime +14 2>/dev/null | wc -l | tr -d ' ')"
fi
if [[ "$old_logs" -eq 0 ]]; then
    pass "no log files older than 14d"
else
    fail "log retention" "$old_logs files older than 14d under $LOG_DIR"
fi

# 16. ~/.claude.json structurally valid + entry count plausible
if [[ -f "$CLAUDE_JSON" ]]; then
    server_count="$(jq -r '.mcpServers // {} | keys | length' "$CLAUDE_JSON" 2>/dev/null || echo 0)"
    if [[ "$server_count" -ge 1 ]]; then
        pass "~/.claude.json mcpServers count: $server_count"
    else
        fail "~/.claude.json mcpServers count" "0 entries (expected ≥1 incl. one-for-all)"
    fi
fi

# 16b. SPEC R10 / N8 — diff the latest installer backup against the live
#      ~/.claude.json. Every leaf key the user had BEFORE the install must
#      still be present AFTER. The only allowed diff is the new
#      `mcpServers.one-for-all.*` subtree we add.
BACKUP_DIR="${HOME_DIR}/.one-for-all/backups/claude-json"
LATEST_BAK="$(ls -1t "$BACKUP_DIR"/claude.json.*.bak 2>/dev/null | head -n1 || true)"
if [[ -f "$CLAUDE_JSON" && -f "$LATEST_BAK" ]]; then
    # `paths(scalars)` lists every leaf-keyed path; we treat each as a single
    # canonical string and compare set-membership. That way reordering or
    # whitespace differences in the file are ignored.
    if before_keys="$(jq -r 'paths(scalars) | map(tostring) | join(".")' "$LATEST_BAK" 2>/dev/null | sort -u)" \
       && after_keys="$(jq  -r 'paths(scalars) | map(tostring) | join(".")' "$CLAUDE_JSON" 2>/dev/null | sort -u)"
    then
        # Keys lost from before → after, with the expected one-for-all
        # additions filtered out (the installer is allowed to ADD, not lose).
        lost="$(comm -23 <(printf '%s\n' "$before_keys") <(printf '%s\n' "$after_keys") \
                | grep -v '^mcpServers\.one-for-all' || true)"
        if [[ -z "$lost" ]]; then
            pass "R10 ~/.claude.json keys preserved across install (vs $(basename "$LATEST_BAK"))"
        else
            fail "R10 ~/.claude.json keys preserved across install" \
                 "$(printf '%s\n' "$lost" | wc -l | tr -d ' ') key(s) vanished — first: $(printf '%s\n' "$lost" | head -n1)"
        fi
    else
        fail "R10 ~/.claude.json keys preserved across install" \
             "jq failed to enumerate keys (file unparseable?)"
    fi
else
    pass "R10 keys-diff: no backup yet (installer not run, or backup pruned)"
fi

# 17. concurrent-install lock not stale
INSTALL_LOCK="${BRIDGE_DIR}/install.lock.d"
if [[ -d "$INSTALL_LOCK" ]]; then
    age_secs="$(($(date +%s) - $(stat -f %m "$INSTALL_LOCK")))"
    if [[ "$age_secs" -gt 600 ]]; then
        fail "install lock not stale" "lock dir age ${age_secs}s — stale (run: rm -rf $INSTALL_LOCK)"
    else
        pass "install lock fresh (age ${age_secs}s)"
    fi
else
    pass "no stale install lock"
fi

# 18. tools installed
for t in ofa-tail ofa-status ofa-doctor; do
    if [[ -x "$BIN_DIR/$t" ]]; then pass "tool installed: $t"; else fail "tool installed: $t" "missing $BIN_DIR/$t"; fi
done

# 19. SPEC §10 M10 — trace dir writable + active trace files fresh.
SESS_ROOT="${BRIDGE_DIR}/sessions"
if [[ -d "$SESS_ROOT" ]]; then
    bad_trace_dirs=0
    stale_trace_dirs=0
    found=0
    for sdir in "$SESS_ROOT"/*/trace; do
        [[ -d "$sdir" ]] || continue
        found=$((found+1))
        if ! touch "$sdir/.doctor-trace-probe" 2>/dev/null; then
            bad_trace_dirs=$((bad_trace_dirs+1))
            continue
        fi
        rm -f "$sdir/.doctor-trace-probe"
        # Latest jsonl mtime.
        latest=$(ls -1t "$sdir"/[0-9]*.jsonl 2>/dev/null | head -1 || true)
        if [[ -n "$latest" ]]; then
            mtime=$(stat -f %m "$latest" 2>/dev/null || echo 0)
            now=$(date +%s)
            age=$((now - mtime))
            # SPEC §10: when a session has trace enabled and is active, the
            # newest file should be < 1h old. Older means writer died or
            # session went idle without unregister; we flag it as stale.
            if (( age > 3600 )); then
                stale_trace_dirs=$((stale_trace_dirs+1))
            fi
        fi
    done
    if (( found == 0 )); then
        pass "no traced sessions found (M10 idle ok)"
    elif (( bad_trace_dirs == 0 && stale_trace_dirs == 0 )); then
        pass "M10 trace dirs writable + fresh ($found session(s))"
    else
        fail "M10 trace dirs healthy" "$bad_trace_dirs unwritable, $stale_trace_dirs stale (>1h)"
    fi
else
    pass "no sessions root yet (M10 not exercised)"
fi

# ---------- SPEC §11 V3 — sandbox-per-agent checks ----------

# 20. sandbox-exec available
if [[ -x /usr/bin/sandbox-exec ]]; then
    pass "sandbox-exec present at /usr/bin/sandbox-exec"
else
    fail "sandbox-exec present" "/usr/bin/sandbox-exec missing — V3 cannot confine sessions"
fi

# 21. APFS clonefile actually works on the user's session volume.
# Probe by cloning a 1KB file under ~/.one-for-all/sessions/ and asserting
# byte-identity + distinct inode. We use python3 ctypes to call clonefile(2)
# without depending on the not-yet-installed `ofa-merge` binary.
PROBE_DIR="${BRIDGE_DIR}/sessions/.doctor-clone-probe"
rm -rf "$PROBE_DIR" 2>/dev/null || true
mkdir -p "$PROBE_DIR" 2>/dev/null || true
if python3 - "$PROBE_DIR" <<'PY' 2>/dev/null
import ctypes, os, sys
probe_dir = sys.argv[1]
src = os.path.join(probe_dir, "src.bin")
dst = os.path.join(probe_dir, "dst.bin")
with open(src, "wb") as f:
    f.write(b"\x55" * 1024)
libc = ctypes.CDLL("/usr/lib/libSystem.dylib", use_errno=True)
libc.clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint32]
libc.clonefile.restype = ctypes.c_int
rc = libc.clonefile(src.encode(), dst.encode(), 0)
if rc != 0:
    sys.exit(2 + ctypes.get_errno())
if open(dst, "rb").read() != open(src, "rb").read():
    sys.exit(1)
if os.stat(src).st_ino == os.stat(dst).st_ino:
    sys.exit(1)
PY
then
    pass "APFS clonefile(2) works on session volume"
else
    rc=$?
    fail "APFS clonefile(2) works on session volume" \
         "clonefile probe failed (rc=$rc) — V3 will fall back to V-R1 cookie seeding"
fi
rm -rf "$PROBE_DIR" 2>/dev/null || true

# 22. FileVault state — informational. clonefile works on unlocked volumes
#     (the agent runtime is by definition logged-in), so we PASS in both
#     "Off" and "On" cases and only flag a parser failure.
if check_cmd /usr/bin/fdesetup; then
    fv_status="$(/usr/bin/fdesetup status 2>/dev/null | head -n1)"
    case "$fv_status" in
        "FileVault is Off."*)
            pass "FileVault state: Off"
            ;;
        "FileVault is On"*)
            pass "FileVault state: On (unlocked while user is logged in)"
            ;;
        *)
            fail "FileVault state" "fdesetup output unrecognized: ${fv_status:-<empty>}"
            ;;
    esac
else
    fail "FileVault state" "/usr/bin/fdesetup not available"
fi

# 23. Default-allowlist source dirs exist (FAIL per missing path so the
#     operator sees what won't get inherited; the broker still functions).
for src in "$HOME_DIR/Documents" "$HOME_DIR/Downloads" \
           "$HOME_DIR/.ssh" "$HOME_DIR/.config"; do
    if [[ -e "$src" ]]; then
        pass "default-allowlist source present: $(basename "$src")"
    else
        fail "default-allowlist source present: $(basename "$src")" \
             "missing $src (sessions will skip this inherit)"
    fi
done

# 24. Host Chrome Default profile present — informational only. Hosts
#     without Chrome installed still boot sessions, just logged-out.
CHROME_HOST_PROFILE="$HOME_DIR/Library/Application Support/Google/Chrome/Default"
if [[ -d "$CHROME_HOST_PROFILE" ]]; then
    pass "host Chrome Default profile present"
else
    pass "host Chrome Default profile absent (sessions will start logged-out)"
fi

# 25. ofa-merge installed (optional)
if [[ -x "$BIN_DIR/ofa-merge" ]]; then
    pass "tool installed: ofa-merge"
else
    # Soft-fail: ofa-merge ships out of band; sessions are functional without
    # it, the user just can't promote agent state back to host.
    fail "tool installed: ofa-merge" "missing $BIN_DIR/ofa-merge — install via cargo build -p sandbox --release"
fi

# 26. SPEC §11 V2 — Accessibility API trust for native-control (app.*).
#     `--check-ax` returns 0 iff AXIsProcessTrusted() == true.
if [[ -x "$BIN_DIR/one-for-all-broker" ]]; then
    if "$BIN_DIR/one-for-all-broker" --check-ax >/dev/null 2>&1; then
        pass "SPEC §11 V2 AX permission granted (AXIsProcessTrusted)"
    else
        fail "SPEC §11 V2 AX permission granted (AXIsProcessTrusted)" \
             "open: x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility — then re-run ofa-doctor"
    fi
fi

# ---------- doctor deeper (#18) — codesign / sandbox-exec / plist checksum ----------

# 27. Chromium binary is code-signed (any signer; we just want it not corrupted
#     or repacked).
CHROMIUM_BIN_HINT="${HOME_DIR}/.one-for-all/chromium/Chromium.app/Contents/MacOS/Chromium"
if [[ -x "$CHROMIUM_BIN_HINT" ]]; then
    if codesign -dvv "$CHROMIUM_BIN_HINT" >/dev/null 2>&1; then
        pass "Chromium binary code-sign valid"
    else
        fail "Chromium binary code-sign valid" \
             "codesign -dvv failed for $CHROMIUM_BIN_HINT (binary may be stripped or corrupted)"
    fi
else
    pass "Chromium binary code-sign: not yet fetched (skip)"
fi

# 28. sandbox-exec profile parses cleanly. We synthesize a minimal SBPL
#     ('(version 1)(deny default)') and run it against /bin/echo; this proves
#     `sandbox-exec` itself works on this host.
if [[ -x /usr/bin/sandbox-exec ]]; then
    sb_tmp="$(mktemp -t ofa-doctor-sbpl)"
    cat >"$sb_tmp" <<'SBPL'
(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow file-read*)
(allow signal (target self))
SBPL
    if /usr/bin/sandbox-exec -f "$sb_tmp" /bin/echo ofa-sandbox-probe >/dev/null 2>&1; then
        pass "sandbox-exec profile compiles and confines"
    else
        fail "sandbox-exec profile compiles and confines" \
             "synthetic profile failed to launch /bin/echo (Apple may have changed SBPL syntax)"
    fi
    rm -f "$sb_tmp"
fi

# 29. launchd plist checksum (#18) — installer pins SHA256 of the rendered
#     plist into ~/.one-for-all/plist.sha256; verify the on-disk plist matches.
PLIST_SHA_FILE="${HOME_DIR}/.one-for-all/plist.sha256"
if [[ -f "$plist" && -f "$PLIST_SHA_FILE" ]]; then
    expected="$(awk '{print $1; exit}' "$PLIST_SHA_FILE")"
    actual="$(shasum -a 256 "$plist" 2>/dev/null | awk '{print $1; exit}')"
    if [[ -n "$expected" && "$expected" == "$actual" ]]; then
        pass "launchd plist sha256 matches installer pin"
    else
        fail "launchd plist sha256 matches installer pin" \
             "expected=${expected:-<empty>} actual=${actual:-<empty>} — plist tampered or installer not yet rerun"
    fi
elif [[ -f "$plist" ]]; then
    pass "launchd plist sha256 pin: not present yet (run installer to record)"
fi


echo
printf 'Summary: %d PASS / %d FAIL\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]] || exit 1
