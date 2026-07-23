. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "pre-compact" -Agent "grok"
exit 0
