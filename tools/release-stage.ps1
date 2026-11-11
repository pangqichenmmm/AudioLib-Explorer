# tools/release-stage.ps1
# Isolate one pipeline command, preserving typed script parameters and exit status.
# Invoked by release-pipeline.ps1; command files and logs are private local evidence.
#Requires -Version 7.3
param([Parameter(Mandatory)][string]$CommandFile)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$stageLock = $null
try {
    $stage = Get-Content -LiteralPath $CommandFile -Raw | ConvertFrom-Json -AsHashtable
    # Survives a parent-runner interruption until this child exits. A retry
    # cannot overlap a still-running build/install from the interrupted run.
    $stageLock = [IO.File]::Open($stage.lock_path, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    Set-Location -LiteralPath $stage.root
    if ($stage.ContainsKey('github_auth') -and $stage.github_auth -and !$env:GH_TOKEN -and !$env:GITHUB_TOKEN) {
        # Resolve once inside this child. Never persist the credential in a plan,
        # receipt, command file or parent environment.
        $token = & gh auth token 2>$null
        if ($LASTEXITCODE -ne 0 -or !$token) {
            $credential = "protocol=https`nhost=github.com`n`n" | git -c credential.interactive=never credential fill 2>$null
            if ($LASTEXITCODE -ne 0) { throw 'No non-interactive GitHub credential; configure gh or Git Credential Manager.' }
            $password = $credential | Where-Object { $_.StartsWith('password=') } | Select-Object -First 1
            if (!$password) { throw 'No usable GitHub credential.' }
            $token = $password.Substring(9)
            $credential=$null; $password=$null
        }
        $env:GH_TOKEN = ([string]$token).Trim()
        $token=$null
    }
    if ($stage.ContainsKey('script')) {
        $parameters = $stage.parameters
        & $stage.script @parameters
        if (!$?) { throw 'PowerShell stage failed.' }
    } else {
        $arguments = $stage.arguments
        & $stage.command @arguments
        if ($LASTEXITCODE -ne 0) { throw "Native stage exited with code $LASTEXITCODE." }
    }
    exit 0
} catch {
    [Console]::Error.WriteLine($_.ToString())
    exit 1
} finally { if ($stageLock) { $stageLock.Dispose() } }
