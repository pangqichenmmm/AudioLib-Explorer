# tools/validate-macos-client.ps1
# Native package acceptance uses only owned synthetic data. All evidence is kept.
[CmdletBinding()]
param(
    [string]$Tag,
    [string]$ExpectedCommit,
    [string]$BundlePath,
    [string]$ArchivePath,
    [string]$PackageReport,
    [string]$BuildReport,
    [switch]$StructureOnly,
    [switch]$TestBoundaries
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Use PowerShell 7.3 or newer.' }
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
function Assert-Relative([string]$Path) {
    if (!$Path -or [IO.Path]::IsPathRooted($Path) -or $Path -match '[\\:\x00-\x1f]' -or
        @($Path.Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count) { throw 'Unsafe Mac package relative path.' }
    if ($Path -match '(?:^|/)(?:__MACOSX|\.DS_Store|\._[^/]*)(?:/|$)') { throw 'Finder/resource-fork metadata is not a public payload.' }
}
function Path-Key([string]$Path) { return $Path.Normalize([Text.NormalizationForm]::FormC) }
function Assert-Unlinked([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked package/evidence paths are refused.' }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (!$parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}
function Assert-Tree([string]$Path) {
    Assert-Unlinked $Path
    if (@(Get-ChildItem -LiteralPath $Path -Force -Recurse -Attributes ReparsePoint).Count) { throw 'Linked package tree is refused.' }
}
function Assert-PublicPath([string]$Path) {
    Assert-Relative $Path
    $name = [IO.Path]::GetFileName($Path)
    if ($Path -match '\.(?:db|sqlite|sqlite3|log|wav|exe|cmd|dll|pdb|dSYM)$' -or
        $name -in @('settings.json', 'admin-settings.json', 'tokens.json', '.env') -or
        $name -match '(?:-wal|-shm|\.server-state\.json)$' -or
        $Path -match '(?:^|/)(?:logs?|cache|recovery|target|\.git)/') { throw 'Private/runtime/Windows file in Mac payload.' }
}
function Assert-Zip([string]$Path, [string]$RootName, [Collections.Generic.Dictionary[string, object]]$Expected) {
    $directories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $null = $directories.Add((Path-Key $RootName))
    foreach ($relative in $Expected.Keys) {
        $cursor = $RootName + '/' + $relative
        while ($cursor.Contains('/')) {
            $cursor = $cursor.Substring(0, $cursor.LastIndexOf('/'))
            $null = $directories.Add((Path-Key $cursor))
        }
    }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $seenFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $zip = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        foreach ($entry in $zip.Entries) {
            $isDirectory = $entry.FullName.EndsWith('/')
            $name = if ($isDirectory) { $entry.FullName.Substring(0, $entry.FullName.Length - 1) } else { $entry.FullName }
            Assert-Relative $name
            if (!$seen.Add((Path-Key $name))) { throw 'Duplicate/case/Unicode-normalization collision in ZIP.' }
            $rawMode = ($entry.ExternalAttributes -shr 16) -band 65535
            $kind = $rawMode -band 61440
            $permissions = $rawMode -band 4095
            if ($isDirectory) {
                $canonicalDirectory = $null
                if ($kind -ne 16384 -or $permissions -ne 493 -or !$directories.TryGetValue((Path-Key $name), [ref]$canonicalDirectory) -or
                    (Path-Key $name) -cne $canonicalDirectory -or $entry.Length -ne 0) { throw 'Invalid/unplanned ZIP directory mode/path.' }
                continue
            }
            if ($kind -ne 32768 -or !$name.StartsWith($RootName + '/', [StringComparison]::Ordinal)) { throw 'ZIP contains a link/special file or escaped its sole app root.' }
            $relative = $name.Substring($RootName.Length + 1)
            Assert-PublicPath $relative
            $key = Path-Key $relative
            if (!$Expected.ContainsKey($key) -or !$seenFiles.Add($key)) { throw 'ZIP file is outside its exact allowlist.' }
            $wanted = $Expected[$key]
            if ($key -cne (Path-Key $wanted.path) -or $permissions -ne $wanted.mode -or $entry.Length -ne $wanted.bytes) { throw 'ZIP file path/size/permissions differ from package proof.' }
            $stream = $entry.Open()
            try { $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)).ToLowerInvariant() }
            finally { $stream.Dispose() }
            if ($hash -cne $wanted.sha256) { throw 'ZIP file checksum differs from package proof.' }
        }
        if ($seenFiles.Count -ne $Expected.Count) { throw 'ZIP is missing planned files.' }
    } finally { $zip.Dispose() }
}
if ($TestBoundaries) {
    # These adversarial archives exercise the same validator used for releases;
    # no native executable, SDK, library, server or Unix chmod is involved.
    $testRoot = Join-Path ([IO.Path]::GetTempPath()) ('AudioLibMacBoundary-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $expected = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::OrdinalIgnoreCase)
    $bytes = [Text.Encoding]::UTF8.GetBytes('owned synthetic boundary fixture')
    $expected['Contents/Resources/README.md'] = [pscustomobject]@{ path='Contents/Resources/README.md'; mode=420; bytes=$bytes.Length; sha256=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant() }
    $cases = @(
        @{name='valid'; paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=33188; pass=$true},
        @{name='valid-directories'; directories=@('AudioLib Explorer.app/','AudioLib Explorer.app/Contents/','AudioLib Explorer.app/Contents/Resources/'); paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=33188; pass=$true},
        @{name='wrong-directory-case'; directories=@('audiolib explorer.app/'); paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=33188; pass=$false},
        @{name='double-directory-slash'; directories=@('AudioLib Explorer.app//'); paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=33188; pass=$false},
        @{name='traversal'; paths=@('AudioLib Explorer.app/../README.md'); mode=33188; pass=$false},
        @{name='backslash'; paths=@('AudioLib Explorer.app\Contents\Resources\README.md'); mode=33188; pass=$false},
        @{name='absolute'; paths=@('/AudioLib Explorer.app/Contents/Resources/README.md'); mode=33188; pass=$false},
        @{name='symlink'; paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=41471; pass=$false},
        @{name='world-writable'; paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=33206; pass=$false},
        @{name='setuid'; paths=@('AudioLib Explorer.app/Contents/Resources/README.md'); mode=35236; pass=$false},
        @{name='duplicate-case'; paths=@('AudioLib Explorer.app/Contents/Resources/README.md','AudioLib Explorer.app/Contents/Resources/readme.md'); mode=33188; pass=$false},
        @{name='wrong-case'; paths=@('AudioLib Explorer.app/Contents/Resources/readme.md'); mode=33188; pass=$false},
        @{name='colon'; paths=@('AudioLib Explorer.app/Contents/Resources/file:stream'); mode=33188; pass=$false},
        @{name='empty-segment'; paths=@('AudioLib Explorer.app//Contents/Resources/README.md'); mode=33188; pass=$false},
        @{name='finder'; paths=@('__MACOSX/._AudioLib Explorer.app'); mode=33188; pass=$false},
        @{name='private-db'; paths=@('AudioLib Explorer.app/Contents/Resources/library.db'); mode=33188; pass=$false},
        @{name='extra'; paths=@('AudioLib Explorer.app/Contents/Resources/README.md','AudioLib Explorer.app/Contents/Resources/extra.txt'); mode=33188; pass=$false}
    )
    foreach ($case in $cases) {
        $archive = Join-Path $testRoot ($case.name + '.zip')
        $zip = [IO.Compression.ZipFile]::Open($archive, [IO.Compression.ZipArchiveMode]::Create)
        try {
            if ($case.ContainsKey('directories')) {
                foreach ($directory in $case.directories) {
                    $entry = $zip.CreateEntry($directory)
                    $entry.ExternalAttributes = [int](16877 -shl 16)
                }
            }
            foreach ($path in $case.paths) {
                $entry = $zip.CreateEntry($path)
                $entry.ExternalAttributes = [int]($case.mode -shl 16)
                $stream = $entry.Open()
                try { $stream.Write($bytes) } finally { $stream.Dispose() }
            }
        } finally { $zip.Dispose() }
        $passed = $true
        try { Assert-Zip $archive 'AudioLib Explorer.app' $expected } catch { $passed = $false }
        if ($passed -ne $case.pass) { throw "Unexpected synthetic boundary result: $($case.name)" }
    }
    $unicode = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $null = $unicode.Add((Path-Key ('caf' + [char]233)))
    if ($unicode.Add((Path-Key ('cafe' + [char]769)))) { throw 'Canonical Unicode collision was not detected.' }
    $unicodeExpected = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::OrdinalIgnoreCase)
    $unicodePath = 'Contents/Resources/caf' + [char]233 + '.md'
    $decomposedPath = 'Contents/Resources/cafe' + [char]769 + '.md'
    $unicodeExpected[(Path-Key $unicodePath)] = [pscustomobject]@{path=$unicodePath;mode=420;bytes=$bytes.Length;sha256=$expected['Contents/Resources/README.md'].sha256}
    foreach ($duplicate in @($false, $true)) {
        $archive = Join-Path $testRoot $(if ($duplicate) { 'unicode-duplicate.zip' } else { 'unicode-normalized.zip' })
        $zip = [IO.Compression.ZipFile]::Open($archive, [IO.Compression.ZipArchiveMode]::Create)
        try {
            $paths = @($decomposedPath)
            if ($duplicate) { $paths += $unicodePath }
            foreach ($path in $paths) {
                $entry = $zip.CreateEntry('AudioLib Explorer.app/' + $path)
                $entry.ExternalAttributes = [int](33188 -shl 16)
                $stream = $entry.Open()
                try { $stream.Write($bytes) } finally { $stream.Dispose() }
            }
        } finally { $zip.Dispose() }
        $passed = $true
        try { Assert-Zip $archive 'AudioLib Explorer.app' $unicodeExpected } catch { $passed = $false }
        if ($passed -eq $duplicate) { throw 'Unexpected canonical Unicode ZIP result.' }
    }
    [IO.File]::WriteAllText((Join-Path $testRoot 'boundary-report.json'), (([ordered]@{success=$true;zip_cases=($cases.Count + 2);unicode_collision=$true;native_execution=$false} | ConvertTo-Json) + [char]10), [Text.UTF8Encoding]::new($false))
    Write-Host "Mac ZIP boundary checks passed ($($cases.Count + 2) archives, including Unicode normalization/collision). Evidence retained: $testRoot"
    return
}
if (!$IsMacOS -or [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -ne [Runtime.InteropServices.Architecture]::Arm64) {
    throw 'Native Mac acceptance requires arm64 PowerShell on Apple Silicon.'
}
if ($Tag -cnotmatch '^v(\d+\.\d+\.\d+)(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$' -or $ExpectedCommit -cnotmatch '^[0-9a-f]{40}$' -or
    !$BundlePath -or !$ArchivePath -or !$PackageReport) { throw 'Supply explicit tag, exact commit, app bundle, ZIP and package report.' }
$taskVersion = ([regex]::Match($Tag, '^v(\d+\.\d+\.\d+)')).Groups[1].Value
function Read-Tool([string]$Tool, [string[]]$Arguments) {
    $result = (& $Tool @Arguments 2>&1 | Out-String).Trim()
    $nativeExitCode = $LASTEXITCODE
    if ($nativeExitCode -ne 0) { throw "Native validation command failed ($nativeExitCode): $Tool`n$result" }
    return $result
}
$BundlePath = [IO.Path]::GetFullPath($BundlePath, $taskRoot)
$ArchivePath = [IO.Path]::GetFullPath($ArchivePath, $taskRoot)
$PackageReport = [IO.Path]::GetFullPath($PackageReport, $taskRoot)
foreach ($path in @($BundlePath, $ArchivePath, $PackageReport, ($ArchivePath + '.sha256'))) { Assert-Unlinked $path }
Assert-Tree $BundlePath
$taskManifest = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'macos-client-files.json')) | ConvertFrom-Json
$taskReport = [IO.File]::ReadAllText($PackageReport) | ConvertFrom-Json
$taskPublicKeys = @('schema_version','success','tag','version','commit','target','bundle_name','bundle_identifier','minimum_macos','asio','signing','notarized','manual_update','runtime_validation','build_binary_sha256','archive_sha256','files')
if (@($taskReport.PSObject.Properties.Name).Count -ne $taskPublicKeys.Count -or
    @($taskReport.PSObject.Properties.Name | Where-Object { $_ -cnotin $taskPublicKeys }).Count) { throw 'Unexpected public package report schema.' }
