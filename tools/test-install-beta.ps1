# tools/test-install-beta.ps1
# Exercise the real beta installer/launcher in an owned temporary checkout.
# Package privacy is stubbed here; the maintained package allowlist is unchanged.
#Requires -Version 7.3
$ErrorActionPreference = 'Stop'
if (!$IsWindows) { throw 'The beta installation regression requires Windows.' }
$taskRoot = Join-Path ([IO.Path]::GetTempPath()) ('AudioLibBetaInstall-' + [guid]::NewGuid().ToString('N'))
$taskTools = Join-Path $taskRoot 'tools'
$taskSource = Join-Path $taskRoot 'payload'
$taskDestination = Join-Path $taskRoot 'beta_release'
New-Item -ItemType Directory -Path $taskTools, $taskSource, $taskDestination | Out-Null
foreach ($name in @('install-beta.ps1', 'AudioLibExplorer-beta.cmd')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $taskTools
}
$taskInstaller = Join-Path $taskTools 'install-beta.ps1'
$taskPwsh = Join-Path $PSHOME 'pwsh.exe'
@'
param([string]$BinaryDirectory)
$ErrorActionPreference = 'Stop'
$allowed = @('AudioLibExplorer.exe', 'AudioLibCli.exe', 'update-channel.txt', 'update-version.txt')
foreach ($file in Get-ChildItem -LiteralPath $BinaryDirectory -Recurse -File) {
    if ($file.Name -cnotin $allowed) { throw 'Unexpected fixture payload file.' }
}
foreach ($name in $allowed) {
    if (!(Test-Path -LiteralPath (Join-Path $BinaryDirectory $name) -PathType Leaf)) { throw 'Incomplete fixture payload.' }
}
'@ | Set-Content -LiteralPath (Join-Path $taskTools 'validate-package-privacy.ps1') -Encoding utf8

$taskCompiler = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
$taskLauncherAvailable = Test-Path -LiteralPath $taskCompiler -PathType Leaf
if ($taskLauncherAvailable) {
    $taskStubSource = Join-Path $taskRoot 'LauncherProbe.cs'
    @'
using System;
using System.IO;
internal static class LauncherProbe {
    private static void Main(string[] args) {
        if (args.Length != 1) { Environment.Exit(2); }
        File.WriteAllLines(args[0], new[] {
            Environment.GetEnvironmentVariable("AUDIOLIB_UPDATE_DISABLE") ?? "",
            Environment.GetEnvironmentVariable("AUDIOLIB_DATA_DIR") ?? ""
        });
    }
}
'@ | Set-Content -LiteralPath $taskStubSource -Encoding utf8
    & $taskCompiler /nologo /target:winexe (('/out:') + (Join-Path $taskSource 'AudioLibExplorer.exe')) $taskStubSource *> (Join-Path $taskRoot 'compile-launcher-probe.log')
    if ($LASTEXITCODE -ne 0) { throw "Launcher probe compilation failed; inspect $taskRoot" }
} else {
    [IO.File]::WriteAllText((Join-Path $taskSource 'AudioLibExplorer.exe'), 'owned executable identity fixture')
}
[IO.File]::WriteAllText((Join-Path $taskSource 'AudioLibCli.exe'), 'owned CLI identity fixture')
[IO.File]::WriteAllText((Join-Path $taskSource 'update-channel.txt'), 'stable')
[IO.File]::WriteAllText((Join-Path $taskSource 'update-version.txt'), '0.8.6')
$taskMarker = Join-Path $taskDestination 'update-development.txt'
$taskVersion = Join-Path $taskDestination 'update-version.txt'
$taskSentinel = Join-Path $taskDestination 'unowned.keep'
[IO.File]::WriteAllText($taskSentinel, 'preserve unrelated installation data')

function Invoke-FixtureInstaller([string]$Name, [string[]]$Extra = @(), [bool]$Succeeds = $true) {
    $log = Join-Path $taskRoot ($Name + '.log')
    & $taskPwsh -NoProfile -File $taskInstaller -Source $taskSource -Destination $taskDestination @Extra *> $log
    if (($LASTEXITCODE -eq 0) -ne $Succeeds) { throw "Unexpected installer result for $Name; inspect $log" }
    if ([IO.File]::ReadAllText($taskSentinel) -cne 'preserve unrelated installation data') { throw 'Unowned installation data was modified.' }
}
function Assert-Version([string]$Expected) {
    if ([IO.File]::ReadAllText($taskVersion).Trim() -cne $Expected) { throw 'Installed update version differs from its expected identity.' }
    if ([IO.File]::ReadAllText((Join-Path $taskDestination 'update-channel.txt')).Trim() -cne 'beta') { throw 'The installation did not keep the beta update channel.' }
}
function Assert-LauncherEnvironment([string]$Name, [string]$ExpectedDisable) {
    if (!$taskLauncherAvailable) { return }
    $output = Join-Path $taskRoot ($Name + '-environment.txt')
    $data = Join-Path $taskRoot ('beta-data-' + $Name)
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = Join-Path $env:WINDIR 'System32/cmd.exe'
    $info.Arguments = '/d /s /c ""' + (Join-Path $taskDestination 'AudioLibExplorer-beta.cmd') + '" "' + $output + '""'
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $info.Environment['LOCALAPPDATA'] = Join-Path $taskRoot 'owned-localappdata'
    $info.Environment['AUDIOLIB_BETA_DATA_DIR'] = $data
    $info.Environment['AUDIOLIB_DATA_DIR'] = Join-Path $taskRoot 'unused-normal-data'
    $null = $info.Environment.Remove('AUDIOLIB_UPDATE_DISABLE')
    $process = [Diagnostics.Process]::Start($info)
    try {
        if (!$process.WaitForExit(10000) -or $process.ExitCode -ne 0) { throw 'The owned beta launcher probe failed.' }
    } finally { $process.Dispose() }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path -LiteralPath $output -PathType Leaf) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 25 }
    if (!(Test-Path -LiteralPath $output -PathType Leaf)) { throw 'The beta launcher did not start the owned probe.' }
    $lines = [IO.File]::ReadAllLines($output)
    if ($lines.Length -ne 2 -or $lines[0] -cne $ExpectedDisable -or $lines[1] -cne $data) { throw 'Launcher update isolation or beta data directory did not reach its child process.' }
}

