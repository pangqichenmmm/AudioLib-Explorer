# tools/package-server-docker.ps1
# Build an offline runtime image only from a native, formally accepted Linux package.
# No working checkout, database, credentials or private validation report enters Docker's context.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [Parameter(Mandatory)][ValidateSet('linux/amd64', 'linux/arm64')][string]$Platform,
    [Parameter(Mandatory)][string]$ServerArchive,
    [Parameter(Mandatory)][string]$ServerReport,
    [Parameter(Mandatory)][string]$ServerValidation,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$DockerCommand = 'docker',
    # Resume only an explicitly identified immutable image; labels and its actual
    # server binary hash are rechecked against the accepted native package.
    [ValidatePattern('^sha256:[0-9a-f]{64}$')][string]$ExistingImageId,
    [switch]$WithoutFfmpeg,
    [switch]$ValidateInputsOnly
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Docker packaging requires PowerShell 7.3 or newer.' }
Add-Type -AssemblyName System.Formats.Tar
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$match = [regex]::Match($Tag, '^v(\d+\.\d+\.\d+)(?:-(?:beta|rc)\.(?:0|[1-9]\d*))?$')
if (!$match.Success) { throw 'Invalid release tag.' }
$version = $match.Groups[1].Value
$fullVersion = $Tag.Substring(1)
$arch = $Platform.Split('/')[1]
$target = if ($arch -ceq 'amd64') { 'x86_64-unknown-linux-gnu' } else { 'aarch64-unknown-linux-gnu' }
$nativeSuffix = if ($arch -ceq 'amd64') { 'linux-x64' } else { 'linux-arm64' }
$imageTag = "audiolib-server:$fullVersion-linux-$arch"
$archiveName = "AudioLibServer-$Tag-docker-linux-$arch.tar.gz"
$ownedRoot = Join-Path $taskRoot ('.rust-validation/server-docker-package/' + [guid]::NewGuid().ToString('N'))
$context = Join-Path $ownedRoot 'context'
$payload = Join-Path $context 'payload'

