. "$PSScriptRoot\..\lib\engram-hook.ps1"
Invoke-EngramHook -Event "session-start" -Agent "antigravity-cli" -FetchHandoff -AntigravityPreInvocationOutput
exit 0
