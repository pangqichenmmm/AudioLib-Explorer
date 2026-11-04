# tools/package-macos-client.ps1
# Package a proven native build. Staging/evidence is retained on failure.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^v\d+\.\d+\.\d+(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$')][string]$Tag,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [Parameter(Mandatory)][string]$BuildReport,
    # Release path: validate structure and native startup together, once.
    # Omit only when runtime acceptance must happen later in a logged-in session.
    [switch]$ValidateRuntime,
    [string]$SigningIdentity = '-',
    [string]$OutputDirectory = 'dist/latest'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3' -or !$IsMacOS -or
    [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -ne [Runtime.InteropServices.Architecture]::Arm64) {
    throw 'Package on an Apple Silicon Mac using native arm64 PowerShell 7.3+.'
}
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$taskVersion = ([regex]::Match($Tag, '^v(\d+\.\d+\.\d+)')).Groups[1].Value
function Read-Tool([string]$Tool, [string[]]$Arguments) {
    $text = (& $Tool @Arguments 2>&1 | Out-String).Trim()
    $nativeExitCode = $LASTEXITCODE
    if ($nativeExitCode -ne 0) { throw "Native command failed ($nativeExitCode): $Tool`n$text" }
    return $text
}
function Assert-Unlinked([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked package paths are refused.' }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (!$parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}
function Assert-Relative([string]$Path) {
    if (!$Path -or [IO.Path]::IsPathRooted($Path) -or $Path -match '[\\:\x00-\x1f]' -or
        @($Path.Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count) { throw 'Unsafe package relative path.' }
}
function Assert-Source {
    if ((Read-Tool git @('-C', $taskRoot, 'rev-parse', 'HEAD')) -cne $ExpectedCommit -or
        (Read-Tool git @('-C', $taskRoot, 'status', '--porcelain', '--untracked-files=all'))) {
        throw 'Packaging requires a clean checkout at the exact build commit.'
    }
}
function Add-File([string]$Source, [string]$Relative) {
    Assert-Relative $Relative
    Assert-Unlinked $Source
    if (!(Test-Path -LiteralPath $Source -PathType Leaf) -or !$taskPlanned.Add($Relative.Normalize([Text.NormalizationForm]::FormC))) {
        throw "Missing or duplicate public package source: $Relative"
    }
    $destination = Join-Path $taskBundle $Relative
    Assert-Unlinked $destination
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    Copy-Item -LiteralPath $Source -Destination $destination
}
function Write-File([string]$Relative, [string]$Text) {
    Assert-Relative $Relative
    if (!$taskPlanned.Add($Relative.Normalize([Text.NormalizationForm]::FormC))) { throw 'Duplicate generated package file.' }
    $destination = Join-Path $taskBundle $Relative
    Assert-Unlinked $destination
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    [IO.File]::WriteAllText($destination, ($Text + [char]10), [Text.UTF8Encoding]::new($false))
}
Assert-Unlinked $taskRoot
Assert-Source
$BuildReport = [IO.Path]::GetFullPath($BuildReport, $taskRoot)
Assert-Unlinked $BuildReport
$taskBuild = [IO.File]::ReadAllText($BuildReport) | ConvertFrom-Json
if (!$taskBuild.success -or $taskBuild.commit -cne $ExpectedCommit -or $taskBuild.version -cne $taskVersion -or
    $taskBuild.target -cne 'aarch64-apple-darwin' -or $taskBuild.minimum_macos -cne '11.0' -or $taskBuild.asio -ne $false -or
    $taskBuild.source -cne $taskRoot) { throw 'Native build proof does not match this clean source/tag/target.' }
$taskManifest = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'macos-client-files.json')) | ConvertFrom-Json
if ($taskManifest.bundle_name -cne 'AudioLib Explorer.app' -or $taskManifest.minimum_macos -cne '11.0' -or
    $taskManifest.bundle_identifier -cne 'io.github.freedragonlancer.audiolib-explorer') { throw 'Unexpected Mac bundle contract.' }
$taskCoreAllowlist = @([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'portable-files.json')) | ConvertFrom-Json)
foreach ($mapping in $taskManifest.files) {
    Assert-Relative $mapping.source
    Assert-Relative $mapping.destination
    if ($mapping.source -cne 'MACOS_README.md' -and $mapping.source -cnotin $taskCoreAllowlist) { throw 'Mac resource is outside the shared public allowlist.' }
    if ($mapping.source -eq 'PORTABLE_README.md' -or $mapping.source -match '\.(?:exe|cmd)$') { throw 'Windows launcher/binary in Mac resource manifest.' }
}
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory, $taskRoot)
Assert-Unlinked $OutputDirectory
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$taskArchiveName = "AudioLibExplorer-$Tag-macos-arm64.zip"
$taskReportName = "AudioLibExplorer-$Tag-macos-arm64-report.json"
foreach ($name in @($taskArchiveName, ($taskArchiveName + '.sha256'), $taskReportName)) {
    $path = Join-Path $OutputDirectory $name
    Assert-Unlinked $path
    if (Test-Path -LiteralPath $path) { throw 'Refusing to overwrite a tagged Mac artifact; choose a fresh output directory.' }
}
$taskStage = Join-Path $OutputDirectory ('macos-client/' + [guid]::NewGuid().ToString('N'))
Assert-Unlinked $taskStage
New-Item -ItemType Directory -Path $taskStage | Out-Null
$taskBundle = Join-Path $taskStage $taskManifest.bundle_name
New-Item -ItemType Directory -Path $taskBundle | Out-Null
$taskPlanned = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$taskUnsignedHashes = [ordered]@{}
$taskLock = $null
$taskCopyDisable = $env:COPYFILE_DISABLE
try {
    Assert-Unlinked $taskBuild.target_directory
    $taskLock = [IO.File]::Open((Join-Path $taskBuild.target_directory '.audiolib-macos-client-release.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    foreach ($name in @('AudioLibExplorer', 'AudioLibCli')) {
        $binary = $taskBuild.binaries.$name
        Assert-Unlinked $binary.path
        if ((Get-FileHash -LiteralPath $binary.path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $binary.sha256) { throw 'Build binary changed after proof generation.' }
        Add-File $binary.path ('Contents/MacOS/' + $name)
        $copied = Join-Path $taskBundle ('Contents/MacOS/' + $name)
        if ((Get-FileHash -LiteralPath $copied -Algorithm SHA256).Hash.ToLowerInvariant() -cne $binary.sha256) { throw 'Frozen package binary differs from proven build.' }
        $taskUnsignedHashes[$name] = $binary.sha256
    }
    foreach ($mapping in $taskManifest.files) { Add-File (Join-Path $taskRoot $mapping.source) ('Contents/Resources/' + $mapping.destination) }
    Write-File 'Contents/Resources/update-channel.txt' $(if ($Tag.Contains('-')) { 'beta' } else { 'stable' })
    Write-File 'Contents/Resources/update-version.txt' $Tag.Substring(1)
    $plist = @'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>AudioLibExplorer</string>
<key>CFBundleName</key><string>AudioLib Explorer</string>
<key>CFBundleDisplayName</key><string>AudioLib Explorer</string>
<key>CFBundleIdentifier</key><string>io.github.freedragonlancer.audiolib-explorer</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleIconFile</key><string>AudioLibExplorer.icns</string>
<key>CFBundleShortVersionString</key><string>__VERSION__</string>
<key>CFBundleVersion</key><string>__VERSION__</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>LSApplicationCategoryType</key><string>public.app-category.music</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
'@
    Write-File 'Contents/Info.plist' $plist.Replace('__VERSION__', $taskVersion)
    $null = Read-Tool /usr/bin/plutil @('-lint', (Join-Path $taskBundle 'Contents/Info.plist'))

    # Metadata is private build evidence. Only selected public fields and notice
    # texts are copied; registry/source paths never enter the release manifest.
    Assert-Unlinked $taskBuild.metadata
    $metadata = [IO.File]::ReadAllText($taskBuild.metadata) | ConvertFrom-Json
    $nodes = @{}
    foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
    $included = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $queue = [Collections.Generic.Queue[string]]::new()
    foreach ($name in @('audiolib-desktop', 'audiolib-cli')) {
        $roots = @($metadata.packages | Where-Object { $_.name -ceq $name -and !$_.source })
        if ($roots.Count -ne 1 -or $roots[0].version -cne $taskVersion) { throw 'Desktop/CLI metadata identity differs from tag.' }
        $queue.Enqueue($roots[0].id)
    }
    while ($queue.Count) {
        $id = $queue.Dequeue()
        if (!$included.Add($id)) { continue }
        if (!$nodes.ContainsKey($id)) { throw 'Dependency graph is incomplete.' }
        foreach ($dependency in $nodes[$id].deps) {
            if (@($dependency.dep_kinds | Where-Object { $_.kind -ne 'dev' }).Count) { $queue.Enqueue($dependency.pkg) }
        }
    }
    $packages = @($metadata.packages | Where-Object { $included.Contains($_.id) } | Sort-Object name, version)
    if ($packages.Count -ne $included.Count -or @($packages | Where-Object { $_.name -eq 'asio-sys' }).Count) { throw 'Invalid Mac dependency closure or ASIO dependency.' }
    $notices = [Collections.Generic.List[object]]::new()
    foreach ($package in $packages) {
        if ($package.name -notmatch '^[A-Za-z0-9_-]+$' -or $package.version -notmatch '^[A-Za-z0-9.+_-]+$') { throw 'Unsafe dependency name/version.' }
        $sourceRoot = Split-Path -Parent $package.manifest_path
        Assert-Unlinked $sourceRoot
        $prefix = 'Contents/Resources/licenses/' + $package.name + '-' + $package.version + '/'
        $noticePaths = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($notice in (Get-ChildItem -LiteralPath $sourceRoot -Force | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE)' })) {
            Assert-Unlinked $notice.FullName
            if ($notice.PSIsContainer) {
                if ($notice.Name -match '^(LICENSES?|COPYING|NOTICE)$') {
                    foreach ($file in (Get-ChildItem -LiteralPath $notice.FullName -File -Recurse -Force)) {
                        Assert-Unlinked $file.FullName
                        $relative = [IO.Path]::GetRelativePath($sourceRoot, $file.FullName).Replace('\', '/')
                        if ($noticePaths.Add($relative)) { Add-File $file.FullName ($prefix + $relative) }
                    }
                }
            } elseif ($noticePaths.Add($notice.Name)) { Add-File $notice.FullName ($prefix + $notice.Name) }
        }
        if ($package.license_file) {
            $licenseSource = [IO.Path]::GetFullPath($package.license_file, $sourceRoot)
            if (!$licenseSource.StartsWith($sourceRoot.TrimEnd('/') + '/', [StringComparison]::Ordinal)) { throw 'Dependency license escaped its source tree.' }
            $relative = [IO.Path]::GetFileName($licenseSource)
            if ($noticePaths.Add($relative)) { Add-File $licenseSource ($prefix + $relative) }
        }
        if ($package.name -eq 'soundtouch-ffi') {
            Add-File (Join-Path $sourceRoot 'soundtouch-2_3_2/COPYING.TXT') ($prefix + 'SoundTouch-LGPL.txt')
            $null = $noticePaths.Add('SoundTouch-LGPL.txt')
        }
        $repository = if ($package.repository -and $package.repository -match '^https://[^/@\s?#]+(?:/[^\s?#]*)?$') { $package.repository } else { $null }
        $notices.Add([ordered]@{ name=$package.name; version=$package.version; license=$package.license; repository=$repository; files=@($noticePaths | Sort-Object) })
    }
    Write-File 'Contents/Resources/licenses/dependencies.json' (ConvertTo-Json -InputObject @($notices.ToArray()) -Depth 6)
    foreach ($file in (Get-ChildItem -LiteralPath $taskBundle -File -Recurse -Force)) {
        Assert-Unlinked $file.FullName
        $relative = [IO.Path]::GetRelativePath($taskBundle, $file.FullName).Replace('\', '/')
        if (!$taskPlanned.Contains($relative.Normalize([Text.NormalizationForm]::FormC))) { throw 'Unplanned bundle file.' }
        [IO.File]::SetUnixFileMode($file.FullName, [IO.UnixFileMode]$(if ($relative -in @('Contents/MacOS/AudioLibExplorer', 'Contents/MacOS/AudioLibCli')) { 493 } else { 420 }))
    }
    foreach ($directory in @((Get-Item -LiteralPath $taskBundle)) + @(Get-ChildItem -LiteralPath $taskBundle -Directory -Recurse -Force)) {
        Assert-Unlinked $directory.FullName
        [IO.File]::SetUnixFileMode($directory.FullName, [IO.UnixFileMode]493)
    }
    $signArguments = @('--force', '--sign', $SigningIdentity)
    if ($SigningIdentity -ceq '-') { $signArguments += '--timestamp=none' } else { $signArguments += @('--options', 'runtime', '--timestamp') }
    # Sign the nested CLI before the bundle. Signing the bundle signs its main
    # executable and seals Resources, including the channel/version metadata.
    $null = Read-Tool /usr/bin/codesign ($signArguments + @(Join-Path $taskBundle 'Contents/MacOS/AudioLibCli'))
    $null = Read-Tool /usr/bin/codesign ($signArguments + @($taskBundle))
    $null = Read-Tool /usr/bin/codesign @('--verify', '--deep', '--strict', $taskBundle)
    $null = $taskPlanned.Add('Contents/_CodeSignature/CodeResources')
    foreach ($file in (Get-ChildItem -LiteralPath $taskBundle -File -Recurse -Force)) {
        $relative = [IO.Path]::GetRelativePath($taskBundle, $file.FullName).Replace('\', '/')
        if (!$taskPlanned.Contains($relative.Normalize([Text.NormalizationForm]::FormC))) { throw 'Unexpected codesign output.' }
        [IO.File]::SetUnixFileMode($file.FullName, [IO.UnixFileMode]$(if ($relative -in @('Contents/MacOS/AudioLibExplorer', 'Contents/MacOS/AudioLibCli')) { 493 } else { 420 }))
    }
    foreach ($directory in (Get-ChildItem -LiteralPath $taskBundle -Directory -Recurse -Force)) { [IO.File]::SetUnixFileMode($directory.FullName, [IO.UnixFileMode]493) }
    $taskFiles = @(Get-ChildItem -LiteralPath $taskBundle -File -Recurse -Force | Sort-Object FullName | ForEach-Object {
        [ordered]@{ path=[IO.Path]::GetRelativePath($taskBundle, $_.FullName).Replace('\', '/'); sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); bytes=$_.Length; mode=[int][IO.File]::GetUnixFileMode($_.FullName) }
    })
    $stagedArchive = Join-Path $taskStage $taskArchiveName
    $env:COPYFILE_DISABLE = '1'
    $null = Read-Tool /usr/bin/ditto @('-c', '-k', '--norsrc', '--keepParent', $taskBundle, $stagedArchive)
    $archiveHash = (Get-FileHash -LiteralPath $stagedArchive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText(($stagedArchive + '.sha256'), ($archiveHash + '  ' + $taskArchiveName + [char]10), [Text.UTF8Encoding]::new($false))
    $taskReport = [ordered]@{
        schema_version=1; success=$true; tag=$Tag; version=$taskVersion; commit=$ExpectedCommit; target='aarch64-apple-darwin'
        bundle_name=$taskManifest.bundle_name; bundle_identifier=$taskManifest.bundle_identifier; minimum_macos='11.0'; asio=$false
        signing=$(if ($SigningIdentity -ceq '-') { 'ad-hoc' } else { 'identity' }); notarized=$false; manual_update=$true; runtime_validation='not-run'
        build_binary_sha256=$taskUnsignedHashes; archive_sha256=$archiveHash; files=$taskFiles
    }
    $stagedReport = Join-Path $taskStage $taskReportName
    [IO.File]::WriteAllText($stagedReport, (($taskReport | ConvertTo-Json -Depth 8) + [char]10), [Text.UTF8Encoding]::new($false))
    & (Join-Path $PSScriptRoot 'validate-macos-client.ps1') -Tag $Tag -ExpectedCommit $ExpectedCommit -BundlePath $taskBundle -ArchivePath $stagedArchive -PackageReport $stagedReport -BuildReport $BuildReport -StructureOnly:(!$ValidateRuntime)
    if (!$?) { throw 'Mac package validation failed.' }
    Assert-Source
    # Artifact names are immutable in this output directory. Retain the staged
    # bundle and proof even after success, so a native acceptance run can use it.
    foreach ($name in @($taskArchiveName, ($taskArchiveName + '.sha256'), $taskReportName)) {
        $destination = Join-Path $OutputDirectory $name
        Assert-Unlinked $destination
        if (Test-Path -LiteralPath $destination) { throw 'An artifact appeared while packaging; refusing overwrite.' }
        [IO.File]::Copy((Join-Path $taskStage $name), $destination, $false)
    }
    Write-Host "Mac client package ready: $(Join-Path $OutputDirectory $taskArchiveName)"
    Write-Host "Bundle retained: $taskBundle"
    if ($ValidateRuntime) { Write-Host 'Full native acceptance passed; do not repeat it for these unchanged artifacts.' }
    else { Write-Host 'Runtime acceptance not run; validate-macos-client.ps1 is still required before release.' }
} finally {
    $env:COPYFILE_DISABLE = $taskCopyDisable
    if ($taskLock) { $taskLock.Dispose() }
}
