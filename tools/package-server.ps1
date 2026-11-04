# tools/package-server.ps1
# Package an already-built server on its native host. No server/database is opened:
# the executable is invoked only with --version. Requires PowerShell 7.3 or newer.
# ./tools/package-server.ps1 -Tag v0.4.5 -Target x86_64-pc-windows-msvc -Asio
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'aarch64-apple-darwin')]
    [string]$Target,
    # A directory containing the target binary, or the binary itself.
    [string]$BinaryPath,
    # Match the Windows workspace release build's unified ASIO features.
    [switch]$Asio
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Server packaging requires PowerShell 7.3 or newer.' }
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$pathComparison = if ($IsWindows) { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
$tagMatch = [regex]::Match($Tag, '^v(\d+\.\d+\.\d+)(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$')
if (!$tagMatch.Success) { throw 'Tag must be vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-rc.N / -beta.N.' }
$version = $tagMatch.Groups[1].Value
if ($Asio -and ($Target -cne 'x86_64-pc-windows-msvc' -or !$IsWindows)) { throw '-Asio requires the native Windows server target.' }
$taskProcessArchitecture = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
switch ($Target) {
    'x86_64-pc-windows-msvc' {
        if (!$IsWindows) { throw 'The Windows server must be packaged on Windows.' }
        $platformName = 'win-x64'; $binaryName = 'audiolib-server.exe'; $archiveExtension = 'zip'
    }
    'x86_64-unknown-linux-gnu' {
        if (!$IsLinux -or $taskProcessArchitecture -ne [Runtime.InteropServices.Architecture]::X64) { throw 'The Linux x64 server must be packaged on Linux x64.' }
        $platformName = 'linux-x64'; $binaryName = 'audiolib-server'; $archiveExtension = 'tar.gz'
    }
    'aarch64-unknown-linux-gnu' {
        if (!$IsLinux -or $taskProcessArchitecture -ne [Runtime.InteropServices.Architecture]::Arm64) { throw 'The Linux arm64 server must be packaged on Linux arm64.' }
        $platformName = 'linux-arm64'; $binaryName = 'audiolib-server'; $archiveExtension = 'tar.gz'
    }
    'aarch64-apple-darwin' {
        if (!$IsMacOS) { throw 'The macOS server must be packaged on macOS.' }
        $platformName = 'macos-arm64'; $binaryName = 'audiolib-server'; $archiveExtension = 'tar.gz'
    }
}

function Assert-Within([string]$Path, [string]$Parent) {
    $full = [IO.Path]::GetFullPath($Path)
    $parentFull = [IO.Path]::GetFullPath($Parent).TrimEnd([IO.Path]::DirectorySeparatorChar)
    if (!$full.StartsWith($parentFull + [IO.Path]::DirectorySeparatorChar, $pathComparison)) { throw 'Server package path escaped its owned boundary.' }
}
function Assert-Unlinked([string]$Path) {
    # A safe leaf may have a linked ancestor. Inspect up to the filesystem root,
    # including hidden paths, before opening or replacing any source/output.
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked server package path is not supported.' }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (!$parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}
function Assert-UnlinkedTree([string]$Path) {
    Assert-Unlinked $Path
    if (Test-Path -LiteralPath $Path -PathType Container) {
        if (@(Get-ChildItem -LiteralPath $Path -Recurse -Force -Attributes ReparsePoint).Count) { throw 'Linked server package tree is not supported.' }
    }
}
function Assert-Relative([string]$Path) {
    if (!$Path -or [IO.Path]::IsPathRooted($Path) -or $Path -match '[\\:\x00-\x1f]' -or
        @($Path.Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count) { throw 'Unsafe relative server package file.' }
}
function Copy-PublicFile([string]$Source, [string]$Relative) {
    Assert-Relative $Relative
    Assert-Unlinked $Source
    if (!(Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Required server package file is missing: $Relative" }
    $destination = [IO.Path]::GetFullPath((Join-Path $payloadRoot $Relative))
    Assert-Within $destination $payloadRoot
    Assert-Unlinked $destination
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
    Copy-Item -LiteralPath $Source -Destination $destination -Force
    if (!$plannedFiles.Add($Relative)) { throw "Duplicate server package file: $Relative" }
}
function Copy-LicenseTree([string]$Source, [string]$Relative) {
    Assert-UnlinkedTree $Source
    foreach ($file in (Get-ChildItem -LiteralPath $Source -File -Recurse -Force)) {
        $tail = [IO.Path]::GetRelativePath($Source, $file.FullName).Replace('\', '/')
        Copy-PublicFile $file.FullName ($Relative + '/' + $tail)
    }
}

# Packaging consumes the SDK already prepared by the release build. It never
# downloads or extracts an implicit SDK, and a modified extracted notice is refused.
$sdkLicenseSource = Join-Path $taskRoot 'tools/.build-tools/asio-2.3.4/ASIOSDK/LICENSE.txt'
$sdkLicenseRelative = 'licenses/Steinberg-ASIO-SDK-2.3.4.txt'
if ($Asio) {
    $sdkArchive = Join-Path $taskRoot 'tools/.build-tools/ASIO-SDK_2.3.4_2025-10-15.zip'
    foreach ($sdkPath in @($sdkArchive, $sdkLicenseSource)) {
        Assert-Within $sdkPath $taskRoot
        Assert-Unlinked $sdkPath
        if (!(Test-Path -LiteralPath $sdkPath -PathType Leaf)) { throw 'The prepared ASIO SDK archive/license is missing; run prepare-asio.ps1 before packaging.' }
    }
    $expectedSdkHash = 'D5EBF0C20DD2C5F43771FD0C1418F4B361BF52434EE670097CFA6B3A335E2ECA'
    if ((Get-FileHash -LiteralPath $sdkArchive -Algorithm SHA256).Hash -cne $expectedSdkHash) { throw 'Prepared ASIO SDK archive checksum mismatch.' }
    $sdkZip = [IO.Compression.ZipFile]::OpenRead($sdkArchive)
    try {
        $notices = @($sdkZip.Entries | Where-Object { $_.FullName -ceq 'ASIOSDK/LICENSE.txt' })
        if ($notices.Count -ne 1) { throw 'Pinned ASIO SDK archive license entry is missing or duplicated.' }
        $noticeStream = $notices[0].Open()
        try { $sdkLicenseHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($noticeStream)) }
        finally { $noticeStream.Dispose() }
    } finally { $sdkZip.Dispose() }
    if ((Get-FileHash -LiteralPath $sdkLicenseSource -Algorithm SHA256).Hash -cne $sdkLicenseHash) { throw 'Prepared ASIO SDK license differs from the pinned archive.' }
}

Assert-Unlinked $taskRoot
$manifestPath = Join-Path $PSScriptRoot 'server-files.json'
Assert-Unlinked $manifestPath
$publicFiles = @((Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json) | ForEach-Object { $_ })
if (!$publicFiles.Count -or @($publicFiles | Where-Object { $_ -isnot [string] }).Count) { throw 'Server file allowlist must be a nonempty string array.' }
$publicSet = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($relative in $publicFiles) {
    Assert-Relative $relative
    if (!$publicSet.Add($relative)) { throw 'Server allowlist contains duplicate files.' }
    Assert-Within (Join-Path $taskRoot $relative) $taskRoot
    Assert-Unlinked (Join-Path $taskRoot $relative)
}
if (!$BinaryPath) {
    # Match Cargo and package-portable.ps1: honor CARGO_TARGET_DIR when it is set.
    $targetRoot = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR, $taskRoot) } else { Join-Path $taskRoot 'target' }
    $BinaryPath = Join-Path $targetRoot "$Target/release"
}
$binarySource = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($BinaryPath)) { $BinaryPath } else { Join-Path $taskRoot $BinaryPath }))
Assert-Unlinked $binarySource
if (Test-Path -LiteralPath $binarySource -PathType Container) { $binarySource = Join-Path $binarySource $binaryName }
Assert-Unlinked $binarySource
if (!(Test-Path -LiteralPath $binarySource -PathType Leaf) -or [IO.Path]::GetFileName($binarySource) -cne $binaryName) { throw 'The expected target server binary is missing.' }
$versionText = (& $binarySource --version 2>$null | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionText -cne "audiolib-server $version") { throw 'Server executable version does not match the release tag.' }
$binaryHash = (Get-FileHash -LiteralPath $binarySource -Algorithm SHA256).Hash

