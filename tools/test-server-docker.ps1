# tools/test-server-docker.ps1
# Offline regression tests for the actual archive readers and pre-Docker proof gate.
# Creates only synthetic files below .rust-validation; never invokes a Docker daemon.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Docker boundary tests require PowerShell 7.3 or newer.' }
Add-Type -AssemblyName System.Formats.Tar
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$root = Join-Path $taskRoot ('.rust-validation/server-docker-boundaries/' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $root -Force
$Tag = 'v0.8.4'; $fullVersion = '0.8.4'; $passed = 0
$publicFiles = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'server-files.json') -Raw | ConvertFrom-Json
function Import-TestFunctions([string]$File, [string[]]$Names) {
    # Import function ASTs without executing the tool's packaging/runtime entry point.
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($File, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw 'Docker tool has a PowerShell syntax error.' }
    foreach ($name in $Names) {
        $node = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $true)
        if ($null -eq $node) { throw 'Archive test function is unavailable.' }
        Invoke-Expression ($node.Extent.Text -replace ('^function ' + [regex]::Escape($name)), ('function script:' + $name))
    }
}
function Write-Tar([string]$Path, [Collections.IDictionary]$Files, [string]$Extra = '', [switch]$Raw) {
    $file = [IO.File]::Create($Path)
    $destination = if ($Raw) { $file } else { [IO.Compression.GZipStream]::new($file,[IO.Compression.CompressionLevel]::Optimal) }
    $writer = [Formats.Tar.TarWriter]::new($destination)
    try {
        foreach ($pair in $Files.GetEnumerator()) {
            $entry = [Formats.Tar.PaxTarEntry]::new([Formats.Tar.TarEntryType]::RegularFile,$pair.Key)
            $entry.Mode = [IO.UnixFileMode]$(if ($pair.Key -in @('audiolib-server','server/deploy/redeploy-server.sh')) { 493 } else { 420 })
            if ($Extra -ceq 'bad-mode' -and $pair.Key -ceq 'audiolib-server') { $entry.Mode = [IO.UnixFileMode]420 }
            $entryBytes = if ($pair.Value -is [array]) { [byte[]]$pair.Value } else { [Text.Encoding]::UTF8.GetBytes($pair.Value) }
            $memory = [IO.MemoryStream]::new([byte[]]$entryBytes)
            $entry.DataStream = $memory; $writer.WriteEntry($entry); $memory.Dispose()
        }
        if ($Extra -in @('symlink','hardlink')) {
            $kind = if ($Extra -ceq 'symlink') { [Formats.Tar.TarEntryType]::SymbolicLink } else { [Formats.Tar.TarEntryType]::HardLink }
            $entry = [Formats.Tar.PaxTarEntry]::new($kind,'licenses/fixture-1.0.0/link')
            $entry.LinkName = '/etc/passwd'; $writer.WriteEntry($entry)
        }
        if ($Extra -ceq 'empty-private-dir') {
            $entry = [Formats.Tar.PaxTarEntry]::new([Formats.Tar.TarEntryType]::Directory,'private')
            $entry.Mode = [IO.UnixFileMode]493; $writer.WriteEntry($entry)
        }
        if ($Extra -ceq 'duplicate') {
            $entry = [Formats.Tar.PaxTarEntry]::new([Formats.Tar.TarEntryType]::RegularFile,'manifest.json')
            $memory = [IO.MemoryStream]::new([Text.Encoding]::UTF8.GetBytes('[]'))
            $entry.DataStream=$memory; $writer.WriteEntry($entry); $memory.Dispose()
        }
        if ($Extra -ceq 'case-collision') {
            $entry = [Formats.Tar.PaxTarEntry]::new([Formats.Tar.TarEntryType]::RegularFile,'license')
            $entry.Mode = [IO.UnixFileMode]420
            $memory = [IO.MemoryStream]::new([Text.Encoding]::UTF8.GetBytes('collision'))
            $entry.DataStream=$memory; $writer.WriteEntry($entry); $memory.Dispose()
        }
    } finally { $writer.Dispose(); $destination.Dispose(); $file.Dispose() }
}
Import-TestFunctions (Join-Path $PSScriptRoot 'package-server-docker.ps1') @('Assert-Unlinked','Read-Json','Get-Hash','Assert-Relative','Expand-PublicPayload')
foreach ($case in @('valid','private-file','traversal','windows-traversal','case-collision','unknown-license','private-license','wrong-marker','bad-mode','symlink','hardlink','empty-private-dir','binary-hash')) {
    $caseRoot = Join-Path $root ('native-' + $case)
    $null = New-Item -ItemType Directory -Path $caseRoot
    $payload = Join-Path $caseRoot 'payload'; $ServerArchive = Join-Path $caseRoot 'native.tar.gz'
    $files = [ordered]@{}
    foreach ($name in $publicFiles) { $files[$name] = 'public fixture' }
    $files['audiolib-server'] = 'synthetic binary'; $files['update-version.txt'] = $fullVersion; $files['update-channel.txt'] = 'stable'
    $files['licenses/dependencies.json'] = '[{"name":"fixture","version":"1.0.0","license":"MIT","repository":null}]'
    $files['licenses/fixture-1.0.0/LICENSE'] = 'public MIT text'
    switch ($case) {
        'unknown-license' { $files['licenses/unknown-1.0.0/LICENSE'] = 'unknown' }
        'private-license' { $files['licenses/fixture-1.0.0/local.db'] = 'private' }
        'private-file' { $files['library.db'] = 'private' }
        'traversal' { $files['../escape'] = 'escape' }
        'windows-traversal' { $files['licenses/fixture-1.0.0/.. /.. /escape'] = 'escape' }
        'wrong-marker' { $files['update-version.txt'] = '0.8.3' }
    }
    Write-Tar $ServerArchive $files $case
    $report = @{ binary_sha256=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes('synthetic binary'))) }
    if ($case -ceq 'binary-hash') { $report.binary_sha256 = '0' * 64 }
    $caught = $null
    try { Expand-PublicPayload } catch { $caught = $_.Exception.Message }
    $expectedFailure = switch ($case) {
        'private-file' { 'Non-public file' }
        { $_ -in @('traversal','windows-traversal') } { 'Unsafe archive path' }
        'case-collision' { 'case-colliding' }
        { $_ -in @('unknown-license','private-license') } { 'Unexpected/private file' }
        { $_ -in @('wrong-marker','binary-hash') } { 'identity mismatch' }
        'bad-mode' { 'file mode mismatch' }
        { $_ -in @('symlink','hardlink') } { 'Links and special' }
        'empty-private-dir' { 'empty/non-public' }
        default { '' }
    }
    if (($case -ceq 'valid' -and $caught) -or ($case -cne 'valid' -and (!$caught -or $caught -notmatch $expectedFailure))) { throw "Native payload boundary failed: $case ($caught)" }
    $passed++; Write-Host "PASS native payload: $case"
}
$imageConfig = '{"architecture":"amd64","os":"linux","config":{"User":"10001:10001"}}'
$imageHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($imageConfig))).ToLowerInvariant()
$imageId = 'sha256:' + $imageHash; $expectedTag = 'audiolib-server:0.8.4-linux-amd64'
foreach ($reader in @('package-server-docker.ps1','validate-server-docker.ps1')) {
    Import-TestFunctions (Join-Path $PSScriptRoot $reader) @('Assert-DockerSaveArchive')
    foreach ($case in @('valid','foreign-tag','extra-tag','extra-image','config-hash','oci-foreign-tag','oci-index','symlink','traversal','duplicate')) {
        $archive = Join-Path $root ($reader + '-' + $case + '.tar.gz')
        $manifest = @(@{ Config=($imageHash + '.json'); RepoTags=@($expectedTag); Layers=@() })
        if ($case -ceq 'foreign-tag') { $manifest[0].RepoTags = @('foreign:existing') }
        if ($case -ceq 'extra-tag') { $manifest[0].RepoTags += 'foreign:existing' }
        if ($case -ceq 'extra-image') { $manifest += @{ Config=($imageHash + '.json'); RepoTags=@('foreign:existing'); Layers=@() } }
        $files = [ordered]@{ 'manifest.json'=ConvertTo-Json -InputObject $manifest -Depth 6 -Compress }
        $files[$imageHash + '.json'] = if ($case -ceq 'config-hash') { '{}' } else { $imageConfig }
        if ($case -ceq 'oci-foreign-tag') {
            $files['oci-layout'] = '{"imageLayoutVersion":"1.0.0"}'
            $files['index.json'] = '{"schemaVersion":2,"manifests":[{"annotations":{"io.containerd.image.name":"foreign:existing"}}]}'
        }
        if ($case -ceq 'oci-index') { $files['index.json']='{"schemaVersion":2,"manifests":[]}' }
        if ($case -ceq 'traversal') { $files['../escape']='escape' }
        Write-Tar $archive $files $case
        $caught = $null
        try { Assert-DockerSaveArchive $archive $imageId $expectedTag } catch { $caught=$_.Exception.Message }
        if (($case -ceq 'valid' -and $caught) -or ($case -cne 'valid' -and !$caught)) { throw "Docker archive boundary failed: $reader/$case ($caught)" }
        $passed++; Write-Host "PASS $reader saved image: $case"
    }
}
# Modern producer saves are reconstructed into a closed legacy archive. Exercise
# the actual normalizer with uncompressed/gzip layers and hidden OCI metadata.
Import-TestFunctions (Join-Path $PSScriptRoot 'package-server-docker.ps1') @('Export-LegacyDockerSave','Assert-Relative','Assert-DockerSaveArchive')
$innerLayer = Join-Path $root 'synthetic-layer.tar'
Write-Tar $innerLayer ([ordered]@{'etc/public-fixture'='public synthetic layer'}) -Raw
$layerBytes = [IO.File]::ReadAllBytes($innerLayer)
$diffId = 'sha256:' + [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($layerBytes)).ToLowerInvariant()
foreach ($case in @('hybrid','gzip-layer','foreign-legacy-tag','extra-legacy-image','config-digest','layer-diffid','missing-layer','symlink')) {
    $config = @{architecture='amd64';os='linux';rootfs=@{type='layers';diff_ids=@($(if ($case -ceq 'layer-diffid') {'sha256:'+('0'*64)} else {$diffId}))}}
    $configBytes = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $config -Depth 5 -Compress))
    $configId = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($configBytes)).ToLowerInvariant()
    $configName='blobs/sha256/'+$configId; $layerName='blobs/sha256/'+$diffId.Substring(7)
    $manifest=@(@{Config=$configName;RepoTags=@($expectedTag);Layers=@($layerName)})
    if ($case -ceq 'foreign-legacy-tag') { $manifest[0].RepoTags=@('foreign:existing') }
    if ($case -ceq 'extra-legacy-image') { $manifest+=@{Config=$configName;RepoTags=@('foreign:existing');Layers=@($layerName)} }
    $files=[ordered]@{'manifest.json'=ConvertTo-Json -InputObject $manifest -Depth 5 -Compress; 'oci-layout'='{"imageLayoutVersion":"1.0.0"}';'index.json'='{"schemaVersion":2,"manifests":[{"annotations":{"io.containerd.image.name":"foreign:existing"}}]}';'repositories'='{"foreign":{"existing":"hidden"}}'}
    $files[$configName] = if ($case -ceq 'config-digest') { [Text.Encoding]::UTF8.GetBytes('{}') } else { $configBytes }
    $files['blobs/sha256/'+('c'*64)]='private unreferenced build metadata'
    if ($case -cne 'missing-layer') { $files[$layerName]=$layerBytes }
    if ($case -ceq 'gzip-layer') {
        $memory=[IO.MemoryStream]::new();$gzip=[IO.Compression.GZipStream]::new($memory,[IO.Compression.CompressionLevel]::Optimal,$true)
        try{$gzip.Write($layerBytes)}finally{$gzip.Dispose()}
        $files[$layerName]=$memory.ToArray();$memory.Dispose()
    }
    $raw=Join-Path $root ('producer-'+$case+'.tar');$normalized=Join-Path $root ('normalized-'+$case+'.tar')
    Write-Tar $raw $files $case -Raw
    $caught=$null
    try{Export-LegacyDockerSave $raw $normalized ('sha256:'+$configId) $expectedTag}catch{$caught=$_.Exception.Message}
    if ($case -in @('hybrid','gzip-layer')) {
        if ($caught) { throw "Producer normalization failed: $case ($caught)" }
        Assert-DockerSaveArchive $normalized ('sha256:'+$configId) $expectedTag -Uncompressed
        $file=[IO.File]::OpenRead($normalized);$reader=[Formats.Tar.TarReader]::new($file);$names=@()
        try{while($null-ne($entry=$reader.GetNextEntry())){if([string]$entry.EntryType-cne'GlobalExtendedAttributes'){$names+=,$entry.Name}}}finally{$reader.Dispose();$file.Dispose()}
        if ($names.Count-ne3 -or @($names|Where-Object{$_-cnotin@('manifest.json',($configId+'.json'),'layers/0000.tar')}).Count) { throw 'Normalization exported an unreferenced/private image entry.' }
    } elseif (!$caught) { throw "Unsafe producer archive was normalized: $case" }
    $passed++;Write-Host "PASS closed producer normalization: $case"
}
# Invoke the real packaging entry point for proof/checksum failures, with a sentinel
# Docker command that makes any accidental daemon call a test failure.
$sentinel = Join-Path $root 'docker-invoked'; $dockerStub = Join-Path $root 'docker-must-not-run.ps1'
$escapedSentinel = $sentinel.Replace("'", "''")
Set-Content -LiteralPath $dockerStub -Value "[IO.File]::WriteAllText('$escapedSentinel','unexpected'); throw 'Docker must not run in offline boundary tests.'" -Encoding utf8NoBOM
$archive = Join-Path $root 'AudioLibServer-v0.8.4-linux-x64.tar.gz'
Copy-Item -LiteralPath (Join-Path $root 'native-valid/native.tar.gz') -Destination $archive
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
$commit = 'a' * 40
foreach ($case in @('checksum','unaccepted','target','commit','runtime-proof')) {
    $proof = [ordered]@{ success=$true;tag=$Tag;version='0.8.4';commit=$commit;target='x86_64-unknown-linux-gnu';archive_sha256=$hash;binary_sha256=('b'*64); checks=@{version=$true;allowlist=$true;unlinked_paths=$true;privacy=$true;archive_contents=$true;executable_mode=$true} }
    $runtime = [ordered]@{ success=$true;tag=$Tag;version='0.8.4';commit=$commit;target='x86_64-unknown-linux-gnu';archive_sha256=$hash;binary_sha256=('b'*64);checks=@('synthetic native validation') }
    Set-Content -LiteralPath ($archive + '.sha256') -Value ($hash + '  ' + [IO.Path]::GetFileName($archive)) -Encoding utf8NoBOM
    switch ($case) {
        'checksum' { Set-Content -LiteralPath ($archive + '.sha256') -Value (('0'*64) + '  ' + [IO.Path]::GetFileName($archive)) -Encoding utf8NoBOM }
        'unaccepted' { $proof.success=$false }
        'target' { $proof.target='aarch64-unknown-linux-gnu' }
        'commit' { $proof.commit='c'*40 }
        'runtime-proof' { $runtime.success=$false }
    }
    $reportPath=Join-Path $root 'native-report.json'; $runtimePath=Join-Path $root 'native-runtime.json'
    $proof | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding utf8NoBOM
    $runtime | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $runtimePath -Encoding utf8NoBOM
    $caught=$null
    try { & (Join-Path $PSScriptRoot 'package-server-docker.ps1') -Tag $Tag -ExpectedCommit $commit -Platform linux/amd64 -ServerArchive $archive -ServerReport $reportPath -ServerValidation $runtimePath -OutputDirectory (Join-Path $root 'output') -DockerCommand $dockerStub } catch { $caught=$_.Exception.Message }
    if (!$caught -or (Test-Path -LiteralPath $sentinel) -or $caught -notmatch 'checksum mismatch|acceptance proof identity mismatch') { throw "Pre-Docker proof rejection failed: $case ($caught)" }
    $passed++; Write-Host "PASS pre-Docker proof: $case"
}
$archive = Join-Path $root 'AudioLibServer-v0.8.4-docker-linux-amd64.tar.gz'
Copy-Item -LiteralPath (Join-Path $root 'validate-server-docker.ps1-valid.tar.gz') -Destination $archive
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -LiteralPath ($archive + '.sha256') -Value ($hash + '  ' + [IO.Path]::GetFileName($archive)) -Encoding utf8NoBOM
$dockerProof = @{success=$true;tag=$Tag;version='0.8.4';commit=$commit;platform='linux/amd64';image_tag=$expectedTag;image_id=$imageId;archive_sha256=$hash;runtime_validation='not-run'}
$dockerReport=Join-Path $root 'docker-package-report.json'
$dockerProof | ConvertTo-Json | Set-Content -LiteralPath $dockerReport -Encoding utf8NoBOM
foreach ($case in @('extra-data','source-hash','recorded-path')) {
    $fixtureDirectory=Join-Path $root ('fixture-' + $case)
    $null=New-Item -ItemType Directory -Path (Join-Path $fixtureDirectory 'data'),(Join-Path $fixtureDirectory 'music') -Force
    Set-Content -LiteralPath (Join-Path $fixtureDirectory 'data/library.db') -Value 'synthetic offline input only' -Encoding utf8NoBOM
    $wav=Join-Path $fixtureDirectory 'music/synthetic.wav'
    $bytes=[byte[]]::new(4096); [Text.Encoding]::ASCII.GetBytes('RIFF').CopyTo($bytes,0); [IO.File]::WriteAllBytes($wav,$bytes)
    $fixture=@{schema_version=1;audio_id=1;recorded_path='/music/synthetic.wav';source_sha256=(Get-FileHash -LiteralPath $wav -Algorithm SHA256).Hash.ToLowerInvariant()}
    if ($case -ceq 'extra-data') { Set-Content -LiteralPath (Join-Path $fixtureDirectory 'settings.json') -Value '{}' }
    if ($case -ceq 'source-hash') { $fixture.source_sha256='0'*64 }
    if ($case -ceq 'recorded-path') { $fixture.recorded_path='/private/working-library.wav' }
    $fixture | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $fixtureDirectory 'fixture.json') -Encoding utf8NoBOM
    $caught=$null
    try { & (Join-Path $PSScriptRoot 'validate-server-docker.ps1') -Tag $Tag -ExpectedCommit $commit -Platform linux/amd64 -DockerArchive $archive -PackageReport $dockerReport -FixtureDirectory $fixtureDirectory -DockerCommand $dockerStub *> $null } catch { $caught=$_.Exception.Message }
    if (!$caught -or (Test-Path -LiteralPath $sentinel) -or $caught -notmatch 'Fixture must contain only|fixture size/hash mismatch|fixture manifest') { throw "Pre-Docker fixture rejection failed: $case ($caught)" }
    $passed++; Write-Host "PASS pre-Docker fixture: $case"
}
Write-Host "$passed Docker/native boundary cases passed; Docker build/load/run not invoked."
