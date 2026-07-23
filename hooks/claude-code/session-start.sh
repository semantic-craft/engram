#!/bin/sh
# Claude Code SessionStart hook.
# 1. Forwards the event JSON to the engram server (fire-and-forget).
# 2. Synchronously fetches the pending cross-agent handoff and prints
#    it to stdout — Claude Code prepends `session-start` stdout to the
#    next session, so the resuming agent sees prior context with no
#    human in the loop.
#
# Walks up from the payload's cwd for a .engram.toml marker file
# and appends cwd plus marker query params to both URLs — so a session
# resuming under basename or marker-declared routing doesn't query the
# wrong bucket and miss its own handoff.
# At runtime (after `install-hooks --apply`) `_lib.sh` is staged
# alongside this script. From the source tree it lives one dir up.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${ENGRAM_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(engram_extract_cwd "$PAYLOAD")
QS=$(engram_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | engram_post_hook "$SERVER/hook?event=session-start&agent=claude-code${QS}" >/dev/null 2>&1 || true

# Claude Code prepends a SessionStart hook's stdout to the resuming
# session as context. Emit it as structured JSON
# (hookSpecificOutput.additionalContext) instead of raw text: bare text
# does not start with "{", so Claude Code logs every session start as
# "Hook output does not start with {, treating as plain text". JSON
# injects the same handoff with a clean debug log; no handoff -> "{}".
HANDOFF=$(engram_get_handoff "$SERVER/handoff?agent=claude-code${QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' \
        "$(printf '%s' "$HANDOFF" | engram_json_string)"
else
    printf '{}\n'
fi
exit 0