function Assert-Unlinked([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked packaging paths are forbidden.' }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ($parent -ceq $current) { break }; $current = $parent
    }
}
function Read-Json([string]$Path) {
    Assert-Unlinked $Path
    return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json -AsHashtable
}
function Get-Hash([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Assert-Relative([string]$Path) {
    if (!$Path -or $Path -match '[\\:\x00-\x1f]' -or [IO.Path]::IsPathRooted($Path) -or
        @($Path.Split('/') | Where-Object { $_ -in @('', '.', '..') -or $_.EndsWith(' ') -or $_.EndsWith('.') }).Count) { throw 'Unsafe archive path.' }
}
function Invoke-Docker([string[]]$Arguments, [switch]$AllowFailure) {
    $output = @(& $DockerCommand @Arguments 2>&1)
    $code = $LASTEXITCODE
    if ($code -ne 0 -and !$AllowFailure) { throw "Docker command failed ($($Arguments[0])); output retained only in the private build log." }
    if ($Arguments[0] -in @('buildx', 'save')) { $output | Set-Content -LiteralPath (Join-Path $ownedRoot ($Arguments[0] + '.log')) -Encoding utf8NoBOM }
    return [pscustomobject]@{ ExitCode=$code; Text=($output -join "`n") }
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
function Get-Image([string]$Reference) {
    # Select fields explicitly: never export a full inspect object (it may contain secrets).
    $template = '{{json .Id}}|{{json .Os}}|{{json .Architecture}}|{{json .Config.User}}|{{json (index .Config.Labels "org.opencontainers.image.version")}}|{{json (index .Config.Labels "org.opencontainers.image.revision")}}|{{json .Config.Entrypoint}}|{{range .Config.Env}}{{if eq . "AUDIOLIB_UPDATE_DISABLE=1"}}disabled{{end}}{{end}}'
    $result = Invoke-Docker @('image', 'inspect', '--format', $template, $Reference) -AllowFailure
    if ($result.ExitCode -ne 0) { return $null }
    $fields = $result.Text.Split('|')
    if ($fields.Count -ne 8) { throw 'Unexpected image identity response.' }
    return @($fields[0..6] | ForEach-Object { ConvertFrom-Json -InputObject $_ -NoEnumerate }) + @($fields[7])
}
function Export-LegacyDockerSave([string]$Path, [string]$Destination, [string]$ExpectedImageId, [string]$ExpectedTag) {
    # This is producer-side normalization of our own docker-save output, never an
    # import. Modern stores include an OCI index alongside the legacy manifest.
    # Reconstruct ONLY the single accepted manifest, its config and its layers;
    # no OCI index, repository map, attestation or unreferenced blob is exported.
    $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $regular = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $configs = @{}; $manifest = $null
    $inputFile = [IO.File]::OpenRead($Path); $reader = [Formats.Tar.TarReader]::new($inputFile)
    try {
        while ($null -ne ($entry = $reader.GetNextEntry())) {
            if ([string]$entry.EntryType -ceq 'GlobalExtendedAttributes') { continue }
            $name = $entry.Name
            if ($name.StartsWith('./')) { $name = $name.Substring(2) }
            $name = $name.TrimEnd('/')
            if (!$name -and [string]$entry.EntryType -ceq 'Directory') { continue }
            Assert-Relative $name
            if (!$names.Add($name) -or $names.Count -gt 50000 -or [string]$entry.EntryType -notin @('RegularFile','V7RegularFile','Directory')) { throw 'Unsafe/duplicate producer Docker archive entry.' }
            if ([string]$entry.EntryType -ceq 'Directory') { continue }
            $null = $regular.Add($name)
            if ($name -ceq 'manifest.json' -or $name -cin @(($ExpectedImageId.Substring(7) + '.json'),('blobs/sha256/' + $ExpectedImageId.Substring(7)))) {
                if ($entry.Length -gt 4194304) { throw 'Producer image metadata is excessive.' }
                $memory = [IO.MemoryStream]::new()
                try { $entry.DataStream.CopyTo($memory); $bytes = $memory.ToArray() } finally { $memory.Dispose() }
                if ($name -ceq 'manifest.json') { $manifest = [Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json -AsHashtable -NoEnumerate }
                else { $configs[$name] = $bytes }
            }
        }
    } finally { $reader.Dispose(); $inputFile.Dispose() }
    if ($null -eq $manifest -or @($manifest).Count -ne 1 -or @($manifest[0].RepoTags).Count -ne 1 -or
        $manifest[0].RepoTags[0] -cne $ExpectedTag -or !$configs.ContainsKey($manifest[0].Config) -or
        $manifest[0].Layers -isnot [array] -or !$manifest[0].Layers.Count -or $manifest[0].Layers.Count -gt 128) { throw 'Producer Docker manifest identity/layers mismatch.' }
    $configBytes = $configs[$manifest[0].Config]
    if ([Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($configBytes)).ToLowerInvariant() -cne $ExpectedImageId.Substring(7)) { throw 'Producer image config digest mismatch.' }
    $config = [Text.Encoding]::UTF8.GetString($configBytes) | ConvertFrom-Json -AsHashtable
    if ($config.rootfs.type -cne 'layers' -or $config.rootfs.diff_ids -isnot [array] -or $config.rootfs.diff_ids.Count -ne $manifest[0].Layers.Count) { throw 'Producer image layer DiffIDs are incomplete.' }
    $layerIndexes = [Collections.Generic.Dictionary[string,int]]::new([StringComparer]::Ordinal)
    $canonicalLayers = [Collections.Generic.List[string]]::new()
    for ($index=0; $index -lt $manifest[0].Layers.Count; $index++) {
        $name = $manifest[0].Layers[$index]; Assert-Relative $name
        if (!$regular.Contains($name) -or $name -ceq 'manifest.json' -or $name -ceq $manifest[0].Config -or
            $name -in @('oci-layout','index.json','repositories') -or $layerIndexes.ContainsKey($name) -or
            $config.rootfs.diff_ids[$index] -cnotmatch '^sha256:[0-9a-f]{64}$') { throw 'Producer manifest references an unsafe/missing/duplicate layer.' }
        $layerIndexes.Add($name,$index); $canonicalLayers.Add(('layers/{0:D4}.tar' -f $index))
    }
    $canonical = @([ordered]@{Config=($ExpectedImageId.Substring(7)+'.json');RepoTags=@($ExpectedTag);Layers=@($canonicalLayers)})
    $canonicalBytes = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $canonical -Depth 5 -Compress))
    $outputFile = [IO.File]::Open($Destination,[IO.FileMode]::CreateNew)
    $writer = [Formats.Tar.TarWriter]::new($outputFile)
    try {
        foreach ($metadata in @(@{Name='manifest.json';Bytes=$canonicalBytes},@{Name=($ExpectedImageId.Substring(7)+'.json');Bytes=$configBytes})) {
            $entry = [Formats.Tar.PaxTarEntry]::new([Formats.Tar.TarEntryType]::RegularFile,$metadata.Name)
            $entry.Mode = [IO.UnixFileMode]420
            $memory = [IO.MemoryStream]::new([byte[]]$metadata.Bytes)
            try { $entry.DataStream=$memory; $writer.WriteEntry($entry) } finally { $memory.Dispose() }
        }
        $inputFile = [IO.File]::OpenRead($Path); $reader = [Formats.Tar.TarReader]::new($inputFile)
        try {
            while ($null -ne ($entry = $reader.GetNextEntry())) {
                $name=$entry.Name; if ($name.StartsWith('./')) { $name=$name.Substring(2) }
                if (!$layerIndexes.ContainsKey($name)) { continue }
                $index=$layerIndexes[$name]; $data=$entry.DataStream
                if (!$data.CanSeek) { throw 'Producer layer stream must be seekable.' }
                $position=$data.Position; $first=$data.ReadByte(); $second=$data.ReadByte(); $data.Position=$position
                $sha=[Security.Cryptography.SHA256]::Create(); $decoded=$null
                try {
                    if ($first -eq 31 -and $second -eq 139) { $decoded=[IO.Compression.GZipStream]::new($data,[IO.Compression.CompressionMode]::Decompress,$true); $digest=$sha.ComputeHash($decoded) }
                    else { $digest=$sha.ComputeHash($data) }
                } finally { if ($null -ne $decoded) { $decoded.Dispose() }; $sha.Dispose() }
                $data.Position=$position
                if (('sha256:'+ [Convert]::ToHexString($digest).ToLowerInvariant()) -cne $config.rootfs.diff_ids[$index]) { throw 'Producer layer content does not match its config DiffID.' }
                $copy=[Formats.Tar.PaxTarEntry]::new([Formats.Tar.TarEntryType]::RegularFile,$canonicalLayers[$index]); $copy.Mode=[IO.UnixFileMode]420
                $copy.DataStream=$data; $writer.WriteEntry($copy)
            }
        } finally { $reader.Dispose(); $inputFile.Dispose() }
    } finally { $writer.Dispose(); $outputFile.Dispose() }
}

foreach ($path in @($ServerArchive, ($ServerArchive + '.sha256'), $ServerReport, $ServerValidation, $OutputDirectory, $ownedRoot)) { Assert-Unlinked $path }
$ServerArchive = (Resolve-Path -LiteralPath $ServerArchive).Path
if ([IO.Path]::GetFileName($ServerArchive) -cne "AudioLibServer-$Tag-$nativeSuffix.tar.gz") { throw 'Native server archive name/platform mismatch.' }
$archiveHash = Get-Hash $ServerArchive
$sidecar = (Get-Content -LiteralPath ($ServerArchive + '.sha256') -Raw).TrimEnd("`r", "`n")
if ($sidecar -cnotmatch ('^[0-9a-fA-F]{64}  ' + [regex]::Escape([IO.Path]::GetFileName($ServerArchive)) + '$') -or
    $sidecar.Substring(0,64).ToLowerInvariant() -cne $archiveHash) { throw 'Native server archive checksum mismatch.' }
$report = Read-Json $ServerReport
$validation = Read-Json $ServerValidation
foreach ($proof in @($report, $validation)) {
    if ($proof.success -isnot [bool] -or !$proof.success -or $proof.tag -cne $Tag -or $proof.version -cne $version -or
        $proof.commit -cne $ExpectedCommit -or $proof.target -cne $target -or
        $proof.archive_sha256.ToLowerInvariant() -cne $archiveHash -or $proof.binary_sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'Native Linux acceptance proof identity mismatch.'
    }
}
foreach ($check in @('version', 'allowlist', 'unlinked_paths', 'privacy', 'archive_contents', 'executable_mode')) {
    if ($report.checks[$check] -isnot [bool] -or !$report.checks[$check]) { throw 'Native package boundary proof is incomplete.' }
}
if ($validation.binary_sha256 -ine $report.binary_sha256 -or $validation.checks -isnot [array] -or !$validation.checks.Count) { throw 'Native runtime proof is incomplete.' }
# Pin the tiny public context to the requested source revision. No .git directory is copied.
Push-Location $taskRoot
try {
    $head = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $head -cne $ExpectedCommit) { throw 'Docker context checkout is not the expected commit.' }
    $contextSources = @('server/deploy/docker/Dockerfile.release', 'server/deploy/docker/config.docker.toml', 'tools/server-files.json')
    foreach ($source in $contextSources) {
        Assert-Unlinked (Join-Path $taskRoot $source)
        $null = & git ls-files --error-unmatch -- $source 2>$null
        if ($LASTEXITCODE -ne 0) { throw 'Docker context source must be committed.' }
    }
    & git diff --quiet HEAD -- @contextSources
    if ($LASTEXITCODE -ne 0) { throw 'Docker context sources differ from the expected commit.' }
} finally { Pop-Location }
function Expand-PublicPayload {
$publicFiles = @(Get-Content -LiteralPath (Join-Path $taskRoot 'tools/server-files.json') -Raw | ConvertFrom-Json)
$allowed = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($name in @($publicFiles) + @('audiolib-server', 'update-channel.txt', 'update-version.txt', 'licenses/dependencies.json')) { Assert-Relative $name; $null = $allowed.Add($name) }
$null = New-Item -ItemType Directory -Path $payload -Force
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$files = [Collections.Generic.List[string]]::new()
$directories = [Collections.Generic.List[string]]::new()
$stream = [IO.File]::OpenRead($ServerArchive)
$gzip = [IO.Compression.GZipStream]::new($stream, [IO.Compression.CompressionMode]::Decompress)
$tar = [Formats.Tar.TarReader]::new($gzip)
try {
    while ($null -ne ($entry = $tar.GetNextEntry())) {
        $kind = [string]$entry.EntryType
        if ($kind -ceq 'GlobalExtendedAttributes') { continue }
        if ($kind -notin @('RegularFile', 'V7RegularFile', 'Directory')) { throw 'Links and special archive entries are forbidden.' }
        $name = $entry.Name
        if ($name.StartsWith('./')) { $name = $name.Substring(2) }
        $name = $name.TrimEnd('/')
        if (!$name -and $kind -ceq 'Directory') { continue }
        Assert-Relative $name
        if (!$seen.Add($name) -or $seen.Count -gt 20000) { throw 'Duplicate, case-colliding or excessive archive entries.' }
        $destination = [IO.Path]::GetFullPath((Join-Path $payload $name))
        if (!$destination.StartsWith($payload + [IO.Path]::DirectorySeparatorChar, [StringComparison]::Ordinal)) { throw 'Archive escaped its owned payload.' }
        if ($kind -ceq 'Directory') {
            if (([int]$entry.Mode -band 511) -ne 493) { throw 'Native package directory mode mismatch.' }
            $directories.Add($name)
            $null = New-Item -ItemType Directory -Path $destination -Force; continue
        }
        if ($entry.Length -gt 1073741824) { throw 'Unexpectedly large public package entry.' }
        if (!$allowed.Contains($name) -and !$name.StartsWith('licenses/', [StringComparison]::Ordinal)) { throw 'Non-public file in native server archive.' }
        $mode = [int]$entry.Mode -band 511
        $wantedMode = if ($name -in @('audiolib-server', 'server/deploy/redeploy-server.sh')) { 493 } else { 420 }
        if ($mode -ne $wantedMode) { throw 'Native package file mode mismatch.' }
        $null = New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($destination)) -Force
        $output = [IO.File]::Open($destination, [IO.FileMode]::CreateNew)
        try { $entry.DataStream.CopyTo($output) } finally { $output.Dispose() }
        if (!$IsWindows) { [IO.File]::SetUnixFileMode($destination, [IO.UnixFileMode]$wantedMode) }
        $files.Add($name)
    }
} finally { $tar.Dispose(); $gzip.Dispose(); $stream.Dispose() }
foreach ($name in $allowed) { if (!$files.Contains($name)) { throw 'Required public package file is missing.' } }
$dependencies = Read-Json (Join-Path $payload 'licenses/dependencies.json')
$licenseRoots = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($dependency in $dependencies) {
    if ($dependency.name -cnotmatch '^[A-Za-z0-9_-]+$' -or $dependency.version -cnotmatch '^[A-Za-z0-9.+_-]+$' -or
        @($dependency.Keys | Where-Object { $_ -notin @('name','version','license','repository') }).Count -or
        ($null -ne $dependency.repository -and $dependency.repository -cnotmatch '^https?://[^/@\s]+(?:/[^\s]*)?$')) { throw 'Unsafe dependency license manifest.' }
    $null = $licenseRoots.Add('licenses/' + $dependency.name + '-' + $dependency.version)
}
if (!$licenseRoots.Count) { throw 'Dependency license closure is empty.' }
foreach ($name in $files) {
    if ($allowed.Contains($name)) { continue }
    $parts = $name.Split('/')
    if ($parts.Count -lt 3 -or !$licenseRoots.Contains($parts[0] + '/' + $parts[1]) -or
        $name -match '(?i)(?:^|/)(?:\.env(?:\..*)?|settings\.json|.*\.(?:db|sqlite|sqlite3|wav|log)|.*-(?:wal|shm|journal))$') { throw 'Unexpected/private file in dependency license closure.' }
}
foreach ($directory in $directories) {
    if (!@($files | Where-Object { $_.StartsWith($directory + '/', [StringComparison]::Ordinal) }).Count) { throw 'Unexpected empty/non-public package directory.' }
}
if ((Get-Hash (Join-Path $payload 'audiolib-server')) -cne $report.binary_sha256.ToLowerInvariant() -or
    (Get-Content -LiteralPath (Join-Path $payload 'update-version.txt') -Raw).Trim() -cne $fullVersion -or
    (Get-Content -LiteralPath (Join-Path $payload 'update-channel.txt') -Raw).Trim() -cne $(if ($Tag.Contains('-')) { 'beta' } else { 'stable' })) { throw 'Native payload binary/update identity mismatch.' }
}
Expand-PublicPayload
Copy-Item -LiteralPath (Join-Path $taskRoot 'server/deploy/docker/Dockerfile.release') -Destination (Join-Path $context 'Dockerfile')
Copy-Item -LiteralPath (Join-Path $taskRoot 'server/deploy/docker/config.docker.toml') -Destination (Join-Path $context 'config.docker.toml')
Set-Content -LiteralPath (Join-Path $context '.dockerignore') -Encoding utf8NoBOM -Value @('*', '!Dockerfile', '!config.docker.toml', '!payload/', '!payload/**')
if ($ValidateInputsOnly) {
    [ordered]@{ success=$true; tag=$Tag; commit=$ExpectedCommit; platform=$Platform; native_archive_sha256=$archiveHash; checks=@('native package and runtime proofs','safe public payload extraction'); runtime_validation='not-run'; docker_build='not-run' } | ConvertTo-Json
    return
}
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$null = New-Item -ItemType Directory -Path $OutputDirectory -Force
$published = Join-Path $OutputDirectory $archiveName
if ((Test-Path -LiteralPath $published) -or (Test-Path -LiteralPath ($published + '.sha256'))) { throw 'Refusing to replace an existing Docker delivery artifact.' }
$prior = Get-Image $imageTag
if ($ExistingImageId) {
    if ($null -eq $prior -or $prior[0] -cne $ExistingImageId) { throw 'Resume image ID does not match the existing expected tag.' }
} elseif ($null -ne $prior) { throw 'Expected image tag already exists; use a clean owned Docker image store or explicitly resolve the collision first.' }
$ffmpeg = if ($WithoutFfmpeg) { 'false' } else { 'true' }
if (!$ExistingImageId) { $null = Invoke-Docker @('buildx','build','--platform',$Platform,'--load','--build-arg',"VERSION=$fullVersion",'--build-arg',"REVISION=$ExpectedCommit",'--build-arg',"WITH_FFMPEG=$ffmpeg",'--tag',$imageTag,$context) }
$image = Get-Image $imageTag
if ($null -eq $image -or ($ExistingImageId -and $image[0] -cne $ExistingImageId) -or $image[0] -cnotmatch '^sha256:[0-9a-f]{64}$' -or $image[1] -cne 'linux' -or $image[2] -cne $arch -or
    $image[3] -cne '10001:10001' -or $image[4] -cne $fullVersion -or $image[5] -cne $ExpectedCommit -or
    @($image[6]).Count -ne 1 -or $image[6][0] -cne '/opt/audiolibserver/audiolib-server' -or $image[7] -cne 'disabled') { throw 'Built image identity/security boundary mismatch.' }
