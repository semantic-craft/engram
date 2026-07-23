. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-end" -Agent "claude-code"
exit 0