# Capture raw dependency metadata in memory only: it contains local manifest paths.
# Resolve normal/build dependency edges from the server, excluding dev-only edges.
Push-Location -LiteralPath $taskRoot
try {
    $metadataArguments = @('metadata', '--format-version', '1', '--locked', '--filter-platform', $Target)
    if ($Asio) { $metadataArguments += @('--features', 'audiolib-desktop/asio,audiolib-cli/asio') }
    $metadataText = (& cargo @metadataArguments 2>$null | Out-String)
    if ($LASTEXITCODE -ne 0) { throw 'Server dependency metadata generation failed.' }
    $metadata = $metadataText | ConvertFrom-Json
    $commit = (& git -c ('safe.directory=' + $taskRoot.Replace('\', '/')) rev-parse HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Cannot obtain server package source commit.' }
} finally { Pop-Location }
$serverPackages = @($metadata.packages | Where-Object { $_.name -eq 'audiolib-server' -and $_.source -eq $null })
if ($serverPackages.Count -ne 1 -or $serverPackages[0].version -cne $version -or $null -eq $metadata.resolve) { throw 'Server dependency/version identity is invalid.' }
$nodes = @{}
foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
$included = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$queue = [Collections.Generic.Queue[string]]::new()
$queue.Enqueue($serverPackages[0].id)
while ($queue.Count) {
    $id = $queue.Dequeue()
    if (!$included.Add($id)) { continue }
    if (!$nodes.ContainsKey($id)) { throw 'Server dependency graph is incomplete.' }
    foreach ($dependency in $nodes[$id].deps) {
        if (@($dependency.dep_kinds | Where-Object { $_.kind -ne 'dev' }).Count) { $queue.Enqueue($dependency.pkg) }
    }
}
$packages = @($metadata.packages | Where-Object { $included.Contains($_.id) } | Sort-Object name, version)
if ($packages.Count -ne $included.Count) { throw 'Server dependency package list is incomplete.' }
$hasAsio = @($packages | Where-Object { $_.name -ceq 'asio-sys' }).Count -gt 0
if ($hasAsio -ne [bool]$Asio) { throw 'Server ASIO dependency closure differs from the selected -Asio mode.' }

$latestRoot = [IO.Path]::GetFullPath((Join-Path $taskRoot 'dist/latest'))
$outputRoot = [IO.Path]::GetFullPath((Join-Path $latestRoot "server-$platformName"))
Assert-Within $latestRoot $taskRoot
Assert-Unlinked $latestRoot
New-Item -ItemType Directory -Force -Path $latestRoot | Out-Null
$stageRoot = [IO.Path]::GetFullPath((Join-Path $latestRoot ('.server-stage-' + [guid]::NewGuid().ToString('N'))))
Assert-Within $stageRoot $latestRoot
Assert-Unlinked $stageRoot
$payloadRoot = Join-Path $stageRoot 'payload'
$plannedFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$archiveName = "AudioLibServer-$Tag-$platformName.$archiveExtension"
$reportName = "AudioLibServer-$Tag-$platformName-report.json"
$stageOwned = $false
try {
    New-Item -ItemType Directory -Path $stageRoot | Out-Null
    $stageOwned = $true
    New-Item -ItemType Directory -Path $payloadRoot | Out-Null
    Copy-PublicFile $binarySource $binaryName
    $updateChannel = if ($Tag.Contains('-')) { 'beta' } else { 'stable' }
    Set-Content -LiteralPath (Join-Path $payloadRoot 'update-channel.txt') -Value $updateChannel -Encoding ascii
    Set-Content -LiteralPath (Join-Path $payloadRoot 'update-version.txt') -Value $Tag.Substring(1) -Encoding ascii
    $null = $plannedFiles.Add('update-channel.txt')
    $null = $plannedFiles.Add('update-version.txt')
    foreach ($relative in $publicFiles) { Copy-PublicFile (Join-Path $taskRoot $relative) $relative }
    if ($Asio) {
        Copy-PublicFile $sdkLicenseSource $sdkLicenseRelative
        if ((Get-FileHash -LiteralPath (Join-Path $payloadRoot $sdkLicenseRelative) -Algorithm SHA256).Hash -cne $sdkLicenseHash) { throw 'Packaged ASIO SDK notice identity changed.' }
    }
    $licenseManifest = @()
    foreach ($package in $packages) {
        if ($package.name -notmatch '^[A-Za-z0-9_-]+$' -or $package.version -notmatch '^[A-Za-z0-9.+_-]+$') { throw 'Unsafe dependency identity.' }
        $repository = if ($package.repository -and $package.repository -match '^https?://[^/@\s]+(?:/[^\s]*)?$') { $package.repository } else { $null }
        $licenseManifest += [pscustomobject][ordered]@{ name=$package.name; version=$package.version; license=$package.license; repository=$repository }
        $sourceRoot = Split-Path -Parent $package.manifest_path
        Assert-Unlinked $sourceRoot
        $destination = 'licenses/' + $package.name + '-' + $package.version
        $copiedNotices = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($notice in (Get-ChildItem -LiteralPath $sourceRoot -Force | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE)' })) {
            if ($notice.PSIsContainer) {
                if ($notice.Name -match '^(LICENSES?|COPYING|NOTICE)$') { Copy-LicenseTree $notice.FullName ($destination + '/' + $notice.Name) }
            } else {
                Copy-PublicFile $notice.FullName ($destination + '/' + $notice.Name)
                $null = $copiedNotices.Add($notice.FullName)
            }
        }
        if ($package.license_file) {
            $licenseSource = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($package.license_file)) { $package.license_file } else { Join-Path $sourceRoot $package.license_file }))
            Assert-Within $licenseSource $sourceRoot
            if (!$copiedNotices.Contains($licenseSource)) { Copy-PublicFile $licenseSource ($destination + '/' + [IO.Path]::GetFileName($licenseSource)) }
        }
        # This upstream crate stores its combined-work notice below the source tree.
        if ($package.name -eq 'soundtouch-ffi') { Copy-PublicFile (Join-Path $sourceRoot 'soundtouch-2_3_2/COPYING.TXT') ($destination + '/SoundTouch-LGPL.txt') }
    }
    $dependencyPath = Join-Path $payloadRoot 'licenses/dependencies.json'
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dependencyPath) | Out-Null
    ConvertTo-Json -InputObject @($licenseManifest) -Depth 5 | Set-Content -LiteralPath $dependencyPath -Encoding utf8NoBOM
    $null = $plannedFiles.Add('licenses/dependencies.json')

    # Check both known machine prefixes and exact payload names before archiving.
    $privatePrefixes = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($prefix in @($taskRoot, $env:USERPROFILE, $env:HOME, $env:CARGO_HOME, $env:RUSTUP_HOME, $env:CARGO_TARGET_DIR)) {
        if ($prefix) {
            $full = [IO.Path]::GetFullPath($prefix).TrimEnd('\', '/')
            $null = $privatePrefixes.Add($full); $null = $privatePrefixes.Add($full.Replace('\', '/'))
        }
    }
    if ($IsWindows) {
        $drive = Get-PSDrive -Name ([IO.Path]::GetPathRoot($taskRoot).TrimEnd(':', '\')) -ErrorAction SilentlyContinue
        if ($drive -and $drive.DisplayRoot) { $null = $privatePrefixes.Add($drive.DisplayRoot.TrimEnd('\') + '\' + $taskRoot.Substring(3)) }
    }
    Assert-UnlinkedTree $payloadRoot
    $payloadFiles = @(Get-ChildItem -LiteralPath $payloadRoot -File -Recurse -Force)
    if ($payloadFiles.Count -ne $plannedFiles.Count) { throw 'Server package file count differs from its allowlist.' }
    $compatibilityEntries = @($payloadFiles | ForEach-Object { [IO.Path]::GetRelativePath($payloadRoot, $_.FullName).Replace('\', '/') })
    & (Join-Path $PSScriptRoot 'validate-update-compatibility.ps1') -Product Server -ServerBinaryName $binaryName -Entries $compatibilityEntries
    foreach ($file in $payloadFiles) {
        $relative = [IO.Path]::GetRelativePath($payloadRoot, $file.FullName).Replace('\', '/')
        if (!$plannedFiles.Contains($relative) -or $file.Extension -in @('.db', '.sqlite', '.sqlite3', '.log', '.wav') -or
            $file.Name -in @('settings.json', 'admin-settings.json', 'tokens.json', '.env') -or
            $file.Name -match '(?:-wal|-shm|\.server-state\.json)$' -or $relative -match '(?:^|/)(?:logs?|cache|recovery)/') { throw "Non-public server package file: $relative" }
        $bytes = [IO.File]::ReadAllBytes($file.FullName)
        $texts = @([Text.Encoding]::UTF8.GetString($bytes))
        if ($relative -eq $binaryName) {
            $texts += [Text.Encoding]::Unicode.GetString($bytes)
            if ($bytes.Length -gt 1) { $texts += [Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1) }
        }
        foreach ($text in $texts) {
            foreach ($prefix in $privatePrefixes) {
                if ($text.IndexOf($prefix, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
                    $text.IndexOf($prefix.Replace('\', '\\'), [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "Local build/user path found in server payload: $relative" }
            }
        }
        if (!$IsWindows) {
            # Normalize public file permissions: no private mode/owner metadata.
            $mode = if ($relative -in @($binaryName, 'server/deploy/redeploy-server.sh')) { 493 } else { 420 }
            [IO.File]::SetUnixFileMode($file.FullName, [IO.UnixFileMode]$mode)
        } elseif ($file.LastWriteTime -lt [datetime]::new(1980, 1, 1)) {
            $file.LastWriteTime = [datetime]::new(1980, 1, 1)
        }
    }
    if (!$IsWindows) {
        foreach ($directory in @((Get-Item -LiteralPath $payloadRoot)) + @(Get-ChildItem -LiteralPath $payloadRoot -Directory -Recurse -Force)) {
            [IO.File]::SetUnixFileMode($directory.FullName, [IO.UnixFileMode]493)
        }
    }
    if ((Get-FileHash -LiteralPath (Join-Path $payloadRoot $binaryName)).Hash -cne $binaryHash) { throw 'Copied server binary identity changed.' }
    $stagedArchive = Join-Path $stageRoot $archiveName
    if ($IsWindows) {
        [IO.Compression.ZipFile]::CreateFromDirectory($payloadRoot, $stagedArchive, [IO.Compression.CompressionLevel]::Optimal, $false)
        $zip = [IO.Compression.ZipFile]::OpenRead($stagedArchive)
        try {
            $entries = @($zip.Entries | Where-Object { !$_.FullName.EndsWith('/') } | ForEach-Object { $_.FullName })
            if ($entries.Count -ne $plannedFiles.Count -or @($entries | Where-Object { !$plannedFiles.Contains($_) }).Count) { throw 'Server ZIP contents differ from its allowlist.' }
        } finally { $zip.Dispose() }
    } else {
        $tarVersion = (& tar --version 2>$null | Out-String)
        if ($LASTEXITCODE -ne 0) { throw 'Native tar is unavailable.' }
        $ownerOptions = if ($tarVersion -match 'GNU tar') { @('--owner=0', '--group=0', '--numeric-owner') } elseif ($tarVersion -match 'bsdtar') { @('--uid', '0', '--gid', '0', '--uname', 'root', '--gname', 'root') } else { throw 'Unsupported native tar implementation.' }
        & tar @ownerOptions --format=pax -czf $stagedArchive -C $payloadRoot .
        if ($LASTEXITCODE -ne 0) { throw 'Native server tar creation failed.' }
        $entries = @(& tar -tzf $stagedArchive)
        if ($LASTEXITCODE -ne 0) { throw 'Native server tar verification failed.' }
        $entries = @($entries | Where-Object { !$_.EndsWith('/') } | ForEach-Object { $_ -replace '^\./', '' })
        if ($entries.Count -ne $plannedFiles.Count -or @($entries | Where-Object { !$plannedFiles.Contains($_) }).Count) { throw 'Server tar contents differ from its allowlist.' }
    }
    $archiveHash = (Get-FileHash -LiteralPath $stagedArchive -Algorithm SHA256).Hash
    Set-Content -LiteralPath ($stagedArchive + '.sha256') -Value ($archiveHash + '  ' + $archiveName) -Encoding ascii
    # Fixed public schema; never serialize process errors, metadata paths or runtime state.
    $report = [ordered]@{
        success=$true
        version=$version; tag=$Tag; commit=$commit; target=$Target
        binary_sha256=$binaryHash; archive_sha256=$archiveHash
        checks=[ordered]@{ version=$true; allowlist=$true; unlinked_paths=$true; privacy=$true; archive_contents=$true; executable_mode=$true }
    }
    $stagedReport = Join-Path $stageRoot $reportName
    $report | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $stagedReport -Encoding utf8NoBOM

    # Only our generated server output is replaced, after the payload/archive pass.
    Assert-Within $outputRoot $latestRoot
    Assert-UnlinkedTree $outputRoot
    foreach ($name in @($archiveName, ($archiveName + '.sha256'), $reportName)) {
        $artifact = Join-Path $latestRoot $name
        Assert-Unlinked $artifact
        if (Test-Path -LiteralPath $artifact -PathType Container) { throw 'A server archive/report output must be a regular file.' }
    }
    if (Test-Path -LiteralPath $outputRoot) { Remove-Item -LiteralPath $outputRoot -Recurse -Force }
    Move-Item -LiteralPath $payloadRoot -Destination $outputRoot
    foreach ($name in @($archiveName, ($archiveName + '.sha256'), $reportName)) { Move-Item -LiteralPath (Join-Path $stageRoot $name) -Destination (Join-Path $latestRoot $name) -Force }
    Write-Host "Server package ready: $archiveName"
} finally {
    if ($stageOwned -and (Test-Path -LiteralPath $stageRoot)) {
        Assert-Within $stageRoot $latestRoot
        if ((Split-Path -Leaf $stageRoot) -notmatch '^\.server-stage-[0-9a-f]{32}$') { throw 'Unexpected server staging cleanup path.' }
        Assert-UnlinkedTree $stageRoot
        Remove-Item -LiteralPath $stageRoot -Recurse -Force
    }
}
