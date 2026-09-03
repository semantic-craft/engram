# Grok Build CLI SessionStart hook (PowerShell). Capture only: the shared Agent
# Adapter contract classifies Grok as ignoring SessionStart output, so the
# handoff-fetch switch is deliberately omitted below. This adapter performs no
# automatic Handoff read or mutation and leaves the transfer open for an
# on-demand MCP claim.
. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-start" -Agent "grok"
exit 0
