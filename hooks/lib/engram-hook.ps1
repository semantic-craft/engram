function Get-EngramCwd {
    param([string] $Payload)
    if (-not $Payload) { return $null }
    try {
        $Parsed = $Payload | ConvertFrom-Json -ErrorAction Stop
        foreach ($Name in @("cwd", "current_dir", "working_dir", "directory")) {
            $Value = $Parsed.$Name
            if ($Value -is [string] -and $Value.Length -gt 0) { return $Value }
        }
        $Paths = $Parsed.workspacePaths
        if ($null -ne $Paths -and $Paths.Count -gt 0 -and $Paths[0] -is [string] -and $Paths[0].Length -gt 0) {
            return $Paths[0]
        }
    } catch {
    }
    $match = [regex]::Match($Payload, '"cwd"\s*:\s*"([^"]*)"')
    if ($match.Success) { return $match.Groups[1].Value }
    $workspaceMatch = [regex]::Match($Payload, '"workspacePaths"\s*:\s*\[\s*"([^"]*)"')
    if ($workspaceMatch.Success) { return $workspaceMatch.Groups[1].Value }
    return $null
}

function Get-EngramMarkerToml {
    param([string] $Cwd)
    if (-not $Cwd) { return $null }
    $dir = $Cwd
    while ($dir -and (Test-Path $dir)) {
        $candidate = Join-Path $dir ".engram.toml"
        if (Test-Path $candidate -PathType Leaf) { return $candidate }
        if ($env:HOME -and $dir -eq $env:HOME) { return $null }
        if ($env:USERPROFILE -and $dir -eq $env:USERPROFILE) { return $null }
        $parent = Split-Path $dir -Parent
        if (-not $parent -or $parent -eq $dir) { return $null }
        $dir = $parent
    }
    return $null
}

