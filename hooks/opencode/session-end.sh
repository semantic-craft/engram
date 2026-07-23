#!/bin/sh
# opencode session-end hook.
# Forwards the event JSON to the engram server, fire-and-forget.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${ENGRAM_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(engram_extract_cwd "$PAYLOAD")
QS=$(engram_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | engram_post_hook "$SERVER/hook?event=session-end&agent=open-code${QS}" >/dev/null 2>&1 || true
exit 0
