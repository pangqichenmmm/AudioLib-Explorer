# tools/package-portable.ps1
[CmdletBinding()]
param([switch]$SkipBuild, [switch]$WithoutAsio, [string]$Tag)
$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$workspaceText = Get-Content -LiteralPath (Join-Path $taskRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($workspaceText, '(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*"(\d+\.\d+\.\d+)"')
if (!$versionMatch.Success) { throw 'Cannot identify the portable workspace version.' }
$packageVersion = $versionMatch.Groups[1].Value
if (!$Tag) { $Tag = 'v' + $packageVersion }
$tagMatch = [regex]::Match($Tag, '^v(\d+\.\d+\.\d+)(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$')
if (!$tagMatch.Success -or $tagMatch.Groups[1].Value -cne $packageVersion) { throw 'Portable tag must match the workspace version (vX.Y.Z, vX.Y.Z-rc.N or vX.Y.Z-beta.N).' }
$updateChannel = if ($Tag.Contains('-')) { 'beta' } else { 'stable' }
function Assert-WorkspacePath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    if ($cursor -ne $taskRoot -and !$cursor.StartsWith($taskRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Package path escaped workspace.' }
    # A plain leaf can still sit below a junction. Check every existing ancestor
    # inside the workspace before copying or replacing generated output.
    while ($true) {
        if ((Test-Path -LiteralPath $cursor) -and ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked package path is not supported.' }
        if ($cursor -eq $taskRoot) { break }
        $cursor = Split-Path -Parent $cursor
    }
}
if (!$SkipBuild) { & (Join-Path $PSScriptRoot 'build-release.ps1') -WithoutAsio:$WithoutAsio }
$latestRoot = [IO.Path]::GetFullPath((Join-Path $taskRoot 'dist/latest'))
$outputRoot = [IO.Path]::GetFullPath((Join-Path $latestRoot 'portable'))
if (!$outputRoot.StartsWith($taskRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Portable path escaped the intended workspace.'
}
# Cargo writes to CARGO_TARGET_DIR when it is set (e.g. a local SSD outside a network
# share); fall back to <repo>/target otherwise.
$targetRoot = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { $env:CARGO_TARGET_DIR } else { Join-Path $taskRoot $env:CARGO_TARGET_DIR })) } else { Join-Path $taskRoot 'target' }
$binaryRoot = Join-Path $targetRoot 'x86_64-pc-windows-msvc/release'
foreach ($binary in @('AudioLibExplorer.exe', 'AudioLibCli.exe')) {
    if (!(Test-Path -LiteralPath (Join-Path $binaryRoot $binary))) { throw "Missing release executable: $binary" }
}
# Build in a fresh owned staging directory. Reusing a previous payload would
# retain obsolete reports, screenshots or local files even after source removal.
Assert-WorkspacePath $latestRoot
New-Item -ItemType Directory -Force -Path $latestRoot | Out-Null
if ((Get-Item -LiteralPath $latestRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Linked package output is not supported.' }
$portableRoot = [IO.Path]::GetFullPath((Join-Path $latestRoot ('.portable-stage-' + [guid]::NewGuid().ToString('N'))))
if (!$portableRoot.StartsWith($latestRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Staging path escaped package output.' }
try {
New-Item -ItemType Directory -Force -Path $portableRoot | Out-Null
foreach ($binary in @('AudioLibExplorer.exe', 'AudioLibCli.exe')) {
    Copy-Item -LiteralPath (Join-Path $binaryRoot $binary) -Destination (Join-Path $portableRoot $binary) -Force
}
foreach ($file in (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'portable-files.json') -Raw | ConvertFrom-Json)) {
    $source = [IO.Path]::GetFullPath((Join-Path $taskRoot $file))
    $destination = [IO.Path]::GetFullPath((Join-Path $portableRoot $file))
    if (!$source.StartsWith($taskRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
        !$destination.StartsWith($portableRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Manifest path escaped package boundary.' }
    Assert-WorkspacePath $source
    Assert-WorkspacePath $destination
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
    Copy-Item -LiteralPath $source -Destination $destination -Force
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'Start-AudioLibExplorer.cmd') -Destination $portableRoot -Force
Set-Content -LiteralPath (Join-Path $portableRoot 'update-channel.txt') -Value $updateChannel -Encoding ASCII
Set-Content -LiteralPath (Join-Path $portableRoot 'update-version.txt') -Value $Tag.Substring(1) -Encoding ASCII
$licenseRoot = Join-Path $portableRoot 'licenses'
New-Item -ItemType Directory -Force -Path $licenseRoot | Out-Null
Push-Location -LiteralPath $taskRoot
try {
    $metadata = cargo metadata --format-version 1 --locked | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Dependency metadata generation failed.' }
    $manifest = @()
    foreach ($package in $metadata.packages) {
        $manifest += [PSCustomObject]@{ name=$package.name; version=$package.version; license=$package.license; repository=$package.repository }
        $sourceRoot = Split-Path -Parent $package.manifest_path
        if ($package.name -eq 'soundtouch-ffi') {
            Copy-Item -LiteralPath (Join-Path $sourceRoot 'soundtouch-2_3_2/COPYING.TXT') -Destination (Join-Path $licenseRoot 'SoundTouch-2.3.2-LGPL.txt') -Force
        }
        $notices = Get-ChildItem -LiteralPath $sourceRoot -File | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE)' }
        if ($notices) {
            $destination = Join-Path $licenseRoot ($package.name + '-' + $package.version)
            New-Item -ItemType Directory -Force -Path $destination | Out-Null
            $notices | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $destination -Force }
        }
        $noticeDirectories = Get-ChildItem -LiteralPath $sourceRoot -Directory | Where-Object { $_.Name -match '^(LICENSES?|COPYING|NOTICE)$' }
        foreach ($noticeDirectory in $noticeDirectories) {
            $destination = Join-Path $licenseRoot ($package.name + '-' + $package.version)
            New-Item -ItemType Directory -Force -Path $destination | Out-Null
            Copy-Item -LiteralPath $noticeDirectory.FullName -Destination $destination -Recurse -Force
        }
        if ($package.license_file -and (Test-Path -LiteralPath $package.license_file -PathType Leaf)) {
            $destination = Join-Path $licenseRoot ($package.name + '-' + $package.version)
            New-Item -ItemType Directory -Force -Path $destination | Out-Null
            Copy-Item -LiteralPath $package.license_file -Destination $destination -Force
        }
    }
    $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $licenseRoot 'dependencies.json') -Encoding UTF8
} finally { Pop-Location }
if (!$WithoutAsio) {
    Copy-Item -LiteralPath (Join-Path $taskRoot 'tools/.build-tools/asio-2.3.4/ASIOSDK/LICENSE.txt') -Destination (Join-Path $licenseRoot 'Steinberg-ASIO-SDK-2.3.4.txt') -Force
}
& (Join-Path $PSScriptRoot 'validate-portable-licenses.ps1') -BinaryDirectory $portableRoot -WithoutAsio:$WithoutAsio
# Inspect actual staged names against the policy embedded in old clients before
# replacing the last verified package, even if today's manifest accepts them.
$compatibilityEntries = @(Get-ChildItem -LiteralPath $portableRoot -Recurse -Force -File | ForEach-Object {
    $_.FullName.Substring($portableRoot.Length).TrimStart('\', '/').Replace('\', '/')
})
& (Join-Path $PSScriptRoot 'validate-update-compatibility.ps1') -Product Desktop -Entries $compatibilityEntries
& (Join-Path $PSScriptRoot 'validate-package-privacy.ps1') -BinaryDirectory $portableRoot
# Replace only the verified generated output, never local_release or runtime data.
if (Test-Path -LiteralPath $outputRoot) {
    Assert-WorkspacePath $outputRoot
    if ((Get-Item -LiteralPath $outputRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint -or
        @(Get-ChildItem -LiteralPath $outputRoot -Recurse -Force -Attributes ReparsePoint).Count -gt 0) { throw 'Linked previous package output is not supported.' }
    Remove-Item -LiteralPath $outputRoot -Recurse -Force
}
Move-Item -LiteralPath $portableRoot -Destination $outputRoot
$portableRoot = $outputRoot
# `<exe>.sha256` sidecars (`<lowercase hex>  <file name>`, sha256sum style) sit next to the
# executables for install-beta.ps1 / beta-smoke.ps1. They are not ZIP entries: the updater
# accepts only the portable-files.json payload, so older clients would reject them.
$sidecarNames = @('AudioLibExplorer.exe.sha256', 'AudioLibCli.exe.sha256')
foreach ($binary in @('AudioLibExplorer.exe', 'AudioLibCli.exe')) {
    $sidecar = Join-Path $portableRoot ($binary + '.sha256')
    Assert-WorkspacePath $sidecar
    $binaryHash = (Get-FileHash -LiteralPath (Join-Path $portableRoot $binary) -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($sidecar, ($binaryHash + '  ' + $binary + "`n"), [Text.Encoding]::ASCII)
}
$archivePath = Join-Path $latestRoot 'AudioLibExplorer-latest-win-x64-portable.zip'
Assert-WorkspacePath $archivePath
Assert-WorkspacePath ($archivePath + '.sha256')
# Cargo archives may give notices pre-1980 timestamps, which ZIP cannot represent.
# Adjust only the copied package files; registry and project sources stay untouched.
$zipEpoch = [datetime]::new(1980, 1, 1)
Get-ChildItem -LiteralPath $portableRoot -Recurse -Force -File |
    Where-Object { $_.LastWriteTime -lt $zipEpoch } |
    ForEach-Object { $_.LastWriteTime = $zipEpoch }
# Write entries one by one with forward-slash names: Windows PowerShell 5's
# Compress-Archive stores `\` separators, which the updater rejects as non-public.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
if (Test-Path -LiteralPath $archivePath) { Remove-Item -LiteralPath $archivePath -Force }
$packageFiles = @(Get-ChildItem -LiteralPath $portableRoot -Recurse -Force -File | Where-Object { $_.DirectoryName -ne $portableRoot -or $sidecarNames -notcontains $_.Name } | Sort-Object FullName)
$plannedEntries = @($packageFiles | ForEach-Object { $_.FullName.Substring($portableRoot.Length).TrimStart('\', '/').Replace('\', '/') })
$zip = [IO.Compression.ZipFile]::Open($archivePath, [IO.Compression.ZipArchiveMode]::Create)
try {
    for ($index = 0; $index -lt $packageFiles.Count; $index++) {
        $entry = $zip.CreateEntry($plannedEntries[$index], [IO.Compression.CompressionLevel]::Optimal)
        $entry.LastWriteTime = [DateTimeOffset]$packageFiles[$index].LastWriteTime
        $source = [IO.File]::OpenRead($packageFiles[$index].FullName)
        try { $target = $entry.Open(); try { $source.CopyTo($target) } finally { $target.Dispose() } } finally { $source.Dispose() }
    }
} finally { $zip.Dispose() }
& (Join-Path $PSScriptRoot 'validate-portable-zip.ps1') -Archive $archivePath -ExpectedEntries $plannedEntries
$hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
Set-Content -LiteralPath ($archivePath + '.sha256') -Value ($hash + '  ' + [IO.Path]::GetFileName($archivePath)) -Encoding ASCII
Write-Host "Portable package ready: $archivePath"
} finally {
    # A failed build may leave a stage, but never delete the published payload here.
    if ($portableRoot -ne $outputRoot -and (Test-Path -LiteralPath $portableRoot)) {
        if (!$portableRoot.StartsWith($latestRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
            (Split-Path -Leaf $portableRoot) -notmatch '^\.portable-stage-[0-9a-f]{32}$') { throw 'Unexpected staging cleanup path.' }
        Assert-WorkspacePath $portableRoot
        if ((Get-Item -LiteralPath $portableRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint -or
            @(Get-ChildItem -LiteralPath $portableRoot -Recurse -Force -Attributes ReparsePoint).Count -gt 0) { throw 'Linked staging cleanup path.' }
        Remove-Item -LiteralPath $portableRoot -Recurse -Force
    }
}

