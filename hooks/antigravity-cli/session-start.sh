#!/bin/sh
# antigravity-cli PreInvocation hook. Forwards the event JSON to the
# engram server, then injects any pending handoff as an ephemeral
# model-visible message using Antigravity's JSON stdout contract.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${ENGRAM_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(engram_extract_cwd "$PAYLOAD")
QS=$(engram_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | engram_post_hook "$SERVER/hook?event=session-start&agent=antigravity-cli${QS}" >/dev/null 2>&1 || true
HANDOFF=$(engram_get_handoff "$SERVER/handoff?agent=antigravity-cli${QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"injectSteps":[{"ephemeralMessage":'
    printf '%s' "$HANDOFF" | engram_json_string
    printf '}]}\n'
else
    printf '{}\n'
fi
exit 0
