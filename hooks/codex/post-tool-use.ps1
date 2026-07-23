. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "post-tool-use" -Agent "codex"
exit 0
