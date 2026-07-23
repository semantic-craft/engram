. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "user-prompt" -Agent "claude-code"
exit 0
