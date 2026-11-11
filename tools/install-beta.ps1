# tools/install-beta.ps1
# Install a validated portable package as the side-by-side beta build.
#   ./tools/install-beta.ps1 [-Source dist/latest/portable] [-Destination beta_release] [-Development] [-WhatIf]
# Relative paths resolve against the repository root. The beta installation is
# launched through AudioLibExplorer-beta.cmd, which sets AUDIOLIB_DATA_DIR to a
# separate directory (default %LOCALAPPDATA%\AudioLibExplorer\beta) so a beta
# never shares settings, database, caches or logs with the stable v1 data root.
# Only package-owned files are overwritten. A published install also removes the
# installer's development marker; no running process is stopped.
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$Source = 'dist/latest/portable',
    [string]$Destination = 'beta_release',
    # Local snapshots have a binary-derived identity and cannot self-update.
    [switch]$Development
)
$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
function Resolve-RepoPath([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) { [IO.Path]::GetFullPath($Path) } else { [IO.Path]::GetFullPath((Join-Path $taskRoot $Path)) }
}
function Assert-UnlinkedPath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked beta installation path is not supported.' }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (!$parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}
$sourceRoot = (Resolve-RepoPath $Source).TrimEnd('\', '/')
$destinationRoot = (Resolve-RepoPath $Destination).TrimEnd('\', '/')
Assert-UnlinkedPath $sourceRoot
Assert-UnlinkedPath $destinationRoot
if (!(Test-Path -LiteralPath (Join-Path $sourceRoot 'AudioLibExplorer.exe') -PathType Leaf)) { throw "No portable package at $sourceRoot (run package-portable.ps1 first)." }
if (!(Test-Path -LiteralPath (Join-Path $sourceRoot 'update-version.txt') -PathType Leaf)) { throw 'The beta source needs update-version.txt; repackage with package-portable.ps1 -Tag <release-tag>.' }
$updateVersion = (Get-Content -LiteralPath (Join-Path $sourceRoot 'update-version.txt') -Raw).Trim()
if ($updateVersion -cnotmatch '^\d+\.\d+\.\d+(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$') { throw 'The beta source has an invalid update version.' }
if ($Development) {
    if ($updateVersion.Contains('-')) { throw '-Development requires an untagged local package; preserve the full release tag for a published candidate.' }
    $developmentHash = (Get-FileHash -LiteralPath (Join-Path $sourceRoot 'AudioLibExplorer.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    $updateVersion += '-0+local.' + $developmentHash.Substring(0, 12)
}
$leaf = [IO.Path]::GetFileName($destinationRoot)
if ($leaf -ieq 'local_release' -or $destinationRoot -ieq $taskRoot -or $destinationRoot -ieq $sourceRoot) { throw 'Beta destination must be a dedicated directory, not the stable installation, repository root or package source.' }
if ((Test-Path -LiteralPath $destinationRoot) -and ((Get-Item -LiteralPath $destinationRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked beta destination is not supported.' }
$running = @(Get-Process -Name AudioLibExplorer, AudioLibCli -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($destinationRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) })
if ($running.Count -gt 0) { throw 'The beta build is running; close it before installing (processes are never stopped by this script).' }

& (Join-Path $PSScriptRoot 'validate-package-privacy.ps1') -BinaryDirectory $sourceRoot
$files = @(Get-ChildItem -LiteralPath $sourceRoot -Recurse -File)
foreach ($file in $files) {
    $relative = $file.FullName.Substring($sourceRoot.Length + 1)
    $target = [IO.Path]::GetFullPath((Join-Path $destinationRoot $relative))
    if (!$target.StartsWith($destinationRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Beta installation target escaped its owned directory.' }
    Assert-UnlinkedPath $target
}
foreach ($name in @('AudioLibExplorer-beta.cmd', 'update-channel.txt', 'update-version.txt', 'update-development.txt')) { Assert-UnlinkedPath (Join-Path $destinationRoot $name) }
$developmentMarker = Join-Path $destinationRoot 'update-development.txt'
$previousDevelopmentVersion = $null
if (Test-Path -LiteralPath $developmentMarker) {
    if (!(Test-Path -LiteralPath $developmentMarker -PathType Leaf)) { throw 'The beta development marker must be a regular file.' }
    $previousDevelopmentVersion = (Get-Content -LiteralPath $developmentMarker -Raw).Trim()
    if ($previousDevelopmentVersion -cnotmatch '^\d+\.\d+\.\d+-0\+local\.[0-9a-f]{12}$') { throw 'The beta development marker is not owned by this installer; preserve it for inspection.' }
}
if ($PSCmdlet.ShouldProcess($destinationRoot, "Install $($files.Count) package files and AudioLibExplorer-beta.cmd")) {
    New-Item -ItemType Directory -Force -Path $destinationRoot | Out-Null
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($sourceRoot.Length + 1)
        $target = Join-Path $destinationRoot $relative
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
        Copy-Item -LiteralPath $file.FullName -Destination $target -Force
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'AudioLibExplorer-beta.cmd') -Destination $destinationRoot -Force
    # Published packages retain their exact version. Development identities are
    # installed here only and must never enter a published package's allowlist.
    Set-Content -LiteralPath (Join-Path $destinationRoot 'update-channel.txt') -Value 'beta' -Encoding ASCII
    foreach ($binary in @('AudioLibExplorer.exe', 'AudioLibCli.exe')) {
        $expected = (Get-FileHash -LiteralPath (Join-Path $sourceRoot $binary) -Algorithm SHA256).Hash
        $actual = (Get-FileHash -LiteralPath (Join-Path $destinationRoot $binary) -Algorithm SHA256).Hash
        if ($expected -ne $actual) { throw "Installed $binary does not match the package." }
        if ($Development -and $binary -eq 'AudioLibExplorer.exe' -and $expected -ine $developmentHash) { throw 'The development source executable changed during installation.' }
    }
    if ($Development) {
        Set-Content -LiteralPath $developmentMarker -Value $updateVersion -Encoding ASCII
        Set-Content -LiteralPath (Join-Path $destinationRoot 'update-version.txt') -Value $updateVersion -Encoding ASCII
    } elseif ($null -ne $previousDevelopmentVersion) {
        # Recheck the single owned cleanup target after copying. Never recursively
        # delete, follow a linked marker, or remove a concurrently replaced file.
        Assert-UnlinkedPath $developmentMarker
        if (!(Test-Path -LiteralPath $developmentMarker -PathType Leaf) -or (Get-Content -LiteralPath $developmentMarker -Raw).Trim() -cne $previousDevelopmentVersion) { throw 'The beta development marker changed during installation.' }
        Remove-Item -LiteralPath $developmentMarker -Force
    }
    Write-Host "Beta installed to $destinationRoot ($($files.Count) files). Launch with AudioLibExplorer-beta.cmd."
}
