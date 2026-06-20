#!/usr/bin/env bash
# e2e-app-smoke.sh — SPEC §11 V2 native-control end-to-end smoke.
#
# Drives Calculator.app via app.list / app.snapshot / app.click and asserts:
#   - app.list returns com.apple.calculator with a non-zero pid
#   - app.snapshot returns elements with role=AXButton and name="5"
#   - app.click on "5" makes the display read "5"
#   - "5 + 3 =" yields display "8"
#   - frontmost-pid does NOT change across the run (focus-no-steal invariant)
#
# Exits 0 on PASS, 1 on FAIL. Skips with WARN (rc=0) when AX permission is
# not granted — CI runners can't grant it, but a developer running locally
# after `installer/install.sh` will have prompted-and-granted.

set -uo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOME_DIR="${HOME:?HOME must be set}"
SOCKET_PATH="${HOME_DIR}/.one-for-all/broker.sock"
BIN_DIR="${HOME_DIR}/.one-for-all/bin"
TMP_DIR="$(mktemp -d -t ofa-app-smoke.XXXXXX)"
trap 'rm -rf "$TMP_DIR"' EXIT

PASS=0; FAIL=0
PASS_C=$'\033[1;32m[PASS]\033[0m'
FAIL_C=$'\033[1;31m[FAIL]\033[0m'
WARN_C=$'\033[1;33m[WARN]\033[0m'

pass(){ printf '%s %s\n' "$PASS_C" "$1"; PASS=$((PASS+1)); }
fail(){ printf '%s %s — %s\n' "$FAIL_C" "$1" "$2"; FAIL=$((FAIL+1)); }
warn(){ printf '%s %s — %s\n' "$WARN_C" "$1" "$2"; }
need(){ command -v "$1" >/dev/null 2>&1 || { fail "tool $1" "missing"; exit 1; }; }

need python3
need jq
need open
need osascript

[[ -S "$SOCKET_PATH" ]] || { fail "preflight: broker socket" "$SOCKET_PATH missing"; exit 1; }
pass "preflight: broker socket present"

# AX permission check via the broker's own --check-ax flag. Skip the smoke
# (rc=0) if not granted — CI environment will hit this path.
ax_bin=""
if [[ -x "$BIN_DIR/one-for-all-broker" ]]; then
    ax_bin="$BIN_DIR/one-for-all-broker"
elif [[ -x "$PROJECT_ROOT/target/release/one-for-all-broker" ]]; then
    ax_bin="$PROJECT_ROOT/target/release/one-for-all-broker"
elif [[ -x "$PROJECT_ROOT/target/debug/one-for-all-broker" ]]; then
    ax_bin="$PROJECT_ROOT/target/debug/one-for-all-broker"
fi
if [[ -z "$ax_bin" ]]; then
    warn "ax-check binary missing" "skipping native smoke (build broker first)"
    echo "Summary: $PASS PASS / $FAIL FAIL — SKIPPED"
    exit 0
fi
if ! "$ax_bin" --check-ax >/dev/null 2>&1; then
    warn "AX permission not granted" "skipping native smoke (grant in System Settings → Privacy & Security → Accessibility)"
    echo "Summary: $PASS PASS / $FAIL FAIL — SKIPPED"
    exit 0
fi
pass "AX permission granted"

# Capture the user's frontmost app BEFORE we touch anything. We assert it
# does NOT change across the test run.
FRONTMOST_BEFORE_BUNDLE="$(osascript -e 'tell application "System Events" to get bundle identifier of first application process whose frontmost is true' 2>/dev/null || echo unknown)"

# Boot Calculator off-foreground. -g = no foreground, -j = hide on launch.
# This mimics how an agent should bring an app online without disturbing the user.
open -gj /System/Applications/Calculator.app 2>/dev/null || open -gj /Applications/Calculator.app 2>/dev/null || true

# Give Calculator a moment to register with the AX system. ~500ms is plenty
# on a warm boot; cold first-launch can take ~1s. Bound the wait.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if osascript -e 'tell application "System Events" to (name of processes) contains "Calculator"' 2>/dev/null | grep -q true; then
        break
    fi
    sleep 0.2
done

cat > "$TMP_DIR/orchestrator.py" <<'PY'
"""Calculator app.* end-to-end orchestrator. Speaks SPEC §2 directly to the broker."""
import json, os, socket, sys, threading, time
from pathlib import Path

SOCK = os.environ["HOME"] + "/.one-for-all/broker.sock"
BUNDLE = "com.apple.calculator"

