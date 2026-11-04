# tools/validate-server-package.ps1
# Validate the native server archive and smoke-test its extracted executable against
# an owned synthetic library. Requires PowerShell 7.3; never uses normal runtime data.
# ./tools/validate-server-package.ps1 -Tag v0.4.6 -Target x86_64-pc-windows-msvc -Asio
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'aarch64-apple-darwin')]
    [string]$Target,
    [switch]$Asio
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'console-utf8.ps1')
try {
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Server validation requires PowerShell 7.3 or newer.' }
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$comparison = if ($IsWindows) { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
$tagMatch = [regex]::Match($Tag, '^v(\d+\.\d+\.\d+)(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$')
if (!$tagMatch.Success) { throw 'Tag must be vMAJOR.MINOR.PATCH, vMAJOR.MINOR.PATCH-rc.N or vMAJOR.MINOR.PATCH-beta.N.' }
$version = $tagMatch.Groups[1].Value
if ($Asio -and ($Target -cne 'x86_64-pc-windows-msvc' -or !$IsWindows)) { throw '-Asio requires the native Windows server target.' }
$taskProcessArchitecture = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
switch ($Target) {
    'x86_64-pc-windows-msvc' {
        if (!$IsWindows) { throw 'Validate the Windows package on Windows.' }
        $suffix = 'win-x64'; $binaryName = 'audiolib-server.exe'; $extension = 'zip'
    }
    'x86_64-unknown-linux-gnu' {
        if (!$IsLinux -or $taskProcessArchitecture -ne [Runtime.InteropServices.Architecture]::X64) { throw 'Validate the Linux x64 package on Linux x64.' }
        $suffix = 'linux-x64'; $binaryName = 'audiolib-server'; $extension = 'tar.gz'
    }
    'aarch64-unknown-linux-gnu' {
        if (!$IsLinux -or $taskProcessArchitecture -ne [Runtime.InteropServices.Architecture]::Arm64) { throw 'Validate the Linux arm64 package on Linux arm64.' }
        $suffix = 'linux-arm64'; $binaryName = 'audiolib-server'; $extension = 'tar.gz'
    }
    'aarch64-apple-darwin' {
        if (!$IsMacOS) { throw 'Validate the macOS package on macOS.' }
        $suffix = 'macos-arm64'; $binaryName = 'audiolib-server'; $extension = 'tar.gz'
    }
}

function Assert-Within([string]$Path, [string]$Parent) {
    $full = [IO.Path]::GetFullPath($Path)
    $boundary = [IO.Path]::GetFullPath($Parent).TrimEnd([IO.Path]::DirectorySeparatorChar)
    if (!$full.StartsWith($boundary + [IO.Path]::DirectorySeparatorChar, $comparison)) { throw 'Server validation path escaped its owned boundary.' }
}
function Assert-UnlinkedTree([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked server validation path is not supported.' }
    if ($null -ne $item -and $item.PSIsContainer -and
        @(Get-ChildItem -LiteralPath $Path -Recurse -Force -Attributes ReparsePoint).Count) { throw 'Linked server validation tree is not supported.' }
}
function Assert-Relative([string]$Path) {
    if (!$Path -or [IO.Path]::IsPathRooted($Path) -or $Path -match '[\\:\x00-\x1f]' -or
        @($Path.Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count) { throw 'Unsafe server archive entry.' }
}
function Assert-Keys($Object, [string[]]$Keys) {
    if ($Object -isnot [Collections.IDictionary] -or $Object.Count -ne $Keys.Count -or
        @($Keys | Where-Object { !$Object.Contains($_) }).Count) { throw 'Server report has an unexpected schema.' }
}
function Quote-NativeArgument([string]$Argument) {
    # Start-Process joins ArgumentList into one command line on every platform.
    # Quote spaces and double backslashes only before quotes/the closing quote.
    return '"' + (($Argument -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
}
function Start-OwnedProcess([string]$Executable, [string[]]$Arguments, [string]$Name, [string]$WorkingDirectory) {
    $options = @{
        FilePath=$Executable; ArgumentList=(($Arguments | ForEach-Object { Quote-NativeArgument $_ }) -join ' ')
        WorkingDirectory=$WorkingDirectory; PassThru=$true
        RedirectStandardOutput=(Join-Path $testRoot ($Name + '.stdout'))
        RedirectStandardError=(Join-Path $testRoot ($Name + '.stderr'))
    }
    if ($IsWindows) { $options.WindowStyle = 'Hidden' }
    $process = Start-Process @options
    $null = $process.Handle
    $ownedProcesses.Add($process)
    return $process
}
function Wait-OwnedProcess([Diagnostics.Process]$Process, [int]$TimeoutMilliseconds, [string]$Failure) {
    if (!$Process.WaitForExit($TimeoutMilliseconds) -or $Process.ExitCode -ne 0) { throw $Failure }
}
function Read-Endpoint([string]$Path) {
    $response = $http.GetAsync($baseUrl + $Path).GetAwaiter().GetResult()
    try {
        if (!$response.IsSuccessStatusCode) { throw 'Server smoke endpoint returned an unsuccessful status.' }
        $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        return ConvertFrom-Json -InputObject $body -AsHashtable
    } finally { $response.Dispose() }
}

$latest = Join-Path $taskRoot 'dist/latest'
$archiveName = "AudioLibServer-$Tag-$suffix.$extension"
$archive = Join-Path $latest $archiveName
$reportPath = Join-Path $latest "AudioLibServer-$Tag-$suffix-report.json"
foreach ($path in @($archive, ($archive + '.sha256'), $reportPath)) {
    Assert-Within $path $taskRoot
    Assert-UnlinkedTree $path
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw 'Required server release artifact is missing.' }
}
$archiveHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
$checksum = (Get-Content -LiteralPath ($archive + '.sha256') -Raw).TrimEnd("`r", "`n")
if ($checksum -cne ($archiveHash + '  ' + $archiveName)) { throw 'Server archive checksum differs from its sidecar.' }
$report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json -AsHashtable
Assert-Keys $report @('success', 'version', 'tag', 'commit', 'target', 'binary_sha256', 'archive_sha256', 'checks')
Assert-Keys $report.checks @('version', 'allowlist', 'unlinked_paths', 'privacy', 'archive_contents', 'executable_mode')
if ($report.success -isnot [bool] -or !$report.success -or
    @($report.checks.Values | Where-Object { $_ -isnot [bool] -or !$_ }).Count -or
    $report.version -cne $version -or $report.tag -cne $Tag -or $report.target -cne $Target -or
    $report.binary_sha256 -isnot [string] -or $report.binary_sha256 -cnotmatch '^[0-9A-F]{64}$' -or
    $report.archive_sha256 -cne $archiveHash) { throw 'Server package report identity/checks are invalid.' }
Push-Location -LiteralPath $taskRoot
try {
    $commit = (& git -c "safe.directory=$taskRoot" rev-parse HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Cannot obtain server validation source commit.' }
    $shortCommit = (& git -c "safe.directory=$taskRoot" rev-parse --short=7 HEAD 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $shortCommit -cnotmatch '^[0-9a-f]{7,40}$') { throw 'Cannot obtain server validation short commit.' }
} finally { Pop-Location }
if ($report.commit -cne $commit) { throw 'Server package report differs from the current source commit.' }
$publicFiles = @(Get-Content -LiteralPath (Join-Path $PSScriptRoot 'server-files.json') -Raw | ConvertFrom-Json)
$required = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$null = $required.Add($binaryName)
$null = $required.Add('update-channel.txt')
$null = $required.Add('update-version.txt')
$null = $required.Add('licenses/dependencies.json')
$sdkLicenseRelative = 'licenses/Steinberg-ASIO-SDK-2.3.4.txt'
if ($Asio) { $null = $required.Add($sdkLicenseRelative) }
foreach ($relative in $publicFiles) {
    if ($relative -isnot [string]) { throw 'Server allowlist must contain strings.' }
    Assert-Relative $relative
    if (!$required.Add($relative)) { throw 'Server allowlist contains duplicate files.' }
}

$tempParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = [IO.Path]::GetFullPath((Join-Path $tempParent ('AudioLibServerValidate-' + [guid]::NewGuid().ToString('N'))))
$extractRoot = Join-Path $testRoot 'package'
$ownedProcesses = [Collections.Generic.List[Diagnostics.Process]]::new()
$server = $null; $http = $null; $handler = $null; $testOwned = $false
$savedUpdateDisable = $env:AUDIOLIB_UPDATE_DISABLE
try {
    # Startup acceptance uses only its isolated fixture, never the public network.
    $env:AUDIOLIB_UPDATE_DISABLE = '1'
    Assert-Within $testRoot $tempParent
    if (Test-Path -LiteralPath $testRoot) { throw 'Server validation temporary directory already exists.' }
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $testOwned = $true
    New-Item -ItemType Directory -Path $extractRoot | Out-Null
    $archiveFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    if ($IsWindows) {
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            foreach ($entry in $zip.Entries) {
                $relative = $entry.FullName.TrimEnd('/')
                Assert-Relative $relative
                # A ZIP may carry Unix symlink/special-file bits even on Windows.
                $type = (($entry.ExternalAttributes -shr 16) -band 61440)
                if ($type -notin @(0, 32768, 16384)) { throw 'Linked or special server ZIP entry is not supported.' }
                if (!$entry.FullName.EndsWith('/') -and !$archiveFiles.Add($relative)) { throw 'Duplicate server archive entry.' }
            }
        } finally { $zip.Dispose() }
    } else {
        $entries = @(& tar -tzf $archive 2>$null)
        if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect server tar entries.' }
        $details = @(& tar -tvzf $archive 2>$null)
        if ($LASTEXITCODE -ne 0 -or $details.Count -ne $entries.Count -or
            @($details | Where-Object { $_ -notmatch '^[d-]' }).Count) { throw 'Linked or special server tar entry is not supported.' }
        foreach ($entry in $entries) {
            if ($entry -eq './') { continue }
            $relative = ($entry -replace '^\./', '').TrimEnd('/')
            Assert-Relative $relative
            if (!$entry.EndsWith('/') -and !$archiveFiles.Add($relative)) { throw 'Duplicate server archive entry.' }
        }
    }
    if (@($required | Where-Object { !$archiveFiles.Contains($_) }).Count -or
        @($archiveFiles | Where-Object { !$required.Contains($_) -and !$_.StartsWith('licenses/', [StringComparison]::Ordinal) }).Count) { throw 'Server archive differs from its public file allowlist.' }
    if ($IsWindows) {
        [IO.Compression.ZipFile]::ExtractToDirectory($archive, $extractRoot)
    } else {
        & tar -xzf $archive -C $extractRoot 2>$null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot extract the server archive.' }
    }
    Assert-UnlinkedTree $extractRoot
    $files = @(Get-ChildItem -LiteralPath $extractRoot -File -Recurse -Force)
    if ($files.Count -ne $archiveFiles.Count) { throw 'Extracted server file count differs from the archive.' }
    $dependencies = @(Get-Content -LiteralPath (Join-Path $extractRoot 'licenses/dependencies.json') -Raw | ConvertFrom-Json -AsHashtable)
    $licenseRoots = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($dependency in $dependencies) {
        Assert-Keys $dependency @('name', 'version', 'license', 'repository')
        if ($dependency.name -isnot [string] -or $dependency.name -cnotmatch '^[A-Za-z0-9_-]+$' -or
            $dependency.version -isnot [string] -or $dependency.version -cnotmatch '^[A-Za-z0-9.+_-]+$' -or
            ($null -ne $dependency.license -and $dependency.license -isnot [string]) -or
            ($null -ne $dependency.repository -and ($dependency.repository -isnot [string] -or $dependency.repository -notmatch '^https?://[^/@\s]+(?:/[^\s]*)?$')) -or
            !$licenseRoots.Add('licenses/' + $dependency.name + '-' + $dependency.version + '/')) { throw 'Server dependency notice schema is invalid.' }
    }
    if (!$dependencies.Count -or !($dependencies | Where-Object { $_.name -ceq 'audiolib-server' -and $_.version -ceq $version })) { throw 'Server dependency notice identity is missing.' }
    $hasAsio = @($dependencies | Where-Object { $_.name -ceq 'asio-sys' }).Count -gt 0
    if ($hasAsio -ne [bool]$Asio -or (!$Asio -and $archiveFiles.Contains($sdkLicenseRelative))) { throw 'Server ASIO notices differ from the selected -Asio mode.' }
    if ($Asio) {
        # SHA256 of ASIOSDK/LICENSE.txt from the pinned SDK 2.3.4 archive
        # D5EBF0C20DD2C5F43771FD0C1418F4B361BF52434EE670097CFA6B3A335E2ECA.
        $expectedLicenseHash = '61E56814F24B32216884B2DB0B4DCC317111A5AB354D54D19DD9FC13D71B1C7A'
        if ((Get-FileHash -LiteralPath (Join-Path $extractRoot $sdkLicenseRelative) -Algorithm SHA256).Hash -cne $expectedLicenseHash) { throw 'Packaged ASIO SDK notice differs from the pinned SDK 2.3.4 license.' }
        $asioNotices = @($archiveFiles | Where-Object { $_ -cmatch '^licenses/asio-sys-[^/]+/(?:LICENSE|COPYING|NOTICE)' })
        if (!$asioNotices.Count) {
            # asio-sys 0.4.0 is the `asio-sys/` subdirectory of CPAL's repository;
            # its published README explicitly refers to ../LICENSE. Both published
            # crates identify source commit e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7.
            # Accept that available root notice only for this audited identity.
            $asioIdentity = @($dependencies | Where-Object { $_.name -ceq 'asio-sys' })
            $cpalIdentity = @($dependencies | Where-Object { $_.name -ceq 'cpal' })
            $cpalNotice = 'licenses/cpal-0.18.2/LICENSE'
            $repository = 'https://github.com/RustAudio/cpal'
            if ($asioIdentity.Count -ne 1 -or $cpalIdentity.Count -ne 1 -or
                $asioIdentity[0].version -cne '0.4.0' -or $cpalIdentity[0].version -cne '0.18.2' -or
                $asioIdentity[0].repository -cne $repository -or $cpalIdentity[0].repository -cne $repository -or
                $asioIdentity[0].license -cne 'Apache-2.0' -or $cpalIdentity[0].license -cne 'Apache-2.0' -or
                !$archiveFiles.Contains($cpalNotice)) { throw 'Packaged asio-sys shared CPAL license identity is invalid or missing.' }
            $expectedCpalHash = 'C71D239DF91726FC519C6EB72D318EC65820627232B2F796219E87DCF35D0AB4'
            if ((Get-FileHash -LiteralPath (Join-Path $extractRoot $cpalNotice) -Algorithm SHA256).Hash -cne $expectedCpalHash) { throw 'Packaged asio-sys shared CPAL license differs from the audited upstream notice.' }
        }
    }
    $privatePrefixes = @($taskRoot, $env:USERPROFILE, $env:HOME, $env:CARGO_HOME, $env:RUSTUP_HOME, $env:CARGO_TARGET_DIR) | Where-Object { $_ }
    foreach ($file in $files) {
        Assert-Within $file.FullName $extractRoot
        $relative = [IO.Path]::GetRelativePath($extractRoot, $file.FullName).Replace('\', '/')
        if (!$archiveFiles.Contains($relative) -or
            (!$required.Contains($relative) -and !@($licenseRoots | Where-Object { $relative.StartsWith($_, [StringComparison]::Ordinal) }).Count) -or
            $file.Extension -in @('.db', '.sqlite', '.sqlite3', '.log', '.wav') -or
            $file.Name -in @('settings.json', 'admin-settings.json', 'tokens.json', '.env') -or
            $file.Name -match '(?:-wal|-shm|\.server-state\.json)$' -or $relative -match '(?:^|/)(?:logs?|cache|recovery)/') { throw 'Server archive contains a non-public file.' }
        $bytes = [IO.File]::ReadAllBytes($file.FullName)
        $texts = @([Text.Encoding]::UTF8.GetString($bytes))
        if ($relative -ceq $binaryName) {
            $texts += [Text.Encoding]::Unicode.GetString($bytes)
            if ($bytes.Length -gt 1) { $texts += [Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1) }
        }
        foreach ($text in $texts) {
            foreach ($prefix in $privatePrefixes) {
                foreach ($spelling in @($prefix, $prefix.Replace('\', '/'), $prefix.Replace('\', '\\'))) {
                    if ($text.IndexOf($spelling, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw 'Server payload contains a local build/user path.' }
                }
            }
        }
        if (!$IsWindows) {
            $expectedMode = if ($relative -in @($binaryName, 'server/deploy/redeploy-server.sh')) { 493 } else { 420 }
            if ([int][IO.File]::GetUnixFileMode($file.FullName) -ne $expectedMode) { throw 'Extracted server file permissions differ from the package contract.' }
        }
    }
    $expectedChannel = if ($Tag.Contains('-')) { 'beta' } else { 'stable' }
    if ((Get-Content -LiteralPath (Join-Path $extractRoot 'update-channel.txt') -Raw).Trim() -cne $expectedChannel -or
        (Get-Content -LiteralPath (Join-Path $extractRoot 'update-version.txt') -Raw).Trim() -cne $Tag.Substring(1)) { throw 'Server update metadata differs from the release tag.' }
    $binary = Join-Path $extractRoot $binaryName
    if ((Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -cne $report.binary_sha256) { throw 'Extracted server binary SHA256 differs from its report.' }
    $versionProcess = Start-OwnedProcess $binary @('--version') 'version' $extractRoot
    Wait-OwnedProcess $versionProcess 30000 'Extracted server version check failed or timed out.'
    if ((Get-Content -LiteralPath (Join-Path $testRoot 'version.stdout') -Raw).Trim() -cne "audiolib-server $version") { throw 'Extracted server version differs from the tag.' }

    $database = Join-Path $testRoot 'library.db'
    if ($IsWindows) {
        $targetRoot = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR, $taskRoot) } else { Join-Path $taskRoot 'target' }
        $benchmark = Join-Path $targetRoot "$Target/release/search-benchmark.exe"
        if (!(Test-Path -LiteralPath $benchmark -PathType Leaf)) { throw 'Build the Windows release search-benchmark before server validation.' }
        Assert-UnlinkedTree $benchmark
        $seed = Start-OwnedProcess $benchmark @('--db', $database, '--records', '1', '--queries', '100') 'seed' $testRoot
    } else {
        $cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
        $seed = Start-OwnedProcess $cargo @('run', '-p', 'audiolib-db', '--bin', 'search-benchmark', '--locked', '--target', $Target, '--', '--db', $database, '--records', '1', '--queries', '100') 'seed' $taskRoot
    }
    Wait-OwnedProcess $seed 600000 'Synthetic server library generation failed or timed out.'
    if (!(Test-Path -LiteralPath $database -PathType Leaf)) { throw 'Synthetic server library was not created.' }
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    try { $listener.Start(); $port = $listener.LocalEndpoint.Port } finally { $listener.Stop() }
    $config = Join-Path $testRoot 'server.toml'
    $configLines = @(
        ('bind = "127.0.0.1:' + $port + '"')
        ('library_db = ' + (ConvertTo-Json -InputObject $database -Compress))
        'auth_mode = "off"', 'manage_mode = "off"', 'roots = []'
        ('token_env = "AUDIOLIB_SERVER_VALIDATION_' + [guid]::NewGuid().ToString('N') + '"')
        ('snapshot_dir = ' + (ConvertTo-Json -InputObject (Join-Path $testRoot 'snapshots') -Compress))
        ('waveform_cache_dir = ' + (ConvertTo-Json -InputObject (Join-Path $testRoot 'waveforms') -Compress))
    )
    Set-Content -LiteralPath $config -Value $configLines -Encoding utf8NoBOM
    $server = Start-OwnedProcess $binary @('--config', $config) 'server' $testRoot
    $baseUrl = "http://127.0.0.1:$port/api/v1"
    $handler = [Net.Http.HttpClientHandler]::new(); $handler.UseProxy = $false
    $http = [Net.Http.HttpClient]::new($handler); $http.Timeout = [TimeSpan]::FromSeconds(2)
    $deadline = [datetime]::UtcNow.AddSeconds(45)
    $health = $null
    do {
        if ($server.HasExited) { throw 'Extracted server exited before its isolated library became ready.' }
        try { $health = Read-Endpoint '/health' } catch { $health = $null }
        if ($null -ne $health -and $health.Contains('status') -and $health.status -ceq 'ok') { break }
        Start-Sleep -Milliseconds 200
    } while ([datetime]::UtcNow -lt $deadline)
    if ($null -eq $health -or $health.status -cne 'ok' -or $health.version -cne $version -or
        $health.api_version -cne 'v1' -or $health.schema_version -ne 8 -or $health.commit -cne $shortCommit) { throw 'Extracted server health identity/API/schema check failed.' }
    $capabilities = Read-Endpoint '/capabilities'
    if ($capabilities.compatibility.features.album_entities -ne 1 -or
        $capabilities.compatibility.features.album_entity_artwork -ne 1 -or
        $capabilities.compatibility.features.Contains('metadata_storage')) {
        throw 'Extracted server shared-album capabilities differ from the plain Metadata contract.'
    }
    $library = Read-Endpoint '/library'
    if ($server.HasExited -or $library.library_id -isnot [string] -or !$library.library_id -or
        $library.schema_version -ne 8 -or $library.identity_bound_writes -isnot [bool] -or !$library.identity_bound_writes -or
        $library.stats.audio_files -ne 1 -or $library.database -cne [IO.Path]::GetFileName($database)) { throw 'Extracted server could not serve its isolated synthetic library.' }
    if ($library.snapshot_format.id -cne 'ale-s8-cld1-t3-m2-p1-a1' -or
        $library.snapshot_format.format.schema_version -ne 8 -or
        $library.snapshot_format.format.features.play_history_retired -ne 2 -or
        $library.snapshot_format.format.features.album_entities -ne 1 -or
        $library.snapshot_format.format.features.Contains('metadata_storage')) {
        throw 'Extracted server snapshot format differs from the current schema 8 contract.'
    }
    # Safe public result only: neither endpoint responses nor process logs are exported.
    [ordered]@{
        success=$true; tag=$Tag; version=$version; target=$Target; commit=$commit
        archive_sha256=$archiveHash; binary_sha256=$report.binary_sha256
        checks=@('archive checksum/report schema', 'public archive contents', 'extracted binary version/hash', 'isolated health API v1/schema 8/commit', 'schema 8 shared albums/plain Metadata format', 'synthetic library available')
        shutdown=$(if ($IsWindows) { 'owned test process termination; graceful drain not tested' } else { 'owned test process SIGTERM with bounded termination fallback' })
    } | ConvertTo-Json -Depth 4
} finally {
    $env:AUDIOLIB_UPDATE_DISABLE = $savedUpdateDisable
    if ($null -ne $http) { $http.Dispose() }
    if ($null -ne $handler) { $handler.Dispose() }
    # Only Process objects returned by this invocation's Start-Process are stopped.
    # Windows termination is deliberately not a server graceful-drain assertion.
    foreach ($process in $ownedProcesses) {
        try {
            if (!$process.HasExited) {
                if (!$IsWindows -and $process -eq $server) {
                    # PowerShell's `kill` alias is Stop-Process; select native kill.
                    $nativeKill = (Get-Command kill -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
                    & $nativeKill -TERM $process.Id 2>$null
                    $null = $process.WaitForExit(10000)
                }
                if (!$process.HasExited) { $process.Kill($true); $null = $process.WaitForExit(10000) }
                if (!$process.HasExited) { throw 'Owned server validation child did not exit.' }
            }
        } finally { $process.Dispose() }
    }
    if ($testOwned -and (Test-Path -LiteralPath $testRoot)) {
        Assert-Within $testRoot $tempParent
        if ((Split-Path -Leaf $testRoot) -cnotmatch '^AudioLibServerValidate-[0-9a-f]{32}$') { throw 'Unexpected server validation cleanup path.' }
        Assert-UnlinkedTree $testRoot
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
} finally {
    Restore-AudioLibConsoleEncoding
}