if ($ExistingImageId) {
    $hashProbe = Invoke-Docker @('run','--rm','--name',('audiolib-package-hash-'+[guid]::NewGuid().ToString('N')),'--label','io.audiolib.task=package-hash','--platform',$Platform,'--read-only','--network','none','--security-opt','no-new-privileges','--user','10001:10001','--tmpfs','/data:rw,nosuid,nodev','--entrypoint','/usr/bin/sha256sum',$ExistingImageId,'/opt/audiolibserver/audiolib-server')
    if (($hashProbe.Text -split '\s+')[0] -cne $report.binary_sha256.ToLowerInvariant()) { throw 'Resumed image binary differs from the accepted native package.' }
}
$originalTar = Join-Path $ownedRoot 'image-original.tar'
$rawTar = Join-Path $ownedRoot 'image.tar'
$null = Invoke-Docker @('save', '--output', $originalTar, $imageTag)
Export-LegacyDockerSave $originalTar $rawTar $image[0] $imageTag
Assert-DockerSaveArchive $rawTar $image[0] $imageTag -Uncompressed
if ((Get-Image $imageTag)[0] -cne $image[0]) { throw 'Image tag changed during packaging.' }
$staged = Join-Path $ownedRoot $archiveName
$inputFile = [IO.File]::OpenRead($rawTar); $outputFile = [IO.File]::Create($staged)
$compress = [IO.Compression.GZipStream]::new($outputFile, [IO.Compression.CompressionLevel]::Optimal)
try { $inputFile.CopyTo($compress) } finally { $compress.Dispose(); $outputFile.Dispose(); $inputFile.Dispose() }
$hash = Get-Hash $staged
$privateReport = Join-Path $ownedRoot 'package-report.json'
[ordered]@{
    success=$true; tag=$Tag; version=$version; commit=$ExpectedCommit; platform=$Platform; target=$target
    image_tag=$imageTag; image_id=$image[0]; archive_sha256=$hash; native_archive_sha256=$archiveHash
    binary_sha256=$report.binary_sha256.ToLowerInvariant(); with_ffmpeg=(!$WithoutFfmpeg)
    checks=@('matching native Linux acceptance','public tiny context','image OS/architecture/version/revision/user/entrypoint/update boundary','image save')
    image_build=$(if ($ExistingImageId) { 'reused explicit immutable image; accepted native binary hash reverified' } else { 'built' })
    archive_format='closed classic docker-save manifest/config/DiffID-verified layers'
    runtime_validation='not-run'
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $privateReport -Encoding utf8NoBOM
Copy-Item -LiteralPath $staged -Destination $published
Set-Content -LiteralPath ($published + '.sha256') -Value "$hash  $archiveName" -Encoding utf8NoBOM
[ordered]@{ success=$true; archive=$published; sha256=$hash; report=$privateReport; runtime_validation='not-run' } | ConvertTo-Json
