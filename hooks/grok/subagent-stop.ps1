. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "subagent-stop" -Agent "grok"
exit 0