$taskHash = (Get-FileHash -LiteralPath (Join-Path $taskSource 'AudioLibExplorer.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
$taskDevelopmentVersion = '0.8.6-0+local.' + $taskHash.Substring(0, 12)
Invoke-FixtureInstaller -Name 'development' -Extra @('-Development')
Assert-Version $taskDevelopmentVersion
if ([IO.File]::ReadAllText($taskMarker).Trim() -cne $taskDevelopmentVersion) { throw 'The development marker differs from the binary-derived version.' }
if ((Get-FileHash -LiteralPath (Join-Path $taskDestination 'AudioLibExplorer.exe') -Algorithm SHA256).Hash.ToLowerInvariant() -cne $taskHash) { throw 'The installed desktop does not match the version hash.' }
Assert-LauncherEnvironment -Name 'development' -ExpectedDisable '1'
Write-Host 'PASS: development identity uses the executable SHA256; marker and launcher disable updates in isolated beta data.'

[IO.File]::WriteAllText((Join-Path $taskSource 'update-version.txt'), '0.8.6-beta.3')
[IO.File]::WriteAllText((Join-Path $taskSource 'update-channel.txt'), 'beta')
Invoke-FixtureInstaller -Name 'published-whatif' -Extra @('-WhatIf')
Assert-Version $taskDevelopmentVersion
if (!(Test-Path -LiteralPath $taskMarker -PathType Leaf)) { throw '-WhatIf removed the development marker.' }
Invoke-FixtureInstaller -Name 'published-beta'
Assert-Version '0.8.6-beta.3'
if (Test-Path -LiteralPath $taskMarker) { throw 'A published beta retained the development update block.' }
Assert-LauncherEnvironment -Name 'published-beta' -ExpectedDisable ''
Write-Host 'PASS: published beta keeps its exact tag, removes only the development marker, and permits updates; WhatIf is read-only.'

Invoke-FixtureInstaller -Name 'reject-tagged-development' -Extra @('-Development') -Succeeds $false
Assert-Version '0.8.6-beta.3'
if (Test-Path -LiteralPath $taskMarker) { throw 'Rejected tagged development install created a marker.' }
$taskInstalledCli = (Get-FileHash -LiteralPath (Join-Path $taskDestination 'AudioLibCli.exe') -Algorithm SHA256).Hash
[IO.File]::WriteAllText((Join-Path $taskSource 'AudioLibCli.exe'), 'different source for preflight rejection')
[IO.File]::WriteAllText($taskMarker, 'unowned marker content')
Invoke-FixtureInstaller -Name 'reject-unowned-marker' -Succeeds $false
if ([IO.File]::ReadAllText($taskMarker) -cne 'unowned marker content') { throw 'Unowned marker content was removed.' }
if ((Get-FileHash -LiteralPath (Join-Path $taskDestination 'AudioLibCli.exe') -Algorithm SHA256).Hash -cne $taskInstalledCli) { throw 'An invalid marker was rejected only after copying binaries.' }
Remove-Item -LiteralPath $taskMarker
$taskOutside = Join-Path $taskRoot 'owned-outside-install'
New-Item -ItemType Directory -Path $taskOutside | Out-Null
[IO.File]::WriteAllText((Join-Path $taskOutside 'keep.txt'), 'outside untouched')
New-Item -ItemType Junction -Path $taskMarker -Target $taskOutside | Out-Null
try {
    Invoke-FixtureInstaller -Name 'reject-linked-marker' -Succeeds $false
    if ([IO.File]::ReadAllText((Join-Path $taskOutside 'keep.txt')) -cne 'outside untouched') { throw 'A linked cleanup target was followed.' }
    if ((Get-FileHash -LiteralPath (Join-Path $taskDestination 'AudioLibCli.exe') -Algorithm SHA256).Hash -cne $taskInstalledCli) { throw 'A linked marker was rejected only after copying binaries.' }
} finally {
    if (![IO.Path]::GetFullPath($taskMarker).StartsWith($taskRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture junction cleanup escaped its owned root.' }
    [IO.Directory]::Delete($taskMarker)
}
Write-Host 'PASS: tagged development input, unowned marker and linked cleanup target rejected before installation.'
if (!$taskLauncherAvailable) { Write-Host 'Launcher child environment: not-run (Windows .NET Framework compiler unavailable).' }
Write-Host "Beta installer regression passed. Owned evidence: $taskRoot"
