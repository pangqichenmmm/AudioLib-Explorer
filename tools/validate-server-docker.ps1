# tools/validate-server-docker.ps1
# Accept an offline image using only containers, volumes and fixture copies owned by
# this invocation. Never starts/stops an installed server or accesses a working library.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [Parameter(Mandatory)][ValidateSet('linux/amd64','linux/arm64')][string]$Platform,
    [Parameter(Mandatory)][string]$DockerArchive,
    [Parameter(Mandatory)][string]$PackageReport,
    [Parameter(Mandatory)][string]$FixtureDirectory,
    [string]$DockerCommand = 'docker',
    [ValidateRange(30,600)][int]$TimeoutSeconds = 90
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Docker validation requires PowerShell 7.3 or newer.' }
Add-Type -AssemblyName System.Formats.Tar
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$match = [regex]::Match($Tag, '^v(\d+\.\d+\.\d+)(?:-(?:beta|rc)\.(?:0|[1-9]\d*))?$')
if (!$match.Success) { throw 'Invalid release tag.' }
$version = $match.Groups[1].Value; $fullVersion = $Tag.Substring(1); $arch = $Platform.Split('/')[1]
$imageTag = "audiolib-server:$fullVersion-linux-$arch"
$nonce = [guid]::NewGuid().ToString('N')
$label = "io.audiolib.validation=$nonce"
$testRoot = Join-Path $taskRoot ('.rust-validation/server-docker-validation/' + $nonce)
$containers = [Collections.Generic.List[string]]::new(); $volumes = [Collections.Generic.List[string]]::new()
$checks = [Collections.Generic.List[string]]::new()
$http = $null; $handler = $null; $tokenFile = $null; $failure = $null; $cleanupErrors = [Collections.Generic.List[string]]::new()
$success = $false; $runtimeStarted = $false; $archiveHash = $null; $report = $null; $daemon = $null

