. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "post-tool-use" -Agent "antigravity-cli"
[Console]::Out.WriteLine("{}")
exit 0