function Get-EngramTomlKey {
    param([string] $File, [string] $Key)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    foreach ($line in Get-Content $File) {
        $m = [regex]::Match($line, "^\s*$Key\s*=\s*`"([^`"]*)`"")
        if ($m.Success) { return $m.Groups[1].Value }
    }
    return $null
}

# Resolve the basename of the MAIN git repository root for $Cwd, following the
# worktree commondir pointer so every linked worktree collapses to one stable
# name. Mirrors the POSIX `engram_repo_root_project`: a containerized server
# cannot see the host checkout, so repo-root must be resolved here. Returns
# $null when git is unavailable or $Cwd is not inside a git work tree.
function Get-EngramRepoRootProject {
    param([string] $Cwd)
    if (-not $Cwd) { return $null }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { return $null }
    $inside = (& git -C $Cwd rev-parse --is-inside-work-tree 2>$null)
    if ($inside -ne "true") { return $null }
    $common = (& git -C $Cwd rev-parse --path-format=absolute --git-common-dir 2>$null)
    if (-not $common) { return $null }
    $root = Split-Path $common -Parent
    if (-not $root -or $root -eq [System.IO.Path]::GetPathRoot($root)) { return $null }
    return Split-Path $root -Leaf
}

function Get-EngramMarkerQuery {
    param([string] $Cwd)
    if (-not $Cwd) { return "" }
    $qs = "&cwd=$([uri]::EscapeDataString($Cwd))"
    $ws = $null
    $proj = $null
    $strategy = $null
    $dropSubagent = $null
    $marker = Get-EngramMarkerToml -Cwd $Cwd
    if ($marker) {
        $ws = Get-EngramTomlKey -File $marker -Key "workspace"
        $proj = Get-EngramTomlKey -File $marker -Key "project"
        $strategy = Get-EngramTomlKey -File $marker -Key "project_strategy"
        $dropSubagent = Get-EngramTomlKey -File $marker -Key "drop_subagent_captures"
    }
    # Install-time default baked into the hook command by
    # `install-hooks --project-strategy` fills the strategy only when no marker
    # pinned one. A marker's explicit project / project_strategy still win.
    if (-not $strategy -and $env:ENGRAM_PROJECT_STRATEGY) {
        $strategy = $env:ENGRAM_PROJECT_STRATEGY
    }
    # repo-root must be resolved host-side (the server may not see this checkout);
    # only when no explicit project is pinned. Explicit project always wins.
    if (-not $proj -and ($strategy -eq "repo-root" -or $strategy -eq "repo_root")) {
        $proj = Get-EngramRepoRootProject -Cwd $Cwd
    }
    if ($ws) { $qs += "&workspace=$([uri]::EscapeDataString($ws))" }
    if ($proj) { $qs += "&project=$([uri]::EscapeDataString($proj))" }
    if ($strategy) { $qs += "&project_strategy=$([uri]::EscapeDataString($strategy))" }
    # Per-project drop_subagent_captures opt-in: forward to the server, which
    # interprets truthiness (1/true/...) and scopes the drop to this project.
    if ($dropSubagent) { $qs += "&drop_subagent=$([uri]::EscapeDataString($dropSubagent))" }
    return $qs
}

function Read-EngramStdin {
    try {
        if (-not [Console]::IsInputRedirected) { return "" }
        $StdinStream = [Console]::OpenStandardInput()
        $StdinReader = [System.IO.StreamReader]::new($StdinStream, [System.Text.Encoding]::UTF8, $false, 4096)
        $ReadTask = $StdinReader.ReadToEndAsync()
        if ($ReadTask.Wait(2000)) {
            $result = $ReadTask.Result
            $StdinReader.Dispose()
            $StdinStream.Dispose()
            return $result
        }
        $StdinReader.Dispose()
        $StdinStream.Dispose()
    } catch {
    }
    return ""
}

function Invoke-EngramHook {
    param(
        [Parameter(Mandatory = $true)] [string] $Event,
        [Parameter(Mandatory = $true)] [string] $Agent,
        [switch] $FetchHandoff,
        [switch] $AntigravityPreInvocationOutput
    )

    $Server = if ($env:ENGRAM_HOOK_URL) { $env:ENGRAM_HOOK_URL } else { "http://127.0.0.1:49374" }
    $Payload = Read-EngramStdin
    $Cwd = Get-EngramCwd -Payload $Payload
    $QS = Get-EngramMarkerQuery -Cwd $Cwd
    $Headers = @{}

    if ($env:ENGRAM_AUTH_TOKEN) {
        $Headers["Authorization"] = "Bearer $env:ENGRAM_AUTH_TOKEN"
    }

    try {
        Invoke-WebRequest `
            -UseBasicParsing `
            -TimeoutSec 3 `
            -Method Post `
            -Uri "$Server/hook?event=$Event&agent=$Agent$QS" `
            -Headers $Headers `
            -ContentType "application/json" `
            -Body $Payload | Out-Null
    } catch {
    }

    if ($FetchHandoff) {
        try {
            $Response = Invoke-WebRequest `
                -UseBasicParsing `
                -TimeoutSec 2 `
                -Uri "$Server/handoff?agent=$Agent$QS" `
                -Headers $Headers
            if ($null -ne $Response -and $Response.Content) {
                if ($AntigravityPreInvocationOutput) {
                    $Payload = @{
                        injectSteps = @(@{ ephemeralMessage = $Response.Content })
                    }
                    [Console]::Out.Write(($Payload | ConvertTo-Json -Depth 5 -Compress))
                } else {
                    [Console]::Out.Write($Response.Content)
                }
            } elseif ($AntigravityPreInvocationOutput) {
                [Console]::Out.Write("{}")
            }
        } catch {
            if ($AntigravityPreInvocationOutput) {
                [Console]::Out.Write("{}")
            }
        }
    } elseif ($AntigravityPreInvocationOutput) {
        [Console]::Out.Write("{}")
    }
}