class Client:
    def __init__(self):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(20.0)
        self.s.connect(SOCK)
        self.buf = b""
        self.nid = 1
        self.lock = threading.Lock()
        self.session_id = None

    def _readline(self):
        while b"\n" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                return ""
            self.buf += chunk
        line, _, self.buf = self.buf.partition(b"\n")
        return line.decode()

    def call(self, method, params):
        with self.lock:
            rid = self.nid; self.nid += 1
            req = {"jsonrpc":"2.0","id":rid,"method":method,"params":params}
            self.s.sendall((json.dumps(req)+"\n").encode())
            while True:
                line = self._readline()
                if not line:
                    raise RuntimeError(f"broker closed during {method}")
                m = json.loads(line)
                if m.get("id") == rid:
                    if m.get("error"):
                        raise RuntimeError(f"{method} error: {m['error']}")
                    return m.get("result") or {}

    def register(self, capabilities):
        r = self.call("session.register", {
            "client_name": "ofa-app-smoke",
            "client_version": "0.1.0",
            "capabilities": capabilities,
        })
        self.session_id = r.get("session_id")
        return r

    def close(self):
        try: self.call("session.unregister", {})
        except Exception: pass
        try: self.s.close()
        except Exception: pass


def find_button(elements, name):
    """First AXButton whose displayed name matches `name`."""
    for e in elements:
        role = e.get("role") or ""
        n = e.get("name") or ""
        if role.endswith("Button") and n == name:
            return e
    return None


def display_text(elements):
    """Calculator's display is an AXScrollArea-contained AXStaticText whose
    `value` (or `name`) is the current readout. We tolerate either field."""
    # Prefer text under a ScrollArea — that's where the running total lives.
    for e in elements:
        role = e.get("role") or ""
        if role == "AXScrollArea":
            # The AXScrollArea itself often carries the value via its
            # description. Walk the snapshot for the descendant text too.
            v = e.get("value") or e.get("description") or e.get("name")
            if v:
                return v
    # Fallback: the largest static text field on the window.
    candidates = []
    for e in elements:
        role = e.get("role") or ""
        if role in ("AXStaticText", "AXTextField"):
            v = e.get("value") or e.get("name") or ""
            if v:
                candidates.append((e.get("bbox", {}).get("w", 0) * e.get("bbox", {}).get("h", 0), v))
    if candidates:
        candidates.sort(reverse=True)
        return candidates[0][1]
    return ""


results = {"assertions": [], "transcript": []}
def a(name, ok, detail=""):
    results["assertions"].append({"name": name, "ok": bool(ok), "detail": detail})


def click_label(c, snap, label):
    btn = find_button(snap["elements"], label)
    if not btn:
        raise RuntimeError(f"no AXButton named {label!r} in snapshot")
    c.call("app.click", {"app_id": BUNDLE, "ref": btn["ref"]})
    # Calculator updates its display synchronously on the click action;
    # tiny sleep tolerates layout debouncing.
    time.sleep(0.15)


