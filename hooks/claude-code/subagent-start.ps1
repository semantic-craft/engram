. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "subagent-start" -Agent "claude-code"
exit 0
