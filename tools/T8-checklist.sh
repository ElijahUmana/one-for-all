#!/usr/bin/env bash
# tools/T8-checklist.sh — single-script T8 finalization gate.
#
# Runs every quality gate from the internal specification §10 plus the byte-exact invariants the
# reviewer-finisher hunts for in T7. Designed to be re-run by T8 after each
# fix lands so we know objectively when we're "done".
#
# Usage:
#   bash tools/T8-checklist.sh             # full run, exit 1 on first fail
#   bash tools/T8-checklist.sh --soft      # report all failures, exit 0
#   bash tools/T8-checklist.sh --only=NAME # run only the named check
#
# Each check has a one-line PASS/FAIL line and, on fail, a "details:" block
# showing the offending lines so the fixer can grep-and-fix without reopening
# this script.

set -uo pipefail

# ---- config -----------------------------------------------------------------

SOFT=0
ONLY=""
for arg in "$@"; do
  case "$arg" in
    --soft) SOFT=1 ;;
    --only=*) ONLY="${arg#--only=}" ;;
    -h|--help)
      sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

cd "$(dirname "$0")/.." || exit 2
ROOT="$(pwd)"

# ANSI only when stdout is a tty
if [[ -t 1 ]]; then
  RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; DIM=$'\033[2m'; RST=$'\033[0m'
else
  RED=""; GREEN=""; YELLOW=""; DIM=""; RST=""
fi

PASS_COUNT=0
FAIL_COUNT=0
FAILED_NAMES=()

run_check() {
  local name="$1"; shift
  local desc="$1"; shift
  if [[ -n "$ONLY" && "$ONLY" != "$name" ]]; then
    return 0
  fi
  printf '  %s ... ' "$name"
  local out
  if out="$("$@" 2>&1)"; then
    printf '%sPASS%s  %s%s%s\n' "$GREEN" "$RST" "$DIM" "$desc" "$RST"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    printf '%sFAIL%s  %s%s%s\n' "$RED" "$RST" "$DIM" "$desc" "$RST"
    if [[ -n "$out" ]]; then
      printf '    details:\n'
      printf '%s\n' "$out" | sed 's/^/      /' | head -20
      local total
      total=$(printf '%s\n' "$out" | wc -l)
      if [[ "$total" -gt 20 ]]; then
        printf '      ... (%d more lines)\n' $((total - 20))
      fi
    fi
    FAIL_COUNT=$((FAIL_COUNT + 1))
    FAILED_NAMES+=("$name")
  fi
}

# ---- check definitions ------------------------------------------------------
# Each `check_*` fn prints offending lines to stdout and exits 1 if any are
# found, exits 0 otherwise. `out` is captured by run_check.

