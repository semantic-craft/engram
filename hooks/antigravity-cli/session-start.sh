#!/bin/sh
# antigravity-cli PreInvocation hook. Forwards the event JSON to the
# engram server, then injects the claimed continuation as an ephemeral
# model-visible message using Antigravity's JSON stdout contract.
#
# Agent Adapter contract: this harness DELIVERS session-start output, so the
# server may discover and atomically claim one eligible Handoff and render the
# continuation envelope, claim metadata, and a budgeted ContextPackage. The
# Handoff is left CLAIMED, never accepted — the receiving Run's first
# `memory_checkpoint_write` acknowledges it, and a lost session's lease expires
# back to `open`. See `engram_core::adapter` for the capability table.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${ENGRAM_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(engram_extract_cwd "$PAYLOAD")
QS=$(engram_marker_qs "$CWD")
# The Run this session is starting. Forwarded on /handoff so the server can
# bind an automatic Handoff claim to the actual receiving Run; without it the
# server renders read-only and the agent claims through MCP instead.
RUN_QS=$(engram_session_qs "$PAYLOAD")

printf '%s' "$PAYLOAD" \
    | engram_post_hook "$SERVER/hook?event=session-start&agent=antigravity-cli${QS}" >/dev/null 2>&1 || true
HANDOFF=$(engram_get_handoff "$SERVER/handoff?agent=antigravity-cli${QS}${RUN_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"injectSteps":[{"ephemeralMessage":'
    printf '%s' "$HANDOFF" | engram_json_string
    printf '}]}\n'
else
    printf '{}\n'
fi
exit 0
