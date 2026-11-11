# tools/build-cache.ps1
# Shared build/cleanup coordination. Compatible with Windows PowerShell 5.1.
function Get-AudioLibCanonicalPath([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT -and $full -match '^[A-Za-z]:\\') {
        $drive = Get-PSDrive -Name $full.Substring(0,1) -ErrorAction SilentlyContinue
        if ($drive -and $drive.DisplayRoot) { $full = [IO.Path]::GetFullPath((Join-Path $drive.DisplayRoot $full.Substring(3))) }
    }
    if ($full.Length -gt [IO.Path]::GetPathRoot($full).Length) { $full = $full.TrimEnd([IO.Path]::DirectorySeparatorChar) }
    return $full
}
function Assert-AudioLibUnlinkedPath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $entry = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($entry -and ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked build/cache path is not supported.' }
        $cursor = [IO.Path]::GetDirectoryName($cursor)
    }
}
function Get-AudioLibBuildCommonDirectory([string]$Root) {
    $savedExitVariable = Get-Variable LASTEXITCODE -Scope Global -ErrorAction SilentlyContinue
    $savedExit = if ($savedExitVariable) { $savedExitVariable.Value } else { $null }
    $savedPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $common = & git -C $Root rev-parse --git-common-dir 2>$null
        if ($LASTEXITCODE -eq 0 -and $common) {
            if (![IO.Path]::IsPathRooted($common)) { $common = Join-Path $Root $common }
            return Get-AudioLibCanonicalPath $common
        }
        return Get-AudioLibCanonicalPath (Join-Path $Root '.rust-validation')
    } finally { $global:LASTEXITCODE = $savedExit; $ErrorActionPreference = $savedPreference }
}
function Enter-AudioLibBuildLease([string]$Root, [switch]$Exclusive) {
    $common = Get-AudioLibBuildCommonDirectory $Root
    Assert-AudioLibUnlinkedPath $common
    $null = [IO.Directory]::CreateDirectory($common)
    $path = Join-Path $common 'audiolib-build-cache.lock'
    Assert-AudioLibUnlinkedPath $path
    $share = if ($Exclusive) { [IO.FileShare]::None } else { [IO.FileShare]::ReadWrite }
    return [IO.File]::Open($path,[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,$share)
}
