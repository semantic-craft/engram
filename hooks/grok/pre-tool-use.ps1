. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "pre-tool-use" -Agent "grok"
exit 0
