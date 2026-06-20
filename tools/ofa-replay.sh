#!/usr/bin/env bash
# ofa-replay — SPEC §10 M10 replay tool.
#
# Usage:
#   ofa-replay ls                                 List all sessions with traces.
#   ofa-replay summary SESSION                    Show counts per kind.
#   ofa-replay actions SESSION                    Print every action with timing.
#   ofa-replay step SESSION                       Step interactively through actions.
#   ofa-replay screenshot SESSION INDEX           Print path to Nth screenshot (0-indexed).
#   ofa-replay search SESSION REGEX               grep across rotated trace JSONL.
#   ofa-replay verify SESSION                     Verify HMAC manifest if present.
#   ofa-replay export SESSION DEST.tar            Bundle the trace dir into a tar.
#   ofa-replay upload SESSION URL                 POST the bundle to URL.
#
# Each subcommand operates on `~/.one-for-all/sessions/<id>/trace/`.
#
# Reading rotated, possibly gzipped trace files: the script transparently
# zcats `*.jsonl.gz` and cats `*.jsonl`, in numeric order, so consumers see
# a single contiguous stream.

set -euo pipefail
trap 'rc=$?; [[ $rc -ne 0 ]] && echo "[ofa-replay] line $LINENO rc=$rc" >&2' ERR

HOME_DIR="${HOME:?HOME must be set}"
SESS_DIR="${HOME_DIR}/.one-for-all/sessions"

usage() { sed -n '2,16p' "$0"; }

# Stream all trace records for SESSION in seq order, transparently
# decompressing rotated files.
stream_trace() {
    local sid="$1"
    local d="$SESS_DIR/$sid/trace"
    [[ -d "$d" ]] || { echo "no trace dir for $sid" >&2; return 1; }
    local files=()
    while IFS= read -r f; do files+=("$f"); done < <(
        ls -1 "$d"/[0-9]*.jsonl "$d"/[0-9]*.jsonl.gz 2>/dev/null \
            | awk -F/ '{ print $NF, $0 }' \
            | sort -k1,1 \
            | awk '{ print $2 }'
    )
    for f in "${files[@]}"; do
        case "$f" in
            *.jsonl.gz) gunzip -c "$f" 2>/dev/null || true ;;
            *.jsonl)    cat "$f" 2>/dev/null || true ;;
        esac
    done
}

cmd="${1:-help}"; shift || true

case "$cmd" in
    ls)
        printf '%-32s %-6s %-6s %-12s\n' SESSION FILES SCREENSHOTS BYTES
        if [[ -d "$SESS_DIR" ]]; then
            for d in "$SESS_DIR"/*/trace; do
                [[ -d "$d" ]] || continue
                sid="$(basename "$(dirname "$d")")"
                files="$(find "$d" -maxdepth 1 -type f \( -name '*.jsonl' -o -name '*.jsonl.gz' \) 2>/dev/null | wc -l | tr -d ' ')"
                shots="$(find "$d/screenshots" -maxdepth 1 -type f -name '*.png' 2>/dev/null | wc -l | tr -d ' ' || echo 0)"
                bytes="$(du -ck "$d" 2>/dev/null | tail -1 | awk '{print $1}')k"
                printf '%-32s %-6s %-6s %-12s\n' "$sid" "$files" "$shots" "$bytes"
            done
        fi
        ;;
    summary)
        sid="${1:?session id required}"
        stream_trace "$sid" \
            | jq -r '.kind // "unknown"' \
            | sort | uniq -c | sort -rn
        ;;
    actions)
        sid="${1:?session id required}"
        stream_trace "$sid" \
            | jq -rc 'select(.kind=="action") | "\(.ts_ms)\t\(.tool)\t\(.tab_id)\t\(.args | tostring | .[0:100])"'
        ;;
    step)
        sid="${1:?session id required}"
        # Interactive: dump each action one at a time, ask to continue.
        stream_trace "$sid" \
            | jq -rc 'select(.kind=="action") | "\(.ts_ms)\t\(.tool)\t\(.tab_id)\t\(.args | tostring | .[0:200])"' \
            | while IFS=$'\t' read -r ts tool tab args; do
                printf '\033[1;36m[%s] %s @tab=%s\033[0m\n  args: %s\n' "$ts" "$tool" "$tab" "$args"
                printf '   (n)ext | (s)kip-rest | (q)uit > '
                read -r ans </dev/tty || ans=q
                case "$ans" in
                    q*) break ;;
                    s*) cat >/dev/null; break ;;
                    *)  ;;
                esac
            done
        ;;
    screenshot)
        sid="${1:?session id required}"; idx="${2:?index required}"
        png=$(stream_trace "$sid" \
            | jq -rc 'select(.kind=="screenshot") | .png_path' \
            | sed -n "$((idx + 1))p")
        [[ -n "$png" ]] || { echo "no screenshot at index $idx" >&2; exit 1; }
        printf '%s\n' "$SESS_DIR/$sid/trace/$png"
        ;;
    search)
        sid="${1:?session id required}"; regex="${2:?regex required}"
        stream_trace "$sid" | grep --line-buffered -E "$regex" || true
        ;;
    verify)
        sid="${1:?session id required}"
        manifest="$SESS_DIR/$sid/trace/manifest.json"
        if [[ ! -f "$manifest" ]]; then
            echo "no manifest.json — trace was written without HMAC integrity"
            exit 1
        fi
        # Recompute HMAC via openssl over each line (excluding hmac itself).
        key="${OFA_TRACE_HMAC_KEY:-}"
        if [[ -z "$key" ]]; then
            echo "set OFA_TRACE_HMAC_KEY to verify" >&2; exit 2
        fi
        expected=$(jq -r '.hmac' "$manifest")
        body=$(jq -c 'del(.hmac)' "$manifest")
        actual=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$key" | awk '{print $2}')
        if [[ "$expected" == "$actual" ]]; then
            echo "OK: manifest HMAC matches"
        else
            echo "FAIL: HMAC mismatch (expected=$expected actual=$actual)" >&2
            exit 3
        fi
        ;;
    export)
        sid="${1:?session id required}"; dest="${2:?dest tar path required}"
        d="$SESS_DIR/$sid"
        [[ -d "$d/trace" ]] || { echo "no trace dir for $sid" >&2; exit 1; }
        ( cd "$SESS_DIR" && tar czf "$dest" "$sid/trace" )
        echo "exported $sid -> $dest ($(du -h "$dest" | awk '{print $1}'))"
        ;;
    upload)
        sid="${1:?session id required}"; url="${2:?upload URL required}"
        tmp=$(mktemp -t ofa-replay-upload.XXXXXX.tar.gz)
        trap 'rm -f "$tmp"' EXIT
        ( cd "$SESS_DIR" && tar czf "$tmp" "$sid/trace" )
        # Multipart upload — bundles JSONL + screenshots + snapshots in one go.
        echo "uploading $(du -h "$tmp" | awk '{print $1}') from $tmp to $url"
        curl --fail --silent --show-error -X POST \
            -F "session_id=$sid" \
            -F "bundle=@$tmp" \
            "$url"
        echo
        ;;
    help|-h|--help) usage ;;
    *) usage; exit 2 ;;
esac
