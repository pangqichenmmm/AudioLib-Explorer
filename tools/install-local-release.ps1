# tools/install-local-release.ps1
# Install an already accepted portable payload into the fixed local_release/.
# Package files only; existing databases/preferences and unrelated files survive.
#Requires -Version 7.3
[CmdletBinding()]
param([string]$Source = 'dist/latest/portable')
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (!$IsWindows) { throw 'Local Windows installation requires Windows.' }
. (Join-Path $PSScriptRoot 'native-desktop-window.ps1')
if (!('AudioLibLocalInstallNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class AudioLibLocalInstallNative {
    [DllImport("user32.dll", EntryPoint = "PostMessageW", SetLastError = true)]
    public static extern bool PostMessage(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);
}
'@
}
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$taskSource = [IO.Path]::GetFullPath($Source, $taskRoot).TrimEnd('\')
$taskDestination = Join-Path $taskRoot 'local_release'
if ($taskSource -ieq $taskDestination -or $taskSource.StartsWith($taskDestination+'\',[StringComparison]::OrdinalIgnoreCase) -or $taskDestination.StartsWith($taskSource+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Source must not overlap the fixed installation.' }
$taskEvidence = Join-Path $taskRoot ('.rust-validation/local-install/' + [guid]::NewGuid().ToString('N'))
$taskDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
function Assert-Directory([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor -and $taskDirectories.Add($cursor)) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked installation paths are not supported.' }
        $cursor = [IO.Path]::GetDirectoryName($cursor)
    }
}
Assert-Directory $taskSource
Assert-Directory $taskDestination
Assert-Directory $taskEvidence
. (Join-Path $PSScriptRoot 'assert-portable-layout.ps1')
$taskFiles = @(Get-AudioLibPortableFiles $taskSource)
$taskManifest = [IO.File]::ReadAllText((Join-Path $taskRoot 'Cargo.toml'))
$taskExpectedVersion = [regex]::Match($taskManifest, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"').Groups[1].Value
$taskPackageVersion = ([IO.File]::ReadAllText((Join-Path $taskSource 'update-version.txt'))).Trim() -replace '-(?:rc|beta)\.\d+$',''
if (!$taskExpectedVersion -or $taskPackageVersion -cne $taskExpectedVersion) { throw 'Accepted package version must match the checkout before installation.' }
$taskLock = $null
$taskSavedData = $env:AUDIOLIB_DATA_DIR
$taskStreams = @()
try {
New-Item -ItemType Directory -Path (Join-Path $taskRoot '.rust-validation') -Force | Out-Null
$taskLockPath = Join-Path $taskRoot '.rust-validation/local-release-install.lock'
Assert-Directory $taskLockPath
$taskLock = [IO.File]::Open($taskLockPath,[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
# Validate every destination before closing an application or copying anything.
$taskCopyPlan = @($taskFiles | ForEach-Object {
    $taskRelative = [IO.Path]::GetRelativePath($taskSource,$_.FullName)
    $taskTarget = [IO.Path]::GetFullPath((Join-Path $taskDestination $taskRelative))
    if (!$taskTarget.StartsWith($taskDestination+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Installation destination escaped local_release.' }
    Assert-Directory ([IO.Path]::GetDirectoryName($taskTarget))
    $taskExisting = Get-Item -LiteralPath $taskTarget -Force -ErrorAction SilentlyContinue
    if ($taskExisting -and ($taskExisting.PSIsContainer -or ($taskExisting.Attributes -band [IO.FileAttributes]::ReparsePoint))) { throw 'Invalid package-owned destination.' }
    [pscustomobject]@{source=$_.FullName;relative=$taskRelative;target=$taskTarget;expected=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash}
})
foreach ($taskProcess in @(Get-Process -Name AudioLibExplorer,AudioLibCli -ErrorAction SilentlyContinue)) {
    if ($taskProcess.Path -and $taskProcess.Path.StartsWith($taskDestination + '\', [StringComparison]::OrdinalIgnoreCase)) {
        $taskHandle = [AudioLibDesktopWindowSelector]::FindApplicationWindow($taskProcess.Id)
        if ($taskHandle -eq [IntPtr]::Zero) {
            $taskProcess.Refresh()
            if ($taskProcess.HasExited) { continue }
            throw 'Installed process has no visible application window; leave it running and close it gracefully before retrying.'
        }
        [AudioLibDesktopWindowSelector]::AssertApplicationWindow($taskHandle,$taskProcess.Id)
        if (![AudioLibLocalInstallNative]::PostMessage($taskHandle,0x0010,[IntPtr]::Zero,[IntPtr]::Zero) -or !$taskProcess.WaitForExit(30000)) { throw 'Installed application is still running/draining writes; close it gracefully before retrying.' }
    }
}
New-Item -ItemType Directory -Path $taskEvidence -Force | Out-Null
$taskRecords = [Collections.Generic.List[object]]::new()
$taskCopied = 0
foreach ($taskFile in $taskCopyPlan) {
    $taskRelative = $taskFile.relative
    $taskTarget = $taskFile.target
    $taskParent = [IO.Path]::GetDirectoryName($taskTarget)
    Assert-Directory $taskParent
    $taskExisting = Get-Item -LiteralPath $taskTarget -Force -ErrorAction SilentlyContinue
    if ($taskExisting -and ($taskExisting.PSIsContainer -or ($taskExisting.Attributes -band [IO.FileAttributes]::ReparsePoint))) { throw 'Invalid package-owned destination.' }
    $taskExpected = $taskFile.expected
    $taskActual = if ($taskExisting) { (Get-FileHash -LiteralPath $taskTarget -Algorithm SHA256).Hash } else { '' }
    if ($taskActual -cne $taskExpected) {
        New-Item -ItemType Directory -Path $taskParent -Force | Out-Null
        Copy-Item -LiteralPath $taskFile.source -Destination $taskTarget -Force
        $taskActual = (Get-FileHash -LiteralPath $taskTarget -Algorithm SHA256).Hash
        $taskCopied++
    }
    if ($taskActual -cne $taskExpected) { throw "Installed file identity failed: $taskRelative" }
    $taskRecords.Add(@{file=$taskRelative;sha256=$taskActual.ToLowerInvariant()})
}
$taskRecords | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $taskEvidence 'files.json') -Encoding utf8
$taskTemporary = Join-Path ([IO.Path]::GetTempPath()) ('AudioLibLocalInstall-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $taskTemporary | Out-Null
    $env:AUDIOLIB_DATA_DIR = $taskTemporary
    @{active_database=(Join-Path $taskTemporary 'library.db');language='cn';playback=@{volume=0.0};allowed_scan_roots=@()} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $taskTemporary 'settings.json') -Encoding utf8
    $taskVersion = & (Join-Path $taskDestination 'AudioLibCli.exe') --version
    if ($LASTEXITCODE -ne 0 -or $taskVersion.Trim() -cne ('AudioLibCli ' + $taskExpectedVersion)) { throw 'Installed CLI version mismatch.' }
    $taskStart = [Diagnostics.ProcessStartInfo]::new()
    $taskStart.FileName = Join-Path $taskDestination 'AudioLibExplorer.exe'
    $taskStart.UseShellExecute=$false; $taskStart.CreateNoWindow=$true; $taskStart.RedirectStandardOutput=$true; $taskStart.RedirectStandardError=$true
    foreach ($taskArgument in @('--smoke-ui','--smoke-language','cn','--screenshot',(Join-Path $taskEvidence 'installed.png'))) { $taskStart.ArgumentList.Add($taskArgument) }
    $taskStreams = @([IO.File]::Create((Join-Path $taskEvidence 'startup.stdout.log')), [IO.File]::Create((Join-Path $taskEvidence 'startup.stderr.log')))
    $taskDesktop = [Diagnostics.Process]::Start($taskStart)
    $taskCopies = @($taskDesktop.StandardOutput.BaseStream.CopyToAsync($taskStreams[0]), $taskDesktop.StandardError.BaseStream.CopyToAsync($taskStreams[1]))
    if (!$taskDesktop.WaitForExit(45000)) { throw 'Owned installed startup did not finish; no process was forcibly stopped.' }
    foreach ($taskCopy in $taskCopies) { $null = $taskCopy.GetAwaiter().GetResult() }
    if ($taskDesktop.ExitCode -ne 0 -or !(Test-Path -LiteralPath (Join-Path $taskEvidence 'installed.png'))) { throw 'Installed desktop startup failed.' }
    $taskDesktop.Dispose()
    $taskResult = @{success=$true;version=$taskVersion.Trim();installation=$taskDestination;verified_files=$taskRecords.Count;copied_files=$taskCopied;startup='passed';evidence=$taskEvidence}
    $taskResult | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskEvidence 'report.json') -Encoding utf8
    $taskResult | ConvertTo-Json -Compress | Write-Output
} finally {
    foreach ($taskStream in $taskStreams) { $taskStream.Dispose() }
    $env:AUDIOLIB_DATA_DIR = $taskSavedData
    if ($taskLock) { $taskLock.Dispose() }
}