function Assert-Unlinked([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked validation paths are forbidden.' }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ($parent -ceq $current) { break }; $current = $parent
    }
}
function Get-Hash([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Invoke-Docker([string[]]$Arguments, [switch]$AllowFailure) {
    $output = @(& $DockerCommand @Arguments 2>&1); $code = $LASTEXITCODE
    if ($code -ne 0 -and !$AllowFailure) { throw "Owned Docker operation failed ($($Arguments[0]))." }
    return [pscustomobject]@{ ExitCode=$code; Text=($output -join "`n") }
}
function Inspect-Container([string]$Id, [string]$Template) { return (Invoke-Docker @('container','inspect','--format',$Template,$Id)).Text }
function Assert-OwnedContainer([string]$Id) {
    if ((Inspect-Container $Id '{{index .Config.Labels "io.audiolib.validation"}}') -cne $nonce) { throw 'Refusing an unowned Docker container.' }
}
function New-Volume([string]$Suffix) {
    $name = "audiolib-validation-$nonce-$Suffix"
    $null = Invoke-Docker @('volume','create','--label',$label,$name)
    $volumes.Add($name); return $name
}
function Start-Container([string]$Suffix, [string]$Volume, [switch]$SeedOnly) {
    $name = "audiolib-validation-$nonce-$Suffix"
    $arguments = @('container','create','--name',$name,'--label',$label,'--platform',$Platform,
        '--read-only','--security-opt','no-new-privileges','--tmpfs','/tmp:rw,nosuid,nodev',
        '--mount',"type=volume,src=$Volume,dst=/data")
    if ($SeedOnly) {
        $arguments += @('--user','0:0','--network','none','--mount',"type=bind,src=$fixtureCopy,dst=/fixture,readonly",'--entrypoint','/bin/sh',
            $report.image_id,'-ec','cp /fixture/data/library.db /data/library.db; chown 10001:10001 /data /data/library.db; chmod 755 /data; chmod 644 /data/library.db')
    } else {
        $arguments += @('--user','10001:10001','--env-file',$tokenFile,'--publish','127.0.0.1::8790',
            '--health-interval','1s','--health-start-period','1s','--health-retries','2',
            '--mount',"type=bind,src=$musicCopy,dst=/music,readonly",$report.image_id)
    }
    $id = (Invoke-Docker $arguments).Text.Trim()
    if ($id -cnotmatch '^[0-9a-f]{64}$') { throw 'Unexpected owned container ID.' }
    $containers.Add($id); Assert-OwnedContainer $id
    $null = Invoke-Docker @('container','start',$id)
    return $id
}
function Get-BaseUrl([string]$Id) {
    $port = Inspect-Container $Id '{{(index (index .NetworkSettings.Ports "8790/tcp") 0).HostPort}}'
    if ($port -cnotmatch '^\d+$') { throw 'Owned container has no loopback port.' }
    return "http://127.0.0.1:$port/api/v1"
}
function Request([string]$Path, [switch]$Authenticated, [switch]$Range) {
    $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Get, $baseUrl + $Path)
    try {
        if ($Authenticated) { $request.Headers.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer',$token) }
        if ($Range) { $request.Headers.Range = [Net.Http.Headers.RangeHeaderValue]::new(0,4095) }
        return $http.SendAsync($request).GetAwaiter().GetResult()
    } finally { $request.Dispose() }
}
function Read-JsonEndpoint([string]$Path) {
    $response = Request $Path -Authenticated
    try {
        if ([int]$response.StatusCode -ne 200) { throw 'Owned Docker server endpoint failed.' }
        return $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json -AsHashtable
    } finally { $response.Dispose() }
}
function Wait-Ready([string]$Id) {
    $deadline = [datetime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $health = $null
        try { $health = Read-JsonEndpoint '/health' } catch { }
        if ($null -ne $health -and $health.status -ceq 'ok') {
            if ($health.version -cne $version -or $health.commit -cne $ExpectedCommit.Substring(0,7) -or
                $health.api_version -cne 'v1' -or $health.schema_version -ne 8) { throw 'Docker health version/commit/API/schema mismatch.' }
            if ((Inspect-Container $Id '{{.State.Health.Status}}') -ceq 'healthy') { return }
        }
        if ((Inspect-Container $Id '{{.State.Running}}') -cne 'true') { throw 'Owned Docker server exited before readiness.' }
        Start-Sleep -Milliseconds 400
    } while ([datetime]::UtcNow -lt $deadline)
    throw 'Docker server readiness/healthcheck timed out.'
}
function Stop-Gracefully([string]$Id) {
    Assert-OwnedContainer $Id
    $null = Invoke-Docker @('container','stop','--time','30',$Id)
    if ((Inspect-Container $Id '{{.State.Running}}|{{.State.ExitCode}}|{{.State.OOMKilled}}') -cne 'false|0|false') { throw 'Owned Docker server did not exit cleanly through SIGTERM.' }
}
function Assert-DockerSaveArchive([string]$Path, [string]$ExpectedImageId, [string]$ExpectedTag, [switch]$Uncompressed) {
    # Accept the classic docker-save format only. Containerd prefers OCI index.json
    # over manifest.json: accepting a hybrid could import hidden foreign RepoTags.
    $inputFile = [IO.File]::OpenRead($Path)
    $source = if ($Uncompressed) { $inputFile } else { [IO.Compression.GZipStream]::new($inputFile, [IO.Compression.CompressionMode]::Decompress) }
    $tar = [Formats.Tar.TarReader]::new($source); $manifest = $null; $configHash = $null
    $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    try {
        while ($null -ne ($entry = $tar.GetNextEntry())) {
            if ([string]$entry.EntryType -ceq 'GlobalExtendedAttributes') { continue }
            $name = $entry.Name
            if ($name.StartsWith('./')) { $name = $name.Substring(2) }
            $name = $name.TrimEnd('/')
            if (!$name -and [string]$entry.EntryType -ceq 'Directory') { continue }
            if ($name -match '[\\:\x00-\x1f]' -or [IO.Path]::IsPathRooted($name) -or
                @($name.Split('/') | Where-Object { $_ -in @('', '.', '..') -or $_.EndsWith(' ') -or $_.EndsWith('.') }).Count -or !$names.Add($name) -or
                $names.Count -gt 50000 -or [string]$entry.EntryType -notin @('RegularFile','V7RegularFile','Directory')) { throw 'Unsafe/duplicate Docker archive entry.' }
            if ($name -in @('oci-layout','index.json')) { throw 'OCI/hybrid archives are unsupported; use a classic docker-save image store.' }
            if ($name -ceq 'manifest.json') {
                if ($entry.Length -gt 1048576) { throw 'Docker archive manifest is excessive.' }
                $reader = [IO.StreamReader]::new($entry.DataStream, [Text.Encoding]::UTF8, $true, 1024, $true)
                try { $manifest = $reader.ReadToEnd() | ConvertFrom-Json -AsHashtable -NoEnumerate } finally { $reader.Dispose() }
            }
            if ($name -ceq ($ExpectedImageId.Substring(7) + '.json')) {
                if ($entry.Length -gt 4194304) { throw 'Docker image configuration is excessive.' }
                $sha = [Security.Cryptography.SHA256]::Create()
                try { $configHash = [Convert]::ToHexString($sha.ComputeHash($entry.DataStream)).ToLowerInvariant() } finally { $sha.Dispose() }
            }
        }
    } finally { $tar.Dispose(); $source.Dispose(); $inputFile.Dispose() }
    if ($null -eq $manifest -or @($manifest).Count -ne 1 -or @($manifest[0].RepoTags).Count -ne 1 -or
        $manifest[0].RepoTags[0] -cne $ExpectedTag -or $configHash -cne $ExpectedImageId.Substring(7) -or
        $manifest[0].Config -cne ($ExpectedImageId.Substring(7) + '.json')) { throw 'Docker archive must contain exactly the accepted image and RepoTag.' }
}

try {
    foreach ($path in @($DockerArchive, ($DockerArchive + '.sha256'), $PackageReport, $FixtureDirectory, $testRoot)) { Assert-Unlinked $path }
    if ($testRoot.Contains(',')) { throw 'Docker bind-mount validation requires a workspace path without commas.' }
    $null = New-Item -ItemType Directory -Path $testRoot -Force
    if (!$IsWindows) { [IO.File]::SetUnixFileMode($testRoot, [IO.UnixFileMode]448) }
    $DockerArchive = (Resolve-Path -LiteralPath $DockerArchive).Path
    $archiveName = "AudioLibServer-$Tag-docker-linux-$arch.tar.gz"
    if ([IO.Path]::GetFileName($DockerArchive) -cne $archiveName) { throw 'Docker archive name/platform mismatch.' }
    $archiveHash = Get-Hash $DockerArchive
    $sidecar = (Get-Content -LiteralPath ($DockerArchive + '.sha256') -Raw).TrimEnd("`r","`n")
    if ($sidecar -cnotmatch ('^[0-9a-fA-F]{64}  ' + [regex]::Escape($archiveName) + '$') -or
        $sidecar.Substring(0,64).ToLowerInvariant() -cne $archiveHash) { throw 'Docker archive checksum mismatch.' }
    $report = Get-Content -LiteralPath $PackageReport -Raw | ConvertFrom-Json -AsHashtable
    if ($report.success -isnot [bool] -or !$report.success -or $report.tag -cne $Tag -or $report.version -cne $version -or
        $report.commit -cne $ExpectedCommit -or $report.platform -cne $Platform -or $report.image_tag -cne $imageTag -or
        $report.image_id -cnotmatch '^sha256:[0-9a-f]{64}$' -or $report.archive_sha256 -cne $archiveHash -or
        $report.runtime_validation -cne 'not-run') { throw 'Docker package report identity mismatch.' }
    Assert-DockerSaveArchive $DockerArchive $report.image_id $imageTag
    $checks.Add('archive checksum, single RepoTag and configuration image ID before load')
    $FixtureDirectory = (Resolve-Path -LiteralPath $FixtureDirectory).Path
    if (@(Get-ChildItem -LiteralPath $FixtureDirectory -Recurse -Force -Attributes ReparsePoint).Count) { throw 'Linked fixture tree is forbidden.' }
    $fixtureFiles = @(Get-ChildItem -LiteralPath $FixtureDirectory -Recurse -Force -File)
    $expectedFiles = @('data/library.db','music/synthetic.wav','fixture.json')
    if ($fixtureFiles.Count -ne 3 -or @($fixtureFiles | Where-Object { [IO.Path]::GetRelativePath($FixtureDirectory,$_.FullName).Replace('\','/') -cnotin $expectedFiles }).Count) { throw 'Fixture must contain only its owned database, WAV and manifest.' }
    $fixture = Get-Content -LiteralPath (Join-Path $FixtureDirectory 'fixture.json') -Raw | ConvertFrom-Json -AsHashtable
    if ($fixture.Count -ne 4 -or $fixture.schema_version -ne 1 -or $fixture.audio_id -isnot [long] -and $fixture.audio_id -isnot [int] -or
        $fixture.audio_id -lt 1 -or $fixture.recorded_path -cne '/music/synthetic.wav' -or $fixture.source_sha256 -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid synthetic stream fixture manifest.' }
    $sourceDb = Join-Path $FixtureDirectory 'data/library.db'; $sourceWav = Join-Path $FixtureDirectory 'music/synthetic.wav'
    if ((Get-Item -LiteralPath $sourceDb).Length -gt 67108864 -or (Get-Item -LiteralPath $sourceWav).Length -lt 4096 -or
        (Get-Item -LiteralPath $sourceWav).Length -gt 16777216 -or (Get-Hash $sourceWav) -cne $fixture.source_sha256) { throw 'Synthetic fixture size/hash mismatch.' }
    $dbHash = Get-Hash $sourceDb
    $fixtureCopy = Join-Path $testRoot 'fixture'; $musicCopy = Join-Path $fixtureCopy 'music'
    $null = New-Item -ItemType Directory -Path (Join-Path $fixtureCopy 'data'), $musicCopy -Force
    Copy-Item -LiteralPath $sourceDb -Destination (Join-Path $fixtureCopy 'data/library.db')
    Copy-Item -LiteralPath $sourceWav -Destination (Join-Path $musicCopy 'synthetic.wav')
    if (!$IsWindows) {
        foreach ($directory in @($fixtureCopy,(Join-Path $fixtureCopy 'data'),$musicCopy)) { [IO.File]::SetUnixFileMode($directory,[IO.UnixFileMode]493) }
        foreach ($file in @((Join-Path $fixtureCopy 'data/library.db'),(Join-Path $musicCopy 'synthetic.wav'))) { [IO.File]::SetUnixFileMode($file,[IO.UnixFileMode]420) }
    }
    $token = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32)).ToLowerInvariant()
    $tokenFile = Join-Path $testRoot 'token.env'
    [IO.File]::WriteAllText($tokenFile,"AUDIOLIB_SERVER_TOKEN=$token`n",[Text.UTF8Encoding]::new($false))
    if (!$IsWindows) { [IO.File]::SetUnixFileMode($tokenFile,[IO.UnixFileMode]384) }
    $daemon = (Invoke-Docker @('info','--format','{{.OSType}}|{{.Architecture}}|{{.ServerVersion}}')).Text.Split('|')
    if ($daemon.Count -ne 3 -or $daemon[0] -cne 'linux') { throw 'Docker validation requires a Linux container engine.' }
    $prior = Invoke-Docker @('image','inspect','--format','{{.Id}}',$imageTag) -AllowFailure
    if ($prior.ExitCode -eq 0 -and $prior.Text.Trim() -cne $report.image_id) { throw 'Refusing to overwrite a conflicting existing Docker tag.' }
    $null = Invoke-Docker @('image','load','--input',$DockerArchive)
    $image = (Invoke-Docker @('image','inspect','--format','{{.Id}}|{{.Os}}|{{.Architecture}}|{{.Config.User}}|{{index .Config.Labels "org.opencontainers.image.version"}}|{{index .Config.Labels "org.opencontainers.image.revision"}}|{{json .Config.Entrypoint}}|{{range .Config.Env}}{{if eq . "AUDIOLIB_UPDATE_DISABLE=1"}}disabled{{end}}{{end}}',$imageTag)).Text.Split('|')
    if ($image.Count -ne 8 -or $image[0] -cne $report.image_id -or $image[1] -cne 'linux' -or $image[2] -cne $arch -or
        $image[3] -cne '10001:10001' -or $image[4] -cne $fullVersion -or $image[5] -cne $ExpectedCommit -or
        $image[6] -cne '["/opt/audiolibserver/audiolib-server"]' -or $image[7] -cne 'disabled') { throw 'Loaded image runtime identity/security mismatch.' }
    $checks.Add('loaded image architecture, version, revision, UID, entrypoint and disabled updater')
    $volume = New-Volume 'data'
    $seed = Start-Container 'seed' $volume -SeedOnly
    $seedExit = (Invoke-Docker @('container','wait',$seed)).Text.Trim()
    if ($seedExit -cne '0') { throw 'Owned fixture data-volume seeding failed.' }
    $server = Start-Container 'server' $volume
    $runtimeStarted = $true
    $baseUrl = Get-BaseUrl $server
    $handler = [Net.Http.HttpClientHandler]::new(); $handler.UseProxy = $false
    $http = [Net.Http.HttpClient]::new($handler); $http.Timeout = [TimeSpan]::FromSeconds(4)
    Wait-Ready $server
    $checks.Add('health API v1/schema 8/version/commit and Docker healthcheck healthy')
    $response = Request '/health'
    try { if ([int]$response.StatusCode -ne 200) { throw 'Public container health endpoint failed.' } } finally { $response.Dispose() }
    $response = Request '/library'
    try { if ([int]$response.StatusCode -ne 401 -or @($response.Headers.WwwAuthenticate | Where-Object Scheme -EQ 'Bearer').Count -eq 0) { throw 'Container authentication boundary failed.' } } finally { $response.Dispose() }
    $library = Read-JsonEndpoint '/library'
    if ($library.stats.audio_files -ne 1 -or $library.schema_version -ne 8 -or !$library.library_id -or $library.database -cne 'library.db') { throw 'Container did not open the single-record synthetic database.' }
    $response = Request ("/audio/" + $fixture.audio_id + '/stream') -Authenticated -Range
    try {
        $bytes = $response.Content.ReadAsByteArrayAsync().GetAwaiter().GetResult()
        if ([int]$response.StatusCode -ne 206 -or !$response.Content.Headers.ContentType.MediaType.StartsWith('audio/') -or
            $bytes.Length -ne 4096 -or [Text.Encoding]::ASCII.GetString($bytes,0,4) -cnotin @('RIFF','RF64') -or
            $response.Content.Headers.ContentRange.From -ne 0 -or $response.Content.Headers.ContentRange.To -ne 4095) { throw 'Authenticated synthetic WAV Range stream failed.' }
    } finally { $response.Dispose() }
    $checks.Add('unauthenticated 401/Bearer, authenticated one-record library and RIFF/RF64 206 Range stream')
    $boundary = Inspect-Container $server '{{.HostConfig.ReadonlyRootfs}}|{{.Config.User}}|{{range .Mounts}}{{if eq .Destination "/music"}}music={{.RW}}{{end}}{{if eq .Destination "/data"}}data={{.RW}}{{end}}{{end}}'
    if ($boundary -cne 'true|10001:10001|data=truemusic=false' -and $boundary -cne 'true|10001:10001|music=falsedata=true') { throw 'Container read-only/music/data/UID mount boundary failed.' }
    $null = Invoke-Docker @('container','exec',$server,'/bin/sh','-ec','test "$(id -u)" = 10001; printf owned-persistence > /data/docker-validation-marker; test -w /tmp; if touch /music/.docker-validation-probe 2>/dev/null; then exit 1; fi; if touch /opt/audiolibserver/.docker-validation-probe 2>/dev/null; then exit 1; fi')
    $checks.Add('UID 10001, read-only root/music, writable data/tmp')
    Stop-Gracefully $server
    $null = Invoke-Docker @('container','start',$server)
    $baseUrl = Get-BaseUrl $server; Wait-Ready $server
    $restarted = Read-JsonEndpoint '/library'
    if ($restarted.library_id -cne $library.library_id -or $restarted.stats.audio_files -ne 1) { throw 'Container restart changed synthetic library identity/data.' }
    $null = Invoke-Docker @('container','exec',$server,'/bin/sh','-ec','test "$(cat /data/docker-validation-marker)" = owned-persistence')
    Stop-Gracefully $server
    $checks.Add('clean SIGTERM exit 0, same-container restart, library identity and data persistence')
    $emptyVolume = New-Volume 'missing'
    $missing = Start-Container 'missing' $emptyVolume
    $baseUrl = Get-BaseUrl $missing
    $deadline = [datetime]::UtcNow.AddSeconds($TimeoutSeconds); $missingRejected = $false
    do {
        $state = Inspect-Container $missing '{{.State.Running}}|{{.State.Health.Status}}'
        if ($state -ceq 'true|unhealthy') { $missingRejected = $true; break }
        if ($state.StartsWith('false|')) { throw 'Missing-library container exited; unhealthy-healthcheck acceptance was not established.' }
        Start-Sleep -Milliseconds 500
    } while ([datetime]::UtcNow -lt $deadline)
    if (!$missingRejected) { throw 'Missing library did not make the owned container unhealthy.' }
    $response = Request '/health'
    try { $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json -AsHashtable; if ($body.status -ceq 'ok') { throw 'Missing library falsely reported healthy.' } } finally { $response.Dispose() }
    $null = Invoke-Docker @('container','exec',$missing,'/bin/sh','-ec','test ! -e /data/library.db')
    Stop-Gracefully $missing
    $checks.Add('missing database remains absent and produces Docker unhealthy status')
    if ((Get-Hash $sourceDb) -cne $dbHash -or (Get-Hash $sourceWav) -cne $fixture.source_sha256 -or
        (Get-Hash (Join-Path $musicCopy 'synthetic.wav')) -cne $fixture.source_sha256) { throw 'Source fixture was modified.' }
    $success = $true
} catch { $failure = $_.Exception.Message }
finally {
    if ($null -ne $http) { $http.Dispose() }; if ($null -ne $handler) { $handler.Dispose() }
    foreach ($id in $containers) {
        try {
            Assert-OwnedContainer $id
            if (!$success) {
                $logs = Invoke-Docker @('container','logs','--tail','200',$id) -AllowFailure
                $logs.Text | Set-Content -LiteralPath (Join-Path $testRoot ($id.Substring(0,12) + '.private.log')) -Encoding utf8NoBOM
            }
            if ((Inspect-Container $id '{{.State.Running}}') -ceq 'true') { Stop-Gracefully $id }
            $null = Invoke-Docker @('container','rm',$id)
        } catch { $cleanupErrors.Add('Owned container retained because normal stop/removal failed.') }
    }
    foreach ($volumeName in $volumes) {
        try {
            if ((Invoke-Docker @('volume','inspect','--format','{{index .Labels "io.audiolib.validation"}}',$volumeName)).Text -cne $nonce) { throw 'Unowned volume.' }
            $null = Invoke-Docker @('volume','rm',$volumeName)
        } catch { $cleanupErrors.Add('Owned volume retained because ownership/removal could not be confirmed.') }
    }
    if ($null -ne $tokenFile -and (Test-Path -LiteralPath $tokenFile)) { Assert-Unlinked $tokenFile; Remove-Item -LiteralPath $tokenFile }
}
if ($cleanupErrors.Count) { $success = $false }
$result = [ordered]@{
    success=$success; tag=$Tag; version=$version; commit=$ExpectedCommit; platform=$Platform
    runtime_validation=$(if ($success) { 'passed' } elseif ($runtimeStarted) { 'failed' } else { 'not-run' }); checks=@($checks)
    archive_sha256=$archiveHash; image_id=$(if ($null -ne $report -and $report.Contains('image_id')) { $report.image_id } else { $null })
    docker_engine=$(if ($null -ne $daemon) { @{os=$daemon[0]; architecture=$daemon[1]; version=$daemon[2]} } else { $null })
    hardware_coverage='Docker Linux environment; physical host hardware is not asserted'
    healthcheck='image health command with 1-second acceptance interval'; shutdown='normal docker stop SIGTERM, exit 0, no forced termination'
    cleanup_errors=@($cleanupErrors)
}
if (Test-Path -LiteralPath $testRoot) {
    $reportPath = Join-Path $testRoot 'validation-report.json'
    $result | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding utf8NoBOM
    Write-Host "Private Docker runtime report: $reportPath"
}
$result | ConvertTo-Json -Depth 5
if (!$success) { if ($failure) { throw $failure }; throw 'Owned Docker cleanup failed; inspect private validation evidence.' }
