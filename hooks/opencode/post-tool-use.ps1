. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "post-tool-use" -Agent "open-code"
exit 0
