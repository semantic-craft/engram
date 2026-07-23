. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "user-prompt" -Agent "gemini-cli"
exit 0
