. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "pre-tool-use" -Agent "claude-code"
exit 0
