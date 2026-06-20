#!/usr/bin/env bash
# lint-sh.sh — bash -n every shell script in the repo. Used by install.sh
# self-check and CI.

set -euo pipefail
trap 'rc=$?; echo "[lint-sh] FAILED at line $LINENO (exit $rc)" >&2; exit $rc' ERR

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

PASS=0; FAIL=0
PASS_C=$'\033[1;32m[ok]\033[0m'
FAIL_C=$'\033[1;31m[FAIL]\033[0m'

scripts="$(find installer tools -type f \( -name '*.sh' \) 2>/dev/null | sort)"
for f in $scripts; do
    if bash -n "$f" 2>/dev/null; then
        printf '%s %s\n' "$PASS_C" "$f"; PASS=$((PASS+1))
    else
        printf '%s %s\n' "$FAIL_C" "$f"; bash -n "$f"; FAIL=$((FAIL+1))
    fi
done

# Also lint embedded heredoc'd Python by ast.parse.
py_files="$(find installer -type f -name '*.py' | sort)"
for f in $py_files; do
    if python3 -c 'import ast,sys; ast.parse(open(sys.argv[1]).read())' "$f"; then
        printf '%s %s\n' "$PASS_C" "$f"; PASS=$((PASS+1))
    else
        printf '%s %s\n' "$FAIL_C" "$f"; FAIL=$((FAIL+1))
    fi
done

echo
printf 'Summary: %d ok / %d FAIL\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]] || exit 1