if ($taskReport.schema_version -ne 1 -or !$taskReport.success -or $taskReport.tag -cne $Tag -or $taskReport.version -cne $taskVersion -or
    $taskReport.commit -cne $ExpectedCommit -or $taskReport.target -cne 'aarch64-apple-darwin' -or $taskReport.minimum_macos -cne '11.0' -or
    $taskReport.bundle_name -cne 'AudioLib Explorer.app' -or $taskReport.bundle_identifier -cne 'io.github.freedragonlancer.audiolib-explorer' -or
    $taskReport.asio -ne $false -or $taskReport.signing -cnotin @('ad-hoc','identity') -or $taskReport.notarized -ne $false -or
    $taskReport.manual_update -ne $true -or $taskReport.runtime_validation -cne 'not-run') { throw 'Mac package report identity/status is invalid.' }
if (@($taskReport.build_binary_sha256.PSObject.Properties.Name).Count -ne 2 -or
    @($taskReport.build_binary_sha256.PSObject.Properties.Name | Where-Object { $_ -cnotin @('AudioLibExplorer','AudioLibCli') }).Count) { throw 'Unexpected unsigned binary proof schema.' }
foreach ($name in @('AudioLibExplorer','AudioLibCli')) {
    if ($taskReport.build_binary_sha256.$name -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid unsigned binary proof checksum.' }
}
if ([IO.Path]::GetFileName($BundlePath) -cne $taskReport.bundle_name -or
    [IO.Path]::GetFileName($ArchivePath) -cne "AudioLibExplorer-$Tag-macos-arm64.zip") { throw 'Unexpected Mac bundle/archive name.' }
$taskPrivatePrefixes = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($prefix in @($taskRoot, $env:HOME, $env:USERPROFILE, $env:CARGO_HOME, $env:RUSTUP_HOME, $env:CARGO_TARGET_DIR)) {
    if ($prefix) { $null = $taskPrivatePrefixes.Add([IO.Path]::GetFullPath($prefix, $taskRoot).TrimEnd('/', '\')) }
}
if ($BuildReport) {
    $BuildReport = [IO.Path]::GetFullPath($BuildReport, $taskRoot)
    Assert-Unlinked $BuildReport
    $build = [IO.File]::ReadAllText($BuildReport) | ConvertFrom-Json
    if (!$build.success -or $build.commit -cne $ExpectedCommit -or $build.version -cne $taskVersion -or $build.target -cne 'aarch64-apple-darwin') { throw 'Private native build proof identity mismatch.' }
    foreach ($prefix in $build.private_prefixes) { if ($prefix) { $null = $taskPrivatePrefixes.Add($prefix) } }
    foreach ($name in @('AudioLibExplorer', 'AudioLibCli')) {
        if ($taskReport.build_binary_sha256.$name -cne $build.binaries.$name.sha256) { throw 'Unsigned build identity differs from package report.' }
    }
}
function Assert-Privacy([byte[]]$Bytes, [string]$Relative) {
    $texts = @([Text.Encoding]::UTF8.GetString($Bytes))
    if ($Relative -in @('Contents/MacOS/AudioLibExplorer', 'Contents/MacOS/AudioLibCli')) {
        $texts += [Text.Encoding]::Unicode.GetString($Bytes)
        if ($Bytes.Length -gt 1) { $texts += [Text.Encoding]::Unicode.GetString($Bytes, 1, $Bytes.Length - 1) }
    }
    foreach ($text in $texts) {
        foreach ($prefix in $taskPrivatePrefixes) {
            foreach ($variant in @($prefix, $prefix.Replace('\','/'), $prefix.Replace('\','\\'))) {
                if ($text.IndexOf($variant, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "Private build/user prefix in payload: $Relative" }
            }
        }
        if ($text -match '(?<!\d)18086(?!\d)|192\.168\.\d{1,3}\.\d{1,3}(?![\d/])' -or
            ($Relative -notlike 'Contents/Resources/licenses/*' -and $text -match '\bPaul\b')) { throw "Private deployment fixture in payload: $Relative" }
    }
}
$taskExpected = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::OrdinalIgnoreCase)
$taskUncompressedBytes = [long]0
foreach ($file in $taskReport.files) {
    if (@($file.PSObject.Properties.Name).Count -ne 4 -or @($file.PSObject.Properties.Name | Where-Object { $_ -cnotin @('path','sha256','bytes','mode') }).Count) { throw 'Unexpected package file record schema.' }
    Assert-PublicPath $file.path
    $wantedMode = if ($file.path -cin @('Contents/MacOS/AudioLibExplorer', 'Contents/MacOS/AudioLibCli')) { 493 } else { 420 }
    if ($file.sha256 -cnotmatch '^[0-9a-f]{64}$' -or $file.bytes -lt 0 -or $file.bytes -gt 1GB -or $file.mode -ne $wantedMode -or !$taskExpected.TryAdd((Path-Key $file.path), $file)) { throw 'Invalid/duplicate file proof or unsafe mode.' }
    $taskUncompressedBytes += $file.bytes
}
if ($taskExpected.Count -gt 20000 -or $taskUncompressedBytes -gt 2GB) { throw 'Mac package exceeds the explicit extraction budget.' }
$taskAllowed = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($relative in @('Contents/Info.plist','Contents/MacOS/AudioLibExplorer','Contents/MacOS/AudioLibCli','Contents/Resources/update-channel.txt','Contents/Resources/update-version.txt','Contents/_CodeSignature/CodeResources','Contents/Resources/licenses/dependencies.json')) { $null = $taskAllowed.Add((Path-Key $relative)) }
$coreAllowlist = @([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'portable-files.json')) | ConvertFrom-Json)
foreach ($mapping in $taskManifest.files) {
    Assert-Relative $mapping.source
    Assert-Relative $mapping.destination
    if ($mapping.source -cne 'MACOS_README.md' -and $mapping.source -cnotin $coreAllowlist) { throw 'Mac resource is outside the common public manifest.' }
    if (!$taskAllowed.Add((Path-Key ('Contents/Resources/' + $mapping.destination)))) { throw 'Duplicate resource destination.' }
}
$taskNoticesPath = Join-Path $BundlePath 'Contents/Resources/licenses/dependencies.json'
$taskNotices = @([IO.File]::ReadAllText($taskNoticesPath) | ConvertFrom-Json)
$noticeIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($notice in $taskNotices) {
    if (@($notice.PSObject.Properties.Name).Count -ne 5 -or @($notice.PSObject.Properties.Name | Where-Object { $_ -cnotin @('name','version','license','repository','files') }).Count -or
        $notice.name -notmatch '^[A-Za-z0-9_-]+$' -or $notice.version -notmatch '^[A-Za-z0-9.+_-]+$' -or
        $notice.name -ceq 'asio-sys' -or !$noticeIds.Add($notice.name + '-' + $notice.version) -or
        ($notice.repository -and $notice.repository -notmatch '^https://[^/@\s?#]+(?:/[^\s?#]*)?$')) { throw 'Invalid public dependency metadata.' }
    foreach ($relative in $notice.files) {
        Assert-Relative $relative
        if ($relative -notmatch '^(?:LICENSE[^/]*|COPYING[^/]*|NOTICE[^/]*|SoundTouch-LGPL\.txt)(?:/.*)?$') { throw 'Dependency payload is not a public license/notice.' }
        $path = 'Contents/Resources/licenses/' + $notice.name + '-' + $notice.version + '/' + $relative
        Assert-PublicPath $path
        if (!$taskAllowed.Add((Path-Key $path))) { throw 'Duplicate dependency notice.' }
    }
}
if ($taskAllowed.Count -ne $taskExpected.Count) { throw 'File report differs from the explicit resource/license allowlist.' }
foreach ($key in $taskExpected.Keys) {
    $canonicalPath = $null
    if (!$taskAllowed.TryGetValue($key, [ref]$canonicalPath) -or $key -cne $canonicalPath) { throw 'File report differs from the exact resource/license paths.' }
}
$soundNotices = @($taskNotices | Where-Object { $_.name -ceq 'soundtouch-ffi' -and 'SoundTouch-LGPL.txt' -cin $_.files })
if (!$noticeIds.Contains('slint-1.17.1') -or !$noticeIds.Contains('soundtouch-0.5.4') -or $soundNotices.Count -ne 1 -or
    !$taskExpected.ContainsKey('Contents/Resources/licenses/slint-1.17.1/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md')) { throw 'Required Slint/SoundTouch license materials are missing.' }
function Assert-BundleFiles([string]$Root, [bool]$CheckModes = $true) {
    Assert-Tree $Root
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($relative in $taskExpected.Keys) {
        $cursor = $relative
        while ($cursor.Contains('/')) {
            $cursor = $cursor.Substring(0, $cursor.LastIndexOf('/'))
            $null = $allowedDirectories.Add((Path-Key $cursor))
        }
    }
    foreach ($file in (Get-ChildItem -LiteralPath $Root -File -Recurse -Force)) {
        $relative = [IO.Path]::GetRelativePath($Root, $file.FullName).Replace('\', '/')
        $key = Path-Key $relative
        if (!$seen.Add($key) -or !$taskExpected.ContainsKey($key)) { throw 'Unplanned/duplicate bundle file.' }
        $wanted = $taskExpected[$key]
        if ($key -cne (Path-Key $wanted.path) -or $file.Length -ne $wanted.bytes -or (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant() -cne $wanted.sha256 -or
            ($CheckModes -and [int][IO.File]::GetUnixFileMode($file.FullName) -ne $wanted.mode)) { throw "Bundle file identity/mode mismatch: $relative" }
        Assert-Privacy ([IO.File]::ReadAllBytes($file.FullName)) $relative
    }
    if ($seen.Count -ne $taskExpected.Count) { throw 'Bundle is missing planned files.' }
    foreach ($directory in (Get-ChildItem -LiteralPath $Root -Directory -Recurse -Force)) {
        $relative = [IO.Path]::GetRelativePath($Root, $directory.FullName).Replace('\','/')
        $canonicalDirectory = $null
        if (!$allowedDirectories.TryGetValue((Path-Key $relative), [ref]$canonicalDirectory) -or (Path-Key $relative) -cne $canonicalDirectory) { throw 'Unplanned/miscased empty bundle directory.' }
    }
    if ($CheckModes) {
        foreach ($directory in @((Get-Item -LiteralPath $Root)) + @(Get-ChildItem -LiteralPath $Root -Directory -Recurse -Force)) {
            if ([int][IO.File]::GetUnixFileMode($directory.FullName) -ne 493) { throw 'Bundle directory mode must be 0755.' }
        }
    }
}
Assert-BundleFiles $BundlePath
Assert-Privacy ([IO.File]::ReadAllBytes($PackageReport)) 'package-report.json'
$taskArchiveHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
if ($taskReport.archive_sha256 -cne $taskArchiveHash -or
    [IO.File]::ReadAllText($ArchivePath + '.sha256').TrimEnd([char]13, [char]10) -cne ($taskArchiveHash + '  ' + [IO.Path]::GetFileName($ArchivePath))) { throw 'Archive/checksum/report identity mismatch.' }
Assert-Zip $ArchivePath $taskReport.bundle_name $taskExpected
$plist = Join-Path $BundlePath 'Contents/Info.plist'
$null = Read-Tool /usr/bin/plutil @('-lint', $plist)
foreach ($pair in @(
    @('CFBundleIdentifier', $taskReport.bundle_identifier), @('CFBundleExecutable','AudioLibExplorer'), @('CFBundleIconFile','AudioLibExplorer.icns'),
    @('CFBundlePackageType','APPL'), @('CFBundleShortVersionString',$taskVersion), @('CFBundleVersion',$taskVersion), @('LSMinimumSystemVersion','11.0')
)) {
    if ((Read-Tool /usr/bin/plutil @('-extract', $pair[0], 'raw', '-o', '-', $plist)) -cne $pair[1]) { throw 'Info.plist identity/platform mismatch.' }
}
foreach ($pair in @(@('update-channel.txt',$(if ($Tag.Contains('-')) { 'beta' } else { 'stable' })), @('update-version.txt',$Tag.Substring(1)))) {
    if ([IO.File]::ReadAllText((Join-Path $BundlePath ('Contents/Resources/' + $pair[0]))).Trim() -cne $pair[1]) { throw 'Bundle update marker mismatch.' }
}
$null = Read-Tool /usr/bin/codesign @('--verify','--deep','--strict',$BundlePath)
[xml]$taskResourceSeal = [IO.File]::ReadAllText((Join-Path $BundlePath 'Contents/_CodeSignature/CodeResources'))
$taskSealedFiles = $taskResourceSeal.SelectSingleNode("/plist/dict/key[text()='files2']/following-sibling::*[1][self::dict]")
if ($null -eq $taskSealedFiles) { throw 'Code signature resource seal is missing.' }
foreach ($name in @('update-channel.txt','update-version.txt')) {
    $sealedMarker = $taskSealedFiles.SelectSingleNode("key[text()='Resources/$name']/following-sibling::*[1][self::dict]")
    $sealedHash = if ($null -ne $sealedMarker) { $sealedMarker.SelectSingleNode("key[text()='hash2']/following-sibling::*[1][self::data]") } else { $null }
    if ($null -eq $sealedHash) { throw 'Package channel/version metadata is outside the code signature resource seal.' }
    $hashBytes = [Convert]::FromBase64String($sealedHash.InnerText)
    if ($hashBytes.Length -ne 32 -or [Convert]::ToHexString($hashBytes).ToLowerInvariant() -cne $taskExpected['Contents/Resources/' + $name].sha256) { throw 'Signed package channel/version hash differs from its file proof.' }
}
$signature = Read-Tool /usr/bin/codesign @('-d','--verbose=4',$BundlePath)
if (($taskReport.signing -ceq 'ad-hoc') -ne ($signature -match '(?m)^Signature=adhoc$')) { throw 'Signature type differs from package report.' }
function Assert-MachO([string]$Path) {
    if ((Read-Tool /usr/bin/lipo @('-archs',$Path)) -cne 'arm64') { throw 'Mac payload must contain native arm64 Mach-O only.' }
    $loads = Read-Tool /usr/bin/otool @('-l',$Path)
    $buildMinimum = [regex]::Match($loads, '(?s)cmd LC_BUILD_VERSION.*?\sminos\s+(\d+\.\d+(?:\.\d+)?)')
    $oldMinimum = [regex]::Match($loads, '(?s)cmd LC_VERSION_MIN_MACOSX.*?\sversion\s+(\d+\.\d+(?:\.\d+)?)')
    $minimum = if ($buildMinimum.Success) { $buildMinimum.Groups[1].Value } elseif ($oldMinimum.Success) { $oldMinimum.Groups[1].Value } else { throw 'Mach-O minimum macOS load command is missing.' }
    $minimumVersion = [version]$minimum
    if ($minimumVersion.Major -ne 11 -or $minimumVersion.Minor -ne 0 -or $minimumVersion.Build -gt 0) { throw 'Mach-O minimum OS differs from the declared macOS 11 target.' }
    $rpaths = @([regex]::Matches($loads, '(?s)cmd LC_RPATH\s+cmdsize \d+\s+path ([^\r\n]+?) \(offset \d+\)') | ForEach-Object { $_.Groups[1].Value })
    foreach ($rpath in $rpaths) {
        $expanded = $rpath.Replace('@loader_path', (Split-Path -Parent $Path)).Replace('@executable_path', (Join-Path $BundlePath 'Contents/MacOS'))
        if (![IO.Path]::IsPathRooted($expanded)) { throw 'Unresolved Mach-O runtime search path.' }
        $expanded = [IO.Path]::GetFullPath($expanded)
        if (!$expanded.StartsWith($BundlePath.TrimEnd('/') + '/', [StringComparison]::Ordinal) -and
            !$expanded.StartsWith('/usr/lib/', [StringComparison]::Ordinal) -and !$expanded.StartsWith('/System/Library/', [StringComparison]::Ordinal)) { throw 'Mach-O runtime search path escaped system/bundle roots.' }
    }
    $lines = (Read-Tool /usr/bin/otool @('-L',$Path)) -split '\r?\n'
    foreach ($line in ($lines | Select-Object -Skip 1)) {
        if ($line -notmatch '^\s+(.+?) \(compatibility version ') { continue }
        $dependency = $Matches[1]
        if ($dependency.StartsWith('/usr/lib/', [StringComparison]::Ordinal) -or $dependency.StartsWith('/System/Library/', [StringComparison]::Ordinal)) { continue }
        $candidates = @()
        $executableRoot = Join-Path $BundlePath 'Contents/MacOS'
        if ($dependency.StartsWith('@loader_path/')) { $candidates += Join-Path (Split-Path -Parent $Path) $dependency.Substring(13) }
        elseif ($dependency.StartsWith('@executable_path/')) { $candidates += Join-Path $executableRoot $dependency.Substring(17) }
        elseif ($dependency.StartsWith('@rpath/')) {
            foreach ($rpath in $rpaths) {
                $base = $rpath.Replace('@loader_path', (Split-Path -Parent $Path)).Replace('@executable_path', $executableRoot)
                if ([IO.Path]::IsPathRooted($base)) { $candidates += Join-Path $base $dependency.Substring(7) }
            }
        }
        $resolved = @($candidates | ForEach-Object { [IO.Path]::GetFullPath($_) } | Where-Object {
            $_.StartsWith($BundlePath.TrimEnd('/') + '/', [StringComparison]::Ordinal) -and (Test-Path -LiteralPath $_ -PathType Leaf)
        })
        if (!$resolved.Count) { throw 'Non-system dependency is not an owned bundled Mach-O.' }
        foreach ($candidate in $resolved) {
            Assert-Unlinked $candidate
            $relative = [IO.Path]::GetRelativePath($BundlePath,$candidate).Replace('\','/')
            if (!$taskExpected.ContainsKey((Path-Key $relative)) -or (Read-Tool /usr/bin/lipo @('-archs',$candidate)) -cne 'arm64') { throw 'Bundled dependency is outside the exact arm64 file allowlist.' }
        }
    }
}
foreach ($name in @('AudioLibExplorer','AudioLibCli')) {
    $path = Join-Path $BundlePath ('Contents/MacOS/' + $name)
    Assert-MachO $path
    $null = Read-Tool /usr/bin/codesign @('--verify','--strict',$path)
    if ((Read-Tool $path @('--version')) -cne ($name + ' ' + $taskVersion)) { throw 'Native binary version differs from tag.' }
}
if ($StructureOnly) {
    Write-Host 'Mac package structural checks passed. Runtime validation: not-run (-StructureOnly).'
    return
}
if ((Read-Tool /usr/bin/id @('-u')) -ceq '0') { throw 'Run acceptance as an ordinary user; root bypasses read-only-directory probes.' }
# /var and /tmp are system symlinks on macOS. Resolve the existing parent before
# creating our unique subtree, then reject every link in that owned subtree.
$entered = $false
try {
    Push-Location -LiteralPath ([IO.Path]::GetTempPath())
    $entered = $true
    $physicalTemp = Read-Tool /bin/pwd @('-P')
} finally { if ($entered) { Pop-Location } }
$taskEvidence = Join-Path $physicalTemp ('AudioLibMacAcceptance-' + [guid]::NewGuid().ToString('N'))
Assert-Unlinked $taskEvidence
New-Item -ItemType Directory -Path $taskEvidence | Out-Null
$taskRuntime = [ordered]@{ success=$false; tag=$Tag; commit=$ExpectedCommit; runtime_validation='failed'; evidence=$taskEvidence; checks=@(); timed_out_pid=$null; native_quit='not-run'; acoustic_playback='not-run'; real_server='not-run' }
$taskReadonly = [Collections.Generic.List[object]]::new()
function Invoke-Owned([string]$Executable, [string[]]$Arguments, [string]$Name, [string]$HomeRoot, [string]$DataRoot, [string]$WorkingDirectory) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    foreach ($key in @($info.Environment.Keys)) {
        if ($key.StartsWith('AUDIOLIB_', [StringComparison]::OrdinalIgnoreCase) -or $key -in @('LOCALAPPDATA','APPDATA','XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_CACHE_HOME','SLINT_BACKEND')) { $null = $info.Environment.Remove($key) }
    }
    $info.Environment['HOME'] = $HomeRoot
    $info.Environment['TMPDIR'] = Join-Path $taskEvidence 'tmp'
    if ($DataRoot) { $info.Environment['AUDIOLIB_DATA_DIR'] = $DataRoot }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    try {
        $null = $process.Start()
        $outTask = $process.StandardOutput.ReadToEndAsync()
        $errTask = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(45000)) {
            $taskRuntime.timed_out_pid = $process.Id
            [IO.File]::WriteAllText((Join-Path $taskEvidence ($Name + '.timeout.txt')), ('Owned process still running: ' + $process.Id), [Text.UTF8Encoding]::new($false))
            throw "Owned $Name process timed out; preserved without termination. Evidence: $taskEvidence"
        }
        $stdout = $outTask.GetAwaiter().GetResult()
        $stderr = $errTask.GetAwaiter().GetResult()
        [IO.File]::WriteAllText((Join-Path $taskEvidence ($Name + '.stdout.txt')), $stdout, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText((Join-Path $taskEvidence ($Name + '.stderr.txt')), $stderr, [Text.UTF8Encoding]::new($false))
        return [pscustomobject]@{ code=$process.ExitCode; output=$stdout; error=$stderr }
    } finally { $process.Dispose() }
}
function Set-Settings([string]$Root, [string]$Backend = 'wasapi') {
    New-Item -ItemType Directory -Path $Root -Force | Out-Null
    $settings = [ordered]@{ active_database=(Join-Path $Root 'library.db'); language='en'; playback=@{backend=$Backend;volume=0.0}; allowed_scan_roots=@(); pregenerate_list_waveforms=$false; data_source=@{mode='local';server_url=''} }
    [IO.File]::WriteAllText((Join-Path $Root 'settings.json'), ($settings | ConvertTo-Json -Depth 6), [Text.UTF8Encoding]::new($false))
}
try {
    foreach ($name in @('extracted','home-explicit','home-default','readonly-cwd','tmp','fixtures','userdata')) { New-Item -ItemType Directory -Path (Join-Path $taskEvidence $name) | Out-Null }
    $extract = Join-Path $taskEvidence 'extracted'
    $null = Read-Tool /usr/bin/ditto @('-x','-k','--norsrc',$ArchivePath,$extract)
    $ownedBundle = Join-Path $extract $taskReport.bundle_name
    Assert-BundleFiles $ownedBundle
    $desktop = Join-Path $ownedBundle 'Contents/MacOS/AudioLibExplorer'
    $cli = Join-Path $ownedBundle 'Contents/MacOS/AudioLibCli'
    $explicitHome = Join-Path $taskEvidence 'home-explicit'
    $data = Join-Path $taskEvidence 'userdata'
    $cwd = Join-Path $taskEvidence 'readonly-cwd'
    Set-Settings $data
    $initial = Invoke-Owned $desktop @('--headless-smoke') 'explicit-headless' $explicitHome $data $cwd
    if ($initial.code -ne 0) { throw 'Isolated headless startup failed.' }
    $initialJson = $initial.output | ConvertFrom-Json
    if (!$initialJson.success -or $initialJson.total_count -ne 0 -or $initialJson.database -cne (Join-Path $data 'library.db')) { throw 'Explicit data-root first launch did not select its owned empty library.' }
    $fixture = Join-Path $taskEvidence 'fixtures/synthetic tone.wav'
    $writer = [IO.BinaryWriter]::new([IO.File]::Create($fixture))
    try {
        $rate = 48000; $size = $rate * 2
        $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $writer.Write([uint32]($size + 36))
        $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $writer.Write([uint32]16)
        $writer.Write([uint16]1); $writer.Write([uint16]1); $writer.Write([uint32]$rate)
        $writer.Write([uint32]($rate * 2)); $writer.Write([uint16]2); $writer.Write([uint16]16)
        $writer.Write([Text.Encoding]::ASCII.GetBytes('data')); $writer.Write([uint32]$size)
        for ($frame=0; $frame -lt $rate; $frame++) { $writer.Write([int16](12000 * [Math]::Sin(2 * [Math]::PI * 619 * $frame / $rate))) }
    } finally { $writer.Dispose() }
    $fixtureHash = (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash
    $scan = Invoke-Owned $cli @('scan',(Split-Path -Parent $fixture)) 'synthetic-scan' $explicitHome $data $cwd
    if ($scan.code -ne 0 -or !($scan.output | ConvertFrom-Json).success) { throw 'Packaged CLI synthetic WAV scan failed.' }
    $stats = Invoke-Owned $cli @('stats') 'synthetic-stats' $explicitHome $data $cwd
    $statsJson = $stats.output | ConvertFrom-Json
    if ($stats.code -ne 0 -or !$statsJson.success -or $statsJson.data.audio_files -ne 1) { throw 'Packaged CLI did not find exactly one synthetic record.' }

    # Exercise AppKit's application termination request while a Metadata draft is
    # dirty. The application hook is constrained to this owned one-record fixture;
    # persisted state is read back through the packaged CLI after the process exits.
    $quitBefore = Invoke-Owned $cli @('search','--limit','2') 'native-quit-before' $explicitHome $data $cwd
    $quitBeforeJson = $quitBefore.output | ConvertFrom-Json
    $quitRecords = @($quitBeforeJson.data.results)
    $fixtureCanonical = (Resolve-Path -LiteralPath $fixture).ProviderPath
    if ($quitBefore.code -ne 0 -or !$quitBeforeJson.success -or $quitBeforeJson.data.total_count -ne 1 -or
        $quitRecords.Count -ne 1 -or $quitRecords[0].id -le 0 -or $quitRecords[0].file_path -cne $fixtureCanonical -or
        $quitRecords[0].notes -cne '') { throw 'Native Quit acceptance requires its untouched, single-record synthetic library.' }
    $quitMarker = Join-Path $data '.native-quit-fixture.json'
    Assert-Unlinked $quitMarker
    [IO.File]::WriteAllText($quitMarker, (([ordered]@{schema_version=1;fixture_path=$fixtureCanonical} | ConvertTo-Json) + [char]10), [Text.UTF8Encoding]::new($false))
    $quitShot = Join-Path $taskEvidence 'native-quit.png'
    $taskRuntime.native_quit = 'failed'
    $quit = Invoke-Owned $desktop @('--smoke-ui','--smoke-native-quit','--screenshot',$quitShot) 'native-quit-ui' $explicitHome $data $cwd
    if ($quit.code -ne 0 -or !(Test-Path -LiteralPath $quitShot -PathType Leaf) -or (Get-Item -LiteralPath $quitShot).Length -eq 0) { throw 'Native application Quit did not exit cleanly after its Metadata draft.' }
    $quitAfter = Invoke-Owned $cli @('info',([string]$quitRecords[0].id)) 'native-quit-after' $explicitHome $data $cwd
    $quitAfterJson = $quitAfter.output | ConvertFrom-Json
    if ($quitAfter.code -ne 0 -or !$quitAfterJson.success -or $null -eq $quitAfterJson.data -or
        $quitAfterJson.data.id -ne $quitRecords[0].id -or $quitAfterJson.data.file_path -cne $fixtureCanonical -or
        $quitAfterJson.data.notes -cne 'AudioLib native Quit drain acceptance' -or
        (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash -cne $fixtureHash) { throw 'Native application Quit lost the pending Metadata draft or changed the source WAV.' }
    $taskRuntime.native_quit = 'passed'
    $taskRuntime.checks += 'native application Quit drains a dirty Metadata draft before exit; CLI readback and source WAV identity verified'

    $screenshot = Join-Path $taskEvidence 'chinese.png'
    $ui = Invoke-Owned $desktop @('--smoke-ui','--smoke-language','cn','--screenshot',$screenshot) 'chinese-ui' $explicitHome $data $cwd
    if ($ui.code -ne 0 -or !(Test-Path -LiteralPath $screenshot -PathType Leaf) -or (Get-Item -LiteralPath $screenshot).Length -eq 0) { throw 'Chinese UI startup/screenshot/clean exit failed.' }
    $bad = Join-Path $taskEvidence 'bad-preferences'
    New-Item -ItemType Directory -Path $bad | Out-Null
    [IO.File]::WriteAllText((Join-Path $bad 'settings.json'), '{invalid JSON', [Text.UTF8Encoding]::new($false))
    $failure = Invoke-Owned $desktop @('--headless-smoke') 'invalid-preferences' $explicitHome $bad $cwd
    $diagnostic = Join-Path $bad 'logs/AudioLibExplorer-startup-error.log'
    if ($failure.code -ne 1 -or !(Test-Path -LiteralPath $diagnostic) -or [IO.File]::ReadAllText($diagnostic) -notmatch 'Load AudioLib preferences') { throw 'Invalid preferences did not produce a synchronous startup diagnostic.' }
    $taskRuntime.checks += @('isolated explicit data root','synthetic WAV import and CLI statistics','Chinese UI screenshot and clean exit','invalid-preference failure diagnostic')

    # Read-only only our extracted copy and empty CWD. A fresh fake home with no
    # settings/override proves the platform default rather than masking it.
    foreach ($item in @((Get-Item -LiteralPath $ownedBundle)) + @(Get-ChildItem -LiteralPath $ownedBundle -Recurse -Force) + @((Get-Item -LiteralPath $cwd))) {
        $mode = [int][IO.File]::GetUnixFileMode($item.FullName)
        $taskReadonly.Add([pscustomobject]@{path=$item.FullName;mode=$mode})
        [IO.File]::SetUnixFileMode($item.FullName, [IO.UnixFileMode]($mode -band 365))
    }
    $defaultHome = Join-Path $taskEvidence 'home-default'
    $defaultChannel = if ($Tag.Contains('-')) { 'beta' } else { 'v1' }
    $defaultRoot = Join-Path $defaultHome ('Library/Application Support/AudioLibExplorer/' + $defaultChannel)
    $untouchedRoot = Join-Path $defaultHome ('Library/Application Support/AudioLibExplorer/' + $(if ($defaultChannel -eq 'beta') { 'v1' } else { 'beta' }))
    $default = Invoke-Owned $desktop @('--headless-smoke') 'default-root-headless' $defaultHome '' $cwd
    $defaultJson = $default.output | ConvertFrom-Json
    if ($default.code -ne 0 -or !$defaultJson.success -or $defaultJson.total_count -ne 0 -or
        $defaultJson.database -cne (Join-Path $defaultRoot 'library.db') -or !(Test-Path -LiteralPath (Join-Path $defaultRoot 'library.db'))) { throw 'Default macOS data root escaped fake HOME or failed with a read-only app/CWD.' }
    $defaultStats = Invoke-Owned $cli @('stats') 'default-root-cli' $defaultHome '' $cwd
    $defaultStatsJson = $defaultStats.output | ConvertFrom-Json
    if ($defaultStats.code -ne 0 -or !$defaultStatsJson.success -or $defaultStatsJson.data.audio_files -ne 0) { throw 'Default macOS CLI did not use the same empty library.' }
    # An imported ASIO selection must launch safely on CoreAudio; keep volume at 0.
    Set-Settings $defaultRoot 'asio'
    $defaultShot = Join-Path $taskEvidence 'default-root-ui.png'
    $defaultUi = Invoke-Owned $desktop @('--smoke-ui','--screenshot',$defaultShot) 'default-root-ui' $defaultHome '' $cwd
    if ($defaultUi.code -ne 0 -or !(Test-Path -LiteralPath $defaultShot -PathType Leaf)) { throw 'Default-root UI/imported-ASIO fallback startup failed.' }
    if ((Test-Path -LiteralPath $untouchedRoot) -or @(Get-ChildItem -LiteralPath $cwd -Force).Count -or
        (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash -cne $fixtureHash) { throw 'Opposite-channel data root, read-only CWD or source WAV changed.' }
    Assert-BundleFiles $ownedBundle $false
    Assert-BundleFiles $BundlePath
    $taskRuntime.checks += @('default root uses fake HOME with no data override','read-only app and CWD remain unchanged','default-root CLI and GUI startup','imported ASIO preferences launch on Mac','source synthetic WAV unchanged')
    $taskRuntime.success = $true
    $taskRuntime.runtime_validation = 'passed'
} finally {
    foreach ($item in $taskReadonly) {
        Assert-Unlinked $item.path
        [IO.File]::SetUnixFileMode($item.path, [IO.UnixFileMode]$item.mode)
    }
    [IO.File]::WriteAllText((Join-Path $taskEvidence 'validation.json'), (($taskRuntime | ConvertTo-Json -Depth 8) + [char]10), [Text.UTF8Encoding]::new($false))
    Write-Host "Mac native acceptance evidence retained: $taskEvidence"
}
Write-Host 'Mac package/native synthetic acceptance passed. Acoustic playback and real-server beta checks: not-run.'