# §10 zero .unwrap() / .expect() outside #[cfg(test)] / tests/
#
# Strategy: Rust convention puts unit tests at the bottom of the file in a
# `#[cfg(test)] mod tests { ... }` block. So we treat everything from the
# first `mod tests {` line onward as test scope. Files under `tests/`
# directories (integration tests) are excluded entirely.
#
# We also honor an explicit `#[allow(clippy::unwrap_used)]` /
# `#[allow(clippy::expect_used)]` in the 3 lines preceding a hit — that
# bypass is the documented escape hatch and SPEC §10's lint enforces the
# rule when no allow is present.
check_no_unwrap_in_prod() {
  local prod=""
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    local test_start
    # Find the first `#[cfg(test)]` directly preceding a `mod` line — that's
    # the conventional Rust unit-test boundary. Also fall back to a literal
    # `mod tests` line if no `#[cfg(test)]` is present.
    test_start=$(awk '
      /^[[:space:]]*#\[cfg\(test\)\]/ { pending = NR; next }
      /^[[:space:]]*mod [a-zA-Z0-9_]+ *\{/ && pending && NR == pending + 1 { print pending; exit }
      /^[[:space:]]*mod tests *\{/ { print NR; exit }
      { pending = 0 }
    ' "$f")
    local cap
    if [[ -n "$test_start" ]]; then
      cap=$((test_start - 1))
    else
      cap=$(wc -l < "$f")
    fi
    local hits
    hits=$(awk -v cap="$cap" '
      NR > cap { exit }
      # Skip pure-comment lines.
      /^[[:space:]]*\/\// { next }
      # Track an `#[allow(clippy::unwrap_used)]` or `expect_used` attr that
      # may apply to the next item.
      /#\[allow\(.*clippy::(unwrap_used|expect_used)/ { allow_until = NR + 8; next }
      (/\.unwrap\(\)/ || /\.expect\(/) && NR > allow_until {
        print FILENAME":"NR": "$0
      }
    ' "$f")
    [[ -n "$hits" ]] && prod+="$hits"$'\n'
  done < <(find crates -name "*.rs" -not -path "*/tests/*" -not -path "*/benches/*" 2>/dev/null)
  if [[ -n "$prod" ]]; then
    printf '%s' "$prod"
    return 1
  fi
  return 0
}

# §10 #![deny(clippy::unwrap_used, clippy::expect_used)] on every lib.rs/main.rs
check_deny_lints() {
  local missing=""
  for f in crates/*/src/lib.rs crates/*/src/main.rs; do
    [[ -f "$f" ]] || continue
    if ! grep -q 'deny(.*clippy::unwrap_used' "$f" 2>/dev/null; then
      missing+="$f: missing #![deny(clippy::unwrap_used, clippy::expect_used)]"$'\n'
    fi
  done
  if [[ -n "$missing" ]]; then
    printf '%s' "$missing"
    return 1
  fi
  return 0
}

# §10 every channel is bounded — no mpsc::unbounded_channel
check_no_unbounded_channel() {
  local hits
  hits=$(grep -rn 'unbounded_channel\|unbounded::<' crates/*/src/ 2>/dev/null \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*///' \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//!')
  if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

# §10 // CANCELLATION: comment on every async pub fn
check_cancellation_comments() {
  local missing=""
  while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    local f l
    f="${entry%%:*}"
    local rest="${entry#*:}"
    l="${rest%%:*}"
    # Look for CANCELLATION: in the 5 lines before this fn declaration
    local prev_start
    prev_start=$(( l > 5 ? l - 5 : 1 ))
    if ! sed -n "${prev_start},${l}p" "$f" | grep -q 'CANCELLATION:'; then
      missing+="$f:$l: pub async fn missing // CANCELLATION: comment"$'\n'
    fi
  done < <(grep -rn 'pub async fn ' crates/*/src/ 2>/dev/null)
  if [[ -n "$missing" ]]; then
    printf '%s' "$missing"
    return 1
  fi
  return 0
}

# Forbidden APIs per SPEC §5 — runtime usage (not in protocol JSON, not in doc comments)
check_no_forbidden_apis() {
  local hits
  hits=$(grep -rn '"Target\.activateTarget"\|activateIgnoringOtherApps\|NSApplicationActivateAllWindows\|makeKeyAndOrderFront' crates/*/src/ 2>/dev/null \
    | grep -v 'src/.*\.json' \
    | grep -v -E '^[^:]+:[0-9]+:[[:space:]]*//' \
    | grep -v -E '^[^:]+:[0-9]+:[[:space:]]*///' \
    | grep -v -E '^[^:]+:[0-9]+:[[:space:]]*//!' \
    || true)
  if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

# clippy.toml workspace-wide disallowed-methods (R6 enforcement guard)
check_clippy_disallowed_methods_config() {
  if [[ ! -f clippy.toml ]]; then
    echo "clippy.toml at workspace root does not exist"
    return 1
  fi
  if ! grep -q 'disallowed-methods' clippy.toml; then
    echo "clippy.toml exists but has no disallowed-methods entry"
    return 1
  fi
  return 0
}

# §10 Pinned exact dep versions, no ^/~/>=
check_pinned_versions() {
  local hits
  # In Cargo.toml a bare "1" or "1.0" is interpreted as ^1 / ^1.0 — that's a
  # range, not a pin. Exact pins start with "=".
  hits=$(awk '
    /^\[workspace\.dependencies\]/ {in_ws=1; next}
    /^\[/ {in_ws=0; next}
    in_ws && /^[a-zA-Z0-9_-]+ *=/ {
      if (match($0, /"[~^]?[0-9]/) || match($0, />=/)) {
        # If the version literal does NOT start with "=", it is a range.
        if (!match($0, /"=[0-9]/)) print FILENAME":"NR": "$0
      }
    }
  ' Cargo.toml 2>/dev/null)
  if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

# Error code table byte-exact match to D17
check_error_code_table() {
  local expected="-32001 SessionNotFound
-32002 TabNotFound
-32003 ContextNotFound
-32004 ElementStale
-32005 ElementNotActionable
-32006 NavigationFailed
-32007 Timeout
-32008 ChromiumLaunchFailed
-32009 PermissionDenied
-32010 ProtocolError
-32011 BrokerUnavailable
-32012 SessionLimitExceeded"
  local actual
  # Two-line scan: variant name on one line, discriminant on the next
  # `=> -3200N,` line. Portable to BSD awk (no gawk match() captures).
  actual=$(awk '
    {
      # capture "ErrorCode::Foo" or just "Foo," variant decl
      if (match($0, /ErrorCode::[A-Z][a-zA-Z]+/)) {
        s = substr($0, RSTART, RLENGTH)
        sub(/ErrorCode::/, "", s)
        # look for "=> -3200N" or "= -3200N" on same line
        if (match($0, /-3200[0-9]/) || match($0, /-3201[0-2]/)) {
          v = substr($0, RSTART, RLENGTH)
          print v" "s
        }
      }
    }
  ' crates/broker/src/protocol.rs 2>/dev/null | sort -u)
  local expected_sorted
  expected_sorted=$(echo "$expected" | sort)
  if [[ "$actual" != "$expected_sorted" ]]; then
    diff <(echo "$expected_sorted") <(echo "$actual")
    return 1
  fi
  return 0
}

# Framing caps in the right files at the right values
check_framing_caps() {
  local fail=""
  if ! grep -q 'LINE_CAP_BYTES.*16 *\* *1024 *\* *1024' crates/broker/src/server.rs 2>/dev/null; then
    fail+="broker socket 16MB cap not found at crates/broker/src/server.rs"$'\n'
  fi
  if ! grep -q 'DEFAULT_MAX_FRAME_BYTES.*100 *\* *1024 *\* *1024' crates/cdp-client/src/framing.rs 2>/dev/null; then
    fail+="CDP NUL framing 100MB cap not found at crates/cdp-client/src/framing.rs"$'\n'
  fi
  if ! grep -q 'Content-Length' crates/mcp-server/src/mcp.rs 2>/dev/null; then
    fail+="LSP Content-Length framing not found at crates/mcp-server/src/mcp.rs"$'\n'
  fi
  if [[ -n "$fail" ]]; then
    printf '%s' "$fail"
    return 1
  fi
  return 0
}

# Tool surface: broker-supported public tool families and MCP-published tools
# must stay in lockstep. This is the high-signal parity gate for the split
# router world: compare the broker's manual `supported_methods()` list against
# the MCP `TOOL_NAMES`/descriptor surface for every public family.
check_tool_surface_consistent() {
  local pattern mcp_tools broker_tools missing_from_broker missing_from_mcp
  pattern='"(browser|tab|page|net|vision|app|clipboard|drag|term|system)\.[a-z_.]+"'
  mcp_tools=$(grep -oE "$pattern" crates/mcp-server/src/tools.rs 2>/dev/null | tr -d '"' | sort -u)
  broker_tools=$(grep -oE "$pattern" crates/broker/src/router/mod.rs 2>/dev/null | tr -d '"' | sort -u)
  missing_from_broker=$(comm -23 <(printf '%s\n' "$mcp_tools") <(printf '%s\n' "$broker_tools"))
  missing_from_mcp=$(comm -23 <(printf '%s\n' "$broker_tools") <(printf '%s\n' "$mcp_tools"))
  if [[ -n "$missing_from_broker" || -n "$missing_from_mcp" ]]; then
    if [[ -n "$missing_from_broker" ]]; then
      echo "tools published by mcp-server but missing from broker/router/mod.rs supported surface:"
      echo "$missing_from_broker"
    fi
    if [[ -n "$missing_from_mcp" ]]; then
      echo "tools supported by broker/router/mod.rs but missing from mcp-server published surface:"
      echo "$missing_from_mcp"
    fi
    return 1
  fi
  return 0
}

# Narrow U9 parity gate: only the terminal PTY surface and its notify topics.
# This is the retained proof check for task #27 while the broader repo-wide
# broker↔MCP parity check above is still red for unrelated families.
check_term_surface_consistent() {
  local mcp_terms broker_terms missing_from_broker missing_from_mcp fail=""
  mcp_terms=$(grep -oE '"term\.[a-z_.]+"' crates/mcp-server/src/tools.rs 2>/dev/null | tr -d '"' | sort -u)
  broker_terms=$(grep -oE '"term\.[a-z_.]+"' crates/broker/src/router/mod.rs 2>/dev/null | tr -d '"' | sort -u | grep -vE '^term\.(output|exit)$')
  missing_from_broker=$(comm -23 <(printf '%s\n' "$mcp_terms") <(printf '%s\n' "$broker_terms"))
  missing_from_mcp=$(comm -23 <(printf '%s\n' "$broker_terms") <(printf '%s\n' "$mcp_terms"))
  if [[ -n "$missing_from_broker" ]]; then
    fail+="term tools published by mcp-server but missing from broker/router/mod.rs:\n$missing_from_broker\n"
  fi
  if [[ -n "$missing_from_mcp" ]]; then
    fail+="term tools supported by broker/router/mod.rs but missing from mcp-server:\n$missing_from_mcp\n"
  fi
  grep -q '"term.output"' crates/broker/src/router/mod.rs 2>/dev/null || fail+="broker supported_events missing term.output\n"
  grep -q '"term.exit"' crates/broker/src/router/mod.rs 2>/dev/null || fail+="broker supported_events missing term.exit\n"
  grep -q '`term.output`' docs/PROTOCOL.md 2>/dev/null || fail+="docs/PROTOCOL.md missing term.output anchor\n"
  grep -q '`term.exit`' docs/PROTOCOL.md 2>/dev/null || fail+="docs/PROTOCOL.md missing term.exit anchor\n"
  if [[ -n "$fail" ]]; then
    printf '%b' "$fail"
    return 1
  fi
  return 0
}

# M-feature presence (cheap structural checks; functional verification is in e2e)
check_m_features() {
  local fail=""
  grep -q 'pub console: Vec' crates/ax-engine/src/merge.rs \
    || fail+="M1: Snapshot.console field not found"$'\n'
  grep -q 'since_seq' crates/ax-engine/src/lib.rs \
    || fail+="M2: snapshot_delta since_seq not present"$'\n'
  grep -q 'addScriptToEvaluateOnNewDocument.*stealth\|stealth.*addScriptToEvaluateOnNewDocument' crates/browser-engine/src/context.rs \
    || fail+="M3: stealth bundle not injected via Page.addScriptToEvaluateOnNewDocument"$'\n'
  grep -q 'session.recovered' crates/broker/src/recovery.rs \
    || fail+="M4: session.recovered topic not emitted in recovery.rs"$'\n'
  grep -q 'consoleAPICalled\|exceptionThrown' crates/browser-engine/src/page.rs \
    || fail+="M5: console/exception streaming not wired"$'\n'
  grep -q 'realistic.*headless\|headless.*realistic\|Headless.*realistic\|realistic_default' crates/browser-engine/src/browser.rs \
    || fail+="M6: realistic input default policy not wired"$'\n'
  grep -q 'page.network_conditions\|page_network_conditions' crates/broker/src/router/mod.rs \
    || fail+="M7: page.network_conditions not dispatched"$'\n'
  grep -q 'page.emulate\|page_emulate' crates/broker/src/router/mod.rs \
    || fail+="M8: page.emulate not dispatched"$'\n'
  grep -q 'RLIMIT_AS' crates/browser-engine/src/browser.rs \
    || fail+="M9: setrlimit RLIMIT_AS not present in browser.rs"$'\n'
  grep -rq 'TraceWriter\|TraceRecorder\|trace.*jsonl\|trace_writer' crates/observability/src/ crates/broker/src/ 2>/dev/null \
    || fail+="M10: trace recorder/jsonl writer not implemented in observability or broker"$'\n'
  if [[ -n "$fail" ]]; then
    printf '%s' "$fail"
    return 1
  fi
  return 0
}

# R11 installer must not touch NativeMessagingHosts
check_no_native_messaging_hosts() {
  local hits
  hits=$(grep -rn 'NativeMessagingHosts' installer/ tools/ 2>/dev/null \
    | grep -v 'tools/T8-checklist.sh' \
    || true)
  if [[ -n "$hits" ]]; then
    echo "$hits"
    return 1
  fi
  return 0
}

# R13 each .rs file under 1500 lines
check_file_length_cap() {
  local hits
  hits=$(find crates -name "*.rs" -exec wc -l {} \; 2>/dev/null | awk '$1 > 1500 {print $2": "$1" lines (cap 1500)"}')
  if [[ -n "$hits" ]]; then
    printf '%s\n' "$hits"
    return 1
  fi
  return 0
}

# cargo gates — only run if cargo is available
check_cargo_check() { command -v cargo >/dev/null 2>&1 || { echo "cargo not in PATH"; return 1; }; cargo check --workspace --all-targets 2>&1 | tail -20; cargo check --workspace --all-targets >/dev/null 2>&1; }
check_cargo_test()  { command -v cargo >/dev/null 2>&1 || { echo "cargo not in PATH"; return 1; }; cargo test  --workspace --all-targets 2>&1 | tail -20; cargo test  --workspace --all-targets >/dev/null 2>&1; }
check_cargo_clippy(){ command -v cargo >/dev/null 2>&1 || { echo "cargo not in PATH"; return 1; }; cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20; cargo clippy --workspace --all-targets -- -D warnings >/dev/null 2>&1; }
check_cargo_deny()  { command -v cargo-deny  >/dev/null 2>&1 || { echo "cargo-deny not installed"; return 1; }; cargo deny  check 2>&1 | tail -20; cargo deny  check >/dev/null 2>&1; }
check_cargo_audit() { command -v cargo-audit >/dev/null 2>&1 || { echo "cargo-audit not installed"; return 1; }; cargo audit         2>&1 | tail -20; cargo audit         >/dev/null 2>&1; }
check_cargo_udeps() { command -v cargo-udeps >/dev/null 2>&1 || { echo "cargo-udeps not installed (nightly)"; return 1; }; cargo +nightly udeps --workspace 2>&1 | tail -20; cargo +nightly udeps --workspace >/dev/null 2>&1; }

# ---- run --------------------------------------------------------------------

echo
echo "=== T8 checklist for $ROOT ==="
echo "(use --only=NAME to run a single check; --soft to never exit nonzero)"
echo

echo "--- §10 quality non-negotiables ---"
run_check no_unwrap_in_prod          "no .unwrap()/.expect() outside #[cfg(test)]/tests/"  check_no_unwrap_in_prod
run_check deny_lints                 "every lib.rs/main.rs has #![deny(unwrap_used,expect_used)]"  check_deny_lints
run_check no_unbounded_channel       "no mpsc::unbounded_channel anywhere"                  check_no_unbounded_channel
run_check cancellation_comments      "every pub async fn has // CANCELLATION:"              check_cancellation_comments
run_check pinned_versions            "workspace deps use exact pins (no ^/~/>=)"            check_pinned_versions

echo
echo "--- §5 forbidden APIs ---"
run_check no_forbidden_apis          "no Target.activateTarget / NSApp.activate / makeKeyAndOrderFront in runtime"  check_no_forbidden_apis
run_check clippy_disallowed_config   "clippy.toml has disallowed-methods entry"             check_clippy_disallowed_methods_config

echo
echo "--- byte-exact wire invariants ---"
run_check error_code_table           "broker error codes -32001..-32012 match D17 exactly"  check_error_code_table
run_check framing_caps               "LSP / 16MB / 100MB framing caps in canonical files"   check_framing_caps
run_check tool_surface_consistent    "every mcp-server tool is dispatched by broker"        check_tool_surface_consistent
run_check term_surface_consistent    "U9 term.* and term notify anchors stay aligned across broker, MCP, and docs" check_term_surface_consistent

echo
echo "--- §10 must-have features ---"
run_check m_features                 "M1-M10 structurally present"                          check_m_features

echo
echo "--- installer / R-rule guards ---"
run_check no_native_messaging_hosts  "installer never touches NativeMessagingHosts (R11)"   check_no_native_messaging_hosts
run_check file_length_cap            "every .rs under 1500 lines (R13)"                      check_file_length_cap

echo
echo "--- cargo gates ---"
run_check cargo_check                "cargo check --workspace --all-targets"                 check_cargo_check
run_check cargo_test                 "cargo test --workspace --all-targets"                  check_cargo_test
run_check cargo_clippy               "cargo clippy --workspace --all-targets -- -D warnings" check_cargo_clippy
run_check cargo_deny                 "cargo deny check"                                      check_cargo_deny
run_check cargo_audit                "cargo audit"                                           check_cargo_audit
run_check cargo_udeps                "cargo +nightly udeps --workspace"                      check_cargo_udeps

echo
echo "--- summary ---"
echo "  ${GREEN}pass: $PASS_COUNT${RST}    ${RED}fail: $FAIL_COUNT${RST}"
if [[ "$FAIL_COUNT" -gt 0 ]]; then
  echo "  failed: ${FAILED_NAMES[*]}"
  if [[ "$SOFT" -eq 0 ]]; then
    exit 1
  fi
fi
exit 0
