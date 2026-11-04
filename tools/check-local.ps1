# tools/check-local.ps1
<#
.SYNOPSIS
Run local Rust validation once with the selected build features.
.DESCRIPTION
Runs format, strict Clippy and selected package tests once, failing at the first error.
Asio selects the release feature configuration after verified SDK preparation;
it does not add a second pass. Omit Asio to validate a non-ASIO build.
Logs stay in .rust-validation/local-checks/<run>/.
This does not build a release, install binaries or run subsystem validators.
Windows checks require the MSVC toolchain. If the default host is GNU, save the
current RUSTUP_TOOLCHAIN value, set $env:RUSTUP_TOOLCHAIN to
'1.97.1-x86_64-pc-windows-msvc' in this shell before invoking the script, and
restore the saved value afterward. Do not change the global Rustup toolchain.
.PARAMETER Asio
Enable desktop/CLI ASIO features for the single Clippy/test pass.
.PARAMETER Package
Packages affected by the change, including relevant direct consumers. Omit for
an explicitly requested workspace-wide check; release tags do not require it.
.PARAMETER NoDefaultFeatures
Match a release built with --no-default-features (for example the headless server).
.PARAMETER Target
Override the repository Cargo target for Clippy/tests (for example,
aarch64-apple-darwin or x86_64-unknown-linux-gnu). Format is target-independent.
ASIO requires a Windows MSVC target; omitting Target retains repository defaults.
.PARAMETER CleanBuildArtifacts
After checks pass, prune debug caches above BuildCacheLimitGiB (default 40).
Requires pwsh 7.3+; do not nest this cleanup inside an active release pipeline.
.EXAMPLE
./tools/check-local.ps1
.EXAMPLE
./tools/check-local.ps1 -Asio
.EXAMPLE
./tools/check-local.ps1 -Package audiolib-desktop,audiolib-cli -Asio
.EXAMPLE
./tools/check-local.ps1 -Package audiolib-server -NoDefaultFeatures -Target x86_64-unknown-linux-gnu
.EXAMPLE
./tools/check-local.ps1 -Target aarch64-apple-darwin
#>
[CmdletBinding()]
param(
    [switch]$Asio,
    [ValidateNotNullOrEmpty()][string[]]$Package,
    [switch]$NoDefaultFeatures,
    [ValidateNotNullOrEmpty()][string]$Target,
    [switch]$CleanBuildArtifacts,
    [ValidateRange(0,100000)][double]$BuildCacheLimitGiB = 40
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$effectiveTarget = if ($Target) { $Target } elseif ($env:CARGO_BUILD_TARGET) { $env:CARGO_BUILD_TARGET } else { 'x86_64-pc-windows-msvc' }
if ($Asio -and $effectiveTarget -notmatch '-windows-msvc$') {
    throw 'ASIO checks require a Windows MSVC target; choose a matching -Target.'
}
$selectionArguments = @('--workspace')
$featureArguments = @()
if ($Package) {
    $Package = @($Package | Select-Object -Unique)
    $selectionArguments = @($Package | ForEach-Object { '-p'; $_ })
}
if ($Asio) {
    $asioPackages = if ($Package) { @($Package | Where-Object { $_ -in @('audiolib-desktop','audiolib-cli','audiolib-audio','audiolib-windows') }) } else { @('audiolib-desktop','audiolib-cli') }
    if (!$asioPackages.Count) { throw '-Asio requires selecting an ASIO-capable package; unrelated packages do not need a second feature configuration.' }
    $featureArguments = @('--features', (($asioPackages | ForEach-Object { "$_/asio" }) -join ','))
}
if ($NoDefaultFeatures) { $featureArguments += '--no-default-features' }
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
. (Join-Path $PSScriptRoot 'build-cache.ps1')
if ($CleanBuildArtifacts -and $PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Use pwsh 7.3+ for post-check build cache maintenance.' }
$buildLease = $null
$runId = [guid]::NewGuid().ToString('N')
$logs = Join-Path (Join-Path $taskRoot '.rust-validation/local-checks') $runId
$targetArguments = if ($Target) { @('--target', $Target) } else { @() }
$savedAsioDirectory = [Environment]::GetEnvironmentVariable('CPAL_ASIO_DIR', 'Process')
$savedClangDirectory = [Environment]::GetEnvironmentVariable('LIBCLANG_PATH', 'Process')

function Invoke-LoggedCheck([string]$Name, [scriptblock]$Command) {
    $logPath = Join-Path $logs ($Name + '.log')
    Write-Host "Running $Name"
    try {
        & $Command *> $logPath
    } catch {
        $_ | Out-String | Add-Content -LiteralPath $logPath
        Get-Content -LiteralPath $logPath -Tail 60 | ForEach-Object { Write-Host $_ }
        throw "$Name failed. Log: $logPath. $($_.Exception.Message)"
    }
}

function Invoke-CargoCheck([string]$Name, [string[]]$Arguments) {
    Invoke-LoggedCheck $Name {
        $cargoCommand = Get-Command cargo -ErrorAction Stop
        $global:LASTEXITCODE = $null
        try {
            # Windows PowerShell treats redirected native stderr as errors.
            # Cargo progress goes to stderr; the process exit code decides success.
            $ErrorActionPreference = 'Continue'
            & $cargoCommand @Arguments
            $cargoExitCode = $global:LASTEXITCODE
        } finally {
            $ErrorActionPreference = 'Stop'
        }
        if ($null -eq $cargoExitCode) { throw 'cargo did not report an exit code.' }
        if ($cargoExitCode -ne 0) { throw "cargo exited with code $cargoExitCode." }
    }
}

Push-Location -LiteralPath $taskRoot
try {
    $buildLease = Enter-AudioLibBuildLease $taskRoot
    New-Item -ItemType Directory -Path $logs -Force | Out-Null
    Invoke-CargoCheck 'format' @('fmt', '--all', '--', '--check')
    if ($Asio) {
        Invoke-LoggedCheck 'prepare-asio' { . (Join-Path $PSScriptRoot 'prepare-asio.ps1') }
    }
    $configuration = if ($Asio) { 'asio' } else { 'default' }
    $checkArguments = $selectionArguments + @('--all-targets', '--locked') + $targetArguments + $featureArguments
    Invoke-CargoCheck "$configuration-clippy" (@('clippy') + $checkArguments + @('--', '-D', 'warnings'))
    Invoke-CargoCheck "$configuration-tests" (@('test') + $checkArguments)
    $selection = if ($Package) { $Package -join ', ' } else { 'workspace' }
    $scope = "$selection, $configuration configuration only; no-default-features=$([bool]$NoDefaultFeatures)"
    Write-Host "Local checks passed ($scope). Logs: $logs"
} finally {
    if ($buildLease) { $buildLease.Dispose() }
    [Environment]::SetEnvironmentVariable('CPAL_ASIO_DIR', $savedAsioDirectory, 'Process')
    [Environment]::SetEnvironmentVariable('LIBCLANG_PATH', $savedClangDirectory, 'Process')
    Pop-Location
}
if ($CleanBuildArtifacts) {
    & (Join-Path $PSScriptRoot 'clean-build-artifacts.ps1') -Action Run -Mode Debug -MaxGiB $BuildCacheLimitGiB
    if ($LASTEXITCODE -ne 0) { throw 'Checks passed; build cache maintenance did not complete. Inspect its status; no process was stopped.' }
}
