. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-end" -Agent "gemini-cli"
exit 0
