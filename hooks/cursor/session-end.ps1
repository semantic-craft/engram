. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-end" -Agent "cursor"
exit 0
