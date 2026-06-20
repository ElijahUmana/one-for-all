#!/usr/bin/env bash
# ofa-trace — inspect M10 broker trace files.
#
#   ofa-trace ls                       List sessions with trace dirs.
#   ofa-trace tail [SESSION]           Live-follow newest trace file.
#   ofa-trace summarize SESSION SEQ    Print event-type histogram.
#   ofa-trace events SESSION SEQ       Pretty-print all events.

set -euo pipefail
trap 'rc=$?; [[ $rc -ne 0 ]] && echo "[ofa-trace] line $LINENO rc=$rc" >&2' ERR

HOME_DIR="${HOME:?HOME must be set}"
SESS_DIR="${HOME_DIR}/.one-for-all/sessions"

usage() { sed -n '2,8p' "$0"; }

cmd="${1:-help}"; shift || true

case "$cmd" in
    ls)
        printf '%-32s %-6s %-12s\n' SESSION FILES BYTES
        if [[ -d "$SESS_DIR" ]]; then
            for d in "$SESS_DIR"/*/trace; do
                [[ -d "$d" ]] || continue
                sid="$(basename "$(dirname "$d")")"
                files="$(find "$d" -name '*.jsonl' 2>/dev/null | wc -l | tr -d ' ')"
                bytes="$(du -ck "$d" 2>/dev/null | tail -1 | awk '{print $1}')k"
                printf '%-32s %-6s %-12s\n' "$sid" "$files" "$bytes"
            done
        fi
        ;;
    tail)
        sid="${1:-}"
        if [[ -z "$sid" ]]; then
            sid="$(ls -t "$SESS_DIR" 2>/dev/null | head -1)"
        fi
        d="$SESS_DIR/$sid/trace"
        [[ -d "$d" ]] || { echo "no trace dir for $sid" >&2; exit 1; }
        f="$(ls -t "$d"/*.jsonl 2>/dev/null | head -1)"
        [[ -n "$f" ]] || { echo "no trace files in $d" >&2; exit 1; }
        echo "[ofa-trace] tailing $f"
        # Schema: every variant has ts_ms + kind; method/tab_id/tool are
        # variant-specific (only emitted when present, hence `?`).
        tail -F "$f" | jq -c '{ts: .ts_ms, kind, tab_id: .tab_id?, method: .method?, tool: .tool?}'
        ;;
    summarize)
        sid="${1:?session id required}"; seq="${2:?seq required}"
        # Trace files are zero-padded by the writer (e.g. 0000.jsonl); accept
        # either bare seq or full filename.
        if [[ "$seq" == *.jsonl ]]; then
            f="$SESS_DIR/$sid/trace/$seq"
        else
            f="$(ls "$SESS_DIR/$sid/trace/"*"${seq}.jsonl" 2>/dev/null | head -1)"
            [[ -z "$f" ]] && f="$SESS_DIR/$sid/trace/${seq}.jsonl"
        fi
        [[ -f "$f" ]] || { echo "missing trace file (looked up: $f)" >&2; exit 1; }
        echo "[ofa-trace] $f"
        jq -r '.kind' "$f" | sort | uniq -c | sort -rn
        ;;
    events)
        sid="${1:?session id required}"; seq="${2:?seq required}"
        if [[ "$seq" == *.jsonl ]]; then
            f="$SESS_DIR/$sid/trace/$seq"
        else
            f="$(ls "$SESS_DIR/$sid/trace/"*"${seq}.jsonl" 2>/dev/null | head -1)"
            [[ -z "$f" ]] && f="$SESS_DIR/$sid/trace/${seq}.jsonl"
        fi
        [[ -f "$f" ]] || { echo "missing trace file (looked up: $f)" >&2; exit 1; }
        jq -C '.' "$f"
        ;;
    help|-h|--help) usage ;;
    *) usage; exit 2 ;;
esac
