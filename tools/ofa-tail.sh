#!/usr/bin/env bash
# ofa-tail — color multi-tail of broker, mcp, and chromium logs.
#
# Usage:
#   ofa-tail [--filter REGEX]
#   ofa-tail --trace SESSION_ID [--filter REGEX]    # SPEC §10 M10

set -euo pipefail
trap 'kill 0 2>/dev/null; exit 0' INT TERM

HOME_DIR="${HOME:?HOME must be set}"
LOG_DIR="${HOME_DIR}/.one-for-all/logs"
SESS_DIR="${HOME_DIR}/.one-for-all/sessions"

FILTER=""
TRACE_SESSION=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --filter) FILTER="$2"; shift 2 ;;
        --trace) TRACE_SESSION="$2"; shift 2 ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

color() {
    local color="$1" prefix="$2"
    while IFS= read -r line; do
        printf '\033[%sm[%s]\033[0m %s\n' "$color" "$prefix" "$line"
    done
}

# SPEC §10 M10 — when --trace is passed, follow the latest <NNNN>.jsonl
# under that session's trace dir. Optionally filter by regex.
if [[ -n "$TRACE_SESSION" ]]; then
    trace_dir="$SESS_DIR/$TRACE_SESSION/trace"
    if [[ ! -d "$trace_dir" ]]; then
        echo "no trace dir for $TRACE_SESSION at $trace_dir" >&2
        exit 1
    fi
    # Pick the highest-numbered current file.
    latest=$(ls -1 "$trace_dir"/[0-9]*.jsonl 2>/dev/null | sort | tail -1)
    if [[ -z "$latest" ]]; then
        echo "no trace files in $trace_dir" >&2
        exit 1
    fi
    echo "[ofa-tail] tailing trace $latest"
    if [[ -n "$FILTER" ]]; then
        tail -n 0 -F "$latest" \
            | grep --line-buffered -E "$FILTER" \
            | color "1;32" "trace:$TRACE_SESSION"
    else
        tail -n 0 -F "$latest" | color "1;32" "trace:$TRACE_SESSION"
    fi
    exit 0
fi

watch_component() {
    local comp="$1" color_code="$2"
    local dir="$LOG_DIR/$comp"
    mkdir -p "$dir"
    # tail -F follows file rotation; * matches today's rolled file.
    if [[ -n "$FILTER" ]]; then
        ( tail -n 0 -F "$dir"/*.log 2>/dev/null \
            | grep --line-buffered -E "$FILTER" \
            | color "$color_code" "$comp" ) &
    else
        ( tail -n 0 -F "$dir"/*.log 2>/dev/null | color "$color_code" "$comp" ) &
    fi
}

watch_component broker   "1;36"   # cyan
watch_component mcp      "1;35"   # magenta
watch_component chromium "1;33"   # yellow

wait
