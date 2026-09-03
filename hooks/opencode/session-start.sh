#!/bin/sh
# opencode SessionStart hook.
# 1. Forwards the event JSON to the engram server (fire-and-forget).
# 2. Synchronously claims any pending cross-agent continuation and prints
#    it to stdout — agent CLIs prepend session-start hook stdout to
#    the next session, so the resuming agent sees prior context with
#    no human in the loop.
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
    | engram_post_hook "$SERVER/hook?event=session-start&agent=open-code${QS}" >/dev/null 2>&1 || true
engram_get_handoff "$SERVER/handoff?agent=open-code${QS}${RUN_QS}" 2>/dev/null || true
exit 0
