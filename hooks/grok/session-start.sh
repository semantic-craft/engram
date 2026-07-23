#!/bin/sh
# Grok Build CLI SessionStart hook.
# Grok ignores SessionStart stdout, so this hook captures the event only.
# Do NOT fetch /handoff here: accepting a handoff is destructive and Grok
# would discard the returned context.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${ENGRAM_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(engram_extract_cwd "$PAYLOAD")
QS=$(engram_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | engram_post_hook "$SERVER/hook?event=session-start&agent=grok${QS}" >/dev/null 2>&1 || true
printf '{}\n'
exit 0
