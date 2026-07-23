. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "pre-compact" -Agent "gemini-cli"
exit 0
