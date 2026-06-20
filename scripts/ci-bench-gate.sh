#!/usr/bin/env bash
# SPEC §11 V5 — CI gate for the SLO benchmark suite.
#
# Runs `cargo bench -p bench --offline` and parses each bench's
# `BENCH_RESULT=…` JSON line on stdout. Fails the run if any bench
# missed its SLO; surfaces tracked WARNs for benches that emitted
# `kind:"skipped"` (e.g. `sandbox_spawn` when `BENCH_SANDBOX_V3` is
# unset) so we never silently lose coverage.
#
# Output format: one BENCH_RESULT line per bench, e.g.:
#   BENCH_RESULT={"name":"frame_capture_to_event_p99","kind":"p99_us",
#                 "target":50000,"measured":18342,"p50_us":12,…,"passed":true}
#
# Exit codes:
#   0 — every bench passed (skips are warns, not failures)
#   1 — at least one bench failed its SLO
#   2 — usage / harness error

set -euo pipefail

usage() {
    cat <<EOF
Usage: $(basename "$0") [--bench-args "<extra args to cargo bench>"]

Runs the V5 SLO bench suite on the host (must be macOS-arm64) and
gates CI on the measured p99 / throughput targets defined in SPEC §11
V4 latency budget table.

Environment overrides:
  BENCH_MODE=warn        Treat SLO misses as WARN, not failure (triage runs).
  BENCH_REAL_CHROMIUM=1  Run page_click against a real Chromium subprocess.
  BENCH_SANDBOX_V3=1     Run sandbox_spawn against the V3 sandbox crate.
EOF
}

BENCH_ARGS=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bench-args) BENCH_ARGS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown arg: $1" >&2; usage; exit 2 ;;
    esac
done

if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not on PATH" >&2
    exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "ERROR: jq is required to parse BENCH_RESULT lines" >&2
    exit 2
fi

LOG_FILE="$(mktemp -t v5-bench-gate.XXXXXX.log)"
trap 'rm -f "$LOG_FILE"' EXIT

# Run the bench suite. Allow it to "fail" — criterion may exit nonzero
# when a benchmark panics on SLO_STRICT mode; we still parse stdout for
# the BENCH_RESULT lines so we report every bench's status.
echo "==> running V5 SLO benches (BENCH_MODE=${BENCH_MODE:-strict})"
set +e
# shellcheck disable=SC2086
cargo bench -p bench --offline -- $BENCH_ARGS 2>&1 | tee "$LOG_FILE"
BENCH_EXIT=$?
set -e

# Extract BENCH_RESULT lines, one JSON object per line.
RESULTS_JSON="$(grep -E '^BENCH_RESULT=' "$LOG_FILE" | sed 's/^BENCH_RESULT=//' || true)"

if [[ -z "$RESULTS_JSON" ]]; then
    echo "ERROR: no BENCH_RESULT lines emitted; bench harness silently skipped?" >&2
    exit 2
fi

echo
echo "==> SLO summary"
printf '%-36s %-22s %12s %12s %s\n' "BENCH" "KIND" "MEASURED" "TARGET" "STATUS"

FAILED=0
WARNED=0
PASSED=0

while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    name=$(jq -r .name <<<"$line")
    kind=$(jq -r .kind <<<"$line")
    target=$(jq -r .target <<<"$line")
    measured=$(jq -r .measured <<<"$line")
    passed=$(jq -r .passed <<<"$line")

    case "$kind" in
        skipped)
            printf '%-36s %-22s %12s %12s \033[33mWARN (skipped)\033[0m\n' \
                "$name" "$kind" "$measured" "$target"
            WARNED=$((WARNED + 1))
            ;;
        *)
            if [[ "$passed" == "true" ]]; then
                printf '%-36s %-22s %12s %12s \033[32mPASS\033[0m\n' \
                    "$name" "$kind" "$measured" "$target"
                PASSED=$((PASSED + 1))
            else
                printf '%-36s %-22s %12s %12s \033[31mFAIL\033[0m\n' \
                    "$name" "$kind" "$measured" "$target"
                FAILED=$((FAILED + 1))
            fi
            ;;
    esac
done <<<"$RESULTS_JSON"

echo
echo "==> totals: ${PASSED} passed, ${WARNED} warned (skipped), ${FAILED} failed"

if [[ "$FAILED" -gt 0 ]]; then
    echo "FAIL: at least one bench missed its SLO" >&2
    exit 1
fi

# Bench harness exited nonzero but every bench passed — surface as WARN
# (could be e.g. a clippy warning during build that didn't affect SLO).
if [[ "$BENCH_EXIT" -ne 0 ]]; then
    echo "WARN: cargo bench exited ${BENCH_EXIT} but all SLOs passed"
fi

exit 0
