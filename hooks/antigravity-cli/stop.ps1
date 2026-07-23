. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "stop" -Agent "antigravity-cli"
[Console]::Out.WriteLine('{"decision":""}')
exit 0
