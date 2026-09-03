#!/bin/sh
# Grok Build CLI SessionStart hook. Capture only.
#
# Agent Adapter contract: Grok IGNORES SessionStart stdout ("For events like
# SessionStart or PostToolUse, stdout is ignored"), so this adapter performs no
# automatic Handoff read and no mutation — a claim nobody could read would
# consume a transfer and leave a lease no Run can acknowledge. The Handoff stays
# `open` for an explicit on-demand `memory_handoff_claim` (discover it first
# with `memory_handoff_discover`). The server enforces the same rule from
# `engram_core::adapter`, so deleting this comment does not change behaviour.
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
