. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "pre-tool-use" -Agent "antigravity-cli"
[Console]::Out.WriteLine('{ "decision": "allow" }')
exit 0
