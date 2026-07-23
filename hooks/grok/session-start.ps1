. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-start" -Agent "grok"
exit 0