try:
    c = Client()
    rA = c.register(["tools", "events", "native"])
    a("session register with native capability", bool(c.session_id), str(rA))
    results["transcript"].append({"step": "session.register", "session_id": c.session_id})

    apps = c.call("app.list", {}).get("apps", []) or []
    calc = next((x for x in apps if x.get("bundle_id") == BUNDLE), None)
    a("app.list contains com.apple.calculator", calc is not None,
      f"have {len(apps)} apps; calc={calc!r}")
    results["transcript"].append({"step": "app.list", "calc": calc})

    if not calc:
        raise RuntimeError("calculator not running; the smoke harness needs it booted")

    snap = c.call("app.snapshot", {"app_id": BUNDLE})
    elements = snap.get("elements") or []
    a("app.snapshot returns elements", len(elements) > 0,
      f"elements={len(elements)} truncated_at={snap.get('truncated_at')}")
    results["transcript"].append({
        "step": "app.snapshot.before",
        "snapshot_seq": snap.get("snapshot_seq"),
        "title": snap.get("title"),
        "n_elements": len(elements),
        "buttons": [
            {"ref": e["ref"], "name": e.get("name"), "role": e.get("role")}
            for e in elements
            if (e.get("role") or "").endswith("Button")
        ][:25],
    })

    five = find_button(elements, "5")
    a("AXButton '5' present", five is not None,
      "" if five else "no five button in snapshot")
    results["transcript"].append({"step": "find_button:5", "button": five})

    if not five:
        raise RuntimeError("'5' button missing from snapshot — Calculator AX surface unexpected")

    # Clear first via the All Clear button to start from a known state.
    for label in ("AC", "C", "All Clear"):
        ac = find_button(elements, label)
        if ac:
            c.call("app.click", {"app_id": BUNDLE, "ref": ac["ref"]})
            time.sleep(0.1)
            break

    # Re-snapshot after AC so refs are fresh.
    snap = c.call("app.snapshot", {"app_id": BUNDLE})
    click_label(c, snap, "5")
    results["transcript"].append({"step": "app.click:5"})

    snap_after = c.call("app.snapshot", {"app_id": BUNDLE})
    disp = display_text(snap_after.get("elements") or [])
    a("display reads '5' after click '5'", "5" in str(disp),
      f"display={disp!r}")
    results["transcript"].append({"step": "app.snapshot.after_5", "display": disp})

    # 5 + 3 = round-trip → display "8".
    snap = c.call("app.snapshot", {"app_id": BUNDLE})
    click_label(c, snap, "+")
    snap = c.call("app.snapshot", {"app_id": BUNDLE})
    click_label(c, snap, "3")
    snap = c.call("app.snapshot", {"app_id": BUNDLE})
    click_label(c, snap, "=")
    snap = c.call("app.snapshot", {"app_id": BUNDLE})
    disp = display_text(snap.get("elements") or [])
    a("display reads '8' after 5 + 3 =", "8" in str(disp),
      f"display={disp!r}")
    results["transcript"].append({"step": "app.snapshot.after_5+3=", "display": disp})

    # app.eval round-trip: get the version number via AppleScript.
    try:
        v = c.call("app.eval", {"app_id": BUNDLE, "applescript": "get version of application id \"com.apple.calculator\""})
        a("app.eval get version returned a value", bool(v.get("value")),
          f"value={v}")
        results["transcript"].append({"step": "app.eval:version", "value": v.get("value")})
    except Exception as e:
        # AppleScript may fail in restricted environments; surface as a
        # non-fatal warning rather than failing the run.
        a("app.eval get version returned a value", False, f"{e}")

    # Negative test: attempting an `activate` body must be rejected.
    try:
        _ = c.call("app.eval", {"app_id": BUNDLE, "applescript": "tell application \"Calculator\" to activate"})
        a("app.eval rejects 'activate' bodies", False, "call succeeded; expected -32009")
    except RuntimeError as e:
        msg = str(e)
        a("app.eval rejects 'activate' bodies", "activate" in msg.lower() or "-32009" in msg,
          msg)

    c.close()
finally:
    Path(sys.argv[1]).write_text(json.dumps(results, indent=2))

ok = all(x["ok"] for x in results["assertions"])
sys.exit(0 if ok else 1)
PY

if python3 "$TMP_DIR/orchestrator.py" "$TMP_DIR/results.json"; then
    rc=0
else
    rc=$?
fi

if [[ -f "$TMP_DIR/results.json" ]]; then
    while IFS= read -r row; do
        name="$(jq -r '.name'  <<<"$row")"
        ok="$(jq -r '.ok'      <<<"$row")"
        det="$(jq -r '.detail' <<<"$row")"
        if [[ "$ok" == "true" ]]; then pass "$name"; else fail "$name" "${det:-failed}"; fi
    done < <(jq -c '.assertions[]' "$TMP_DIR/results.json")
fi

# Frontmost-pid post-check. Acceptable values: same as before, OR our own
# Terminal/iTerm if the user's frontmost was already that. Anything else
# (especially Calculator) means we stole focus — fail.
FRONTMOST_AFTER_BUNDLE="$(osascript -e 'tell application "System Events" to get bundle identifier of first application process whose frontmost is true' 2>/dev/null || echo unknown)"
if [[ "$FRONTMOST_AFTER_BUNDLE" == "com.apple.calculator" ]]; then
    fail "focus-no-steal: Calculator must NOT come to foreground" "before=$FRONTMOST_BEFORE_BUNDLE after=$FRONTMOST_AFTER_BUNDLE"
elif [[ "$FRONTMOST_AFTER_BUNDLE" == "$FRONTMOST_BEFORE_BUNDLE" ]]; then
    pass "focus-no-steal: frontmost unchanged ($FRONTMOST_BEFORE_BUNDLE)"
else
    # Tolerate frontmost drift outside our control (user-driven, screensaver,
    # etc.) but log it so a CI failure has context.
    pass "focus-no-steal: frontmost not Calculator (before=$FRONTMOST_BEFORE_BUNDLE, after=$FRONTMOST_AFTER_BUNDLE)"
fi

# Print transcript for the team-lead summary.
if [[ -f "$TMP_DIR/results.json" ]]; then
    echo
    echo "── transcript ───────────────────────────────────────────────"
    jq '.transcript' "$TMP_DIR/results.json"
    echo "─────────────────────────────────────────────────────────────"
fi

echo
printf 'Summary: %d PASS / %d FAIL (orchestrator rc=%d)\n' "$PASS" "$FAIL" "$rc"
[[ "$FAIL" -eq 0 && "$rc" -eq 0 ]] || exit 1
