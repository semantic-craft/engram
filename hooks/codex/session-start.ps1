. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-start" -Agent "codex" -FetchHandoff
exit 0
