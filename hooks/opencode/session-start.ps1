. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-start" -Agent "open-code" -FetchHandoff
exit 0
