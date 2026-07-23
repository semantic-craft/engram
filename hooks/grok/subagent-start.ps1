. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "subagent-start" -Agent "grok"
exit 0
