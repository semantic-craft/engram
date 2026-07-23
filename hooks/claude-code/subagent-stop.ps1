. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "subagent-stop" -Agent "claude-code"
exit 0
