# tools/test-server-package.ps1
# Isolated package-boundary regression. Uses a tiny --version fixture executable,
# synthetic Cargo/Git metadata, and public placeholders; never builds/starts a server.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$testParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = [IO.Path]::GetFullPath((Join-Path $testParent ('AudioLibServerPackage-' + [guid]::NewGuid().ToString('N'))))
$comparison = if ($IsWindows) { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
function Assert-TestPath([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    if (!$full.StartsWith($testRoot + [IO.Path]::DirectorySeparatorChar, $comparison)) { throw 'Server regression escaped its owned subtree.' }
}
function Expect-Rejection([scriptblock]$Action, [string]$Message) {
    $rejected = $false
    try { & $Action } catch { $rejected = $_.Exception.Message -match $Message }
    if (!$rejected) { throw 'Server package failed to reject a boundary regression.' }
}
$taskProcessArchitecture = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
if ($IsWindows) {
    $target = 'x86_64-pc-windows-msvc'; $platformName = 'win-x64'
} elseif ($IsLinux) {
    switch ($taskProcessArchitecture) {
        ([Runtime.InteropServices.Architecture]::X64) {
            $target = 'x86_64-unknown-linux-gnu'; $platformName = 'linux-x64'
        }
        ([Runtime.InteropServices.Architecture]::Arm64) {
            $target = 'aarch64-unknown-linux-gnu'; $platformName = 'linux-arm64'
        }
        default { throw 'Server package regressions require Linux x64 or arm64.' }
    }
} else {
    $target = 'aarch64-apple-darwin'; $platformName = 'macos-arm64'
}
$binaryName = if ($IsWindows) { 'audiolib-server.exe' } else { 'audiolib-server' }
$extension = if ($IsWindows) { 'zip' } else { 'tar.gz' }
New-Item -ItemType Directory -Path (Join-Path $testRoot 'tools') | Out-Null
$savedTargetDirectory = $env:CARGO_TARGET_DIR
try {
    # The validator's intentionally missing benchmark belongs to this synthetic
    # checkout. A developer's populated target directory must never satisfy it.
    $env:CARGO_TARGET_DIR = Join-Path $testRoot 'target'
    foreach ($file in @('package-server.ps1', 'console-utf8.ps1', 'validate-server-package.ps1', 'validate-update-compatibility.ps1', 'update-compatible-files.json', 'server-files.json')) { Copy-Item -LiteralPath (Join-Path $PSScriptRoot $file) -Destination (Join-Path $testRoot 'tools') }
    $files = @(Get-Content -LiteralPath (Join-Path $testRoot 'tools/server-files.json') -Raw | ConvertFrom-Json)
    foreach ($file in $files) {
        $destination = Join-Path $testRoot $file
        Assert-TestPath $destination
        New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
        Set-Content -LiteralPath $destination -Value 'Synthetic public documentation.' -Encoding utf8NoBOM
    }
    $binaryRoot = Join-Path $testRoot "target/$target/release"
    New-Item -ItemType Directory -Path $binaryRoot -Force | Out-Null
    $binary = Join-Path $binaryRoot $binaryName
    if ($IsWindows) {
        $compiler = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
        if (!(Test-Path -LiteralPath $compiler -PathType Leaf)) { throw 'Windows regression requires the installed .NET Framework C# compiler.' }
        $source = Join-Path $testRoot 'VersionFixture.cs'
        Set-Content -LiteralPath $source -Encoding utf8NoBOM -Value 'using System; class VersionFixture { static int Main(string[] args) { if(args.Length != 1 || args[0] != "--version") return 7; Console.WriteLine("audiolib-server 0.4.4"); return 0; } }'
        & $compiler /nologo /target:exe /optimize+ "/out:$binary" $source
        if ($LASTEXITCODE -ne 0) { throw 'Cannot compile the version-only package fixture.' }
    } else {
        [IO.File]::WriteAllText($binary, "#!/bin/sh`n[ `"`$#`" -eq 1 ] && [ `"`$1`" = '--version' ] || exit 7`nprintf 'audiolib-server 0.4.4\n'`n", [Text.UTF8Encoding]::new($false))
        [IO.File]::SetUnixFileMode($binary, [IO.UnixFileMode]493)
    }
    $licenseRoot = Join-Path $testRoot 'dependency'
    New-Item -ItemType Directory -Path $licenseRoot | Out-Null
    Set-Content -LiteralPath (Join-Path $licenseRoot 'LICENSE-MIT') -Value 'Synthetic MIT license notice.' -Encoding utf8NoBOM
    $fakeMetadata = [ordered]@{
        packages=@(
            @{ id='server'; name='audiolib-server'; version='0.4.4'; source=$null; license='GPL-3.0-only'; license_file=$null; repository=$null; manifest_path=(Join-Path $testRoot 'server/audiolib-server/Cargo.toml') },
            @{ id='normal'; name='normal-dependency'; version='1.0.0'; source='registry+public'; license='MIT'; license_file='LICENSE-MIT'; repository='https://example.com/normal'; manifest_path=(Join-Path $licenseRoot 'Cargo.toml') },
            @{ id='dev'; name='private-dev-dependency'; version='1.0.0'; source='registry+public'; license='MIT'; license_file=$null; repository=$null; manifest_path=(Join-Path $testRoot 'must-not-be-opened/Cargo.toml') }
        )
        resolve=@{ nodes=@(
            @{ id='server'; deps=@(@{pkg='normal'; dep_kinds=@(@{kind=$null})}, @{pkg='dev'; dep_kinds=@(@{kind='dev'})}) },
            @{ id='normal'; deps=@() }, @{ id='dev'; deps=@() }
        ) }
    }
    $fakeAsioMetadata = $fakeMetadata | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    $asioSource = Join-Path $testRoot 'asio-dependency'
    $asioBuildSource = Join-Path $testRoot 'asio-build-dependency'
    foreach ($source in @($asioSource, $asioBuildSource)) {
        New-Item -ItemType Directory -Path $source | Out-Null
        Set-Content -LiteralPath (Join-Path $source 'LICENSE-APACHE') -Value 'Synthetic public Apache license notice.' -Encoding utf8NoBOM
    }
    $fakeAsioMetadata.packages += @(
        @{ id='cpal'; name='cpal'; version='0.18.2'; source='registry+public'; license='Apache-2.0'; license_file=$null; repository=$null; manifest_path=(Join-Path $licenseRoot 'Cargo.toml') },
        @{ id='asio'; name='asio-sys'; version='0.4.0'; source='registry+public'; license='Apache-2.0'; license_file=$null; repository=$null; manifest_path=(Join-Path $asioSource 'Cargo.toml') },
        @{ id='asio-build'; name='asio-build-dependency'; version='1.0.0'; source='registry+public'; license='Apache-2.0'; license_file=$null; repository=$null; manifest_path=(Join-Path $asioBuildSource 'Cargo.toml') }
    )
    $fakeAsioMetadata.resolve.nodes[0].deps += @{pkg='cpal'; dep_kinds=@(@{kind=$null})}
    $fakeAsioMetadata.resolve.nodes += @(
        @{ id='cpal'; deps=@(@{pkg='asio'; dep_kinds=@(@{kind=$null})}) },
        @{ id='asio'; deps=@(@{pkg='asio-build'; dep_kinds=@(@{kind='build'})}, @{pkg='dev'; dep_kinds=@(@{kind='dev'})}) },
        @{ id='asio-build'; deps=@() }
    )
    $metadataState = @{ AsioRequests=0 }
    # These functions mask only the external commands in this invocation's child scope.
    $metadataCommand = {
        if ($args -notcontains '--locked' -or $args -notcontains '--filter-platform') { throw 'Package metadata must use the locked target filter.' }
        if ($args -contains '--features') {
            $position = [array]::IndexOf($args, '--features')
            if ($args[$position + 1] -cne 'audiolib-desktop/asio,audiolib-cli/asio') { throw 'ASIO metadata must match the workspace release features.' }
            $metadataState.AsioRequests++
            $fakeAsioMetadata | ConvertTo-Json -Depth 8 -Compress
        } else { $fakeMetadata | ConvertTo-Json -Depth 8 -Compress }
        $global:LASTEXITCODE = 0
    }.GetNewClosure()
    Set-Item -LiteralPath Function:cargo -Value $metadataCommand
    function git { 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; $global:LASTEXITCODE = 0 }
    $latest = Join-Path $testRoot 'dist/latest'
    $payload = Join-Path $latest "server-$platformName"
    New-Item -ItemType Directory -Path $payload -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $payload 'settings.json') -Value 'Stale private runtime data.'
    foreach ($protected in @('dist/latest/portable/preserve.txt', 'local_release/preserve.txt', 'doc/unlisted-private-report.json')) {
        $path = Join-Path $testRoot $protected
        New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force | Out-Null
        Set-Content -LiteralPath $path -Value 'Owned sentinel.'
    }
    $packager = Join-Path $testRoot 'tools/package-server.ps1'
    $validator = Join-Path $testRoot 'tools/validate-server-package.ps1'
    $compatibilityValidator = Join-Path $testRoot 'tools/validate-update-compatibility.ps1'
    foreach ($compatibilityBinary in @('audiolib-server', 'audiolib-server.exe')) {
        & $compatibilityValidator -Product Server -ServerBinaryName $compatibilityBinary -Entries @($compatibilityBinary, 'licenses/future-dependency-9.0/LICENSE-MIT', 'licenses/dependencies.json')
        $otherBinary = if ($compatibilityBinary -ceq 'audiolib-server') { 'audiolib-server.exe' } else { 'audiolib-server' }
        Expect-Rejection { & $compatibilityValidator -Product Server -ServerBinaryName $compatibilityBinary -Entries @($otherBinary) } 'Update compatibility.*Non-public update file'
    }
    Expect-Rejection { & $compatibilityValidator -Product Server -Entries @('licenses/evil.ps1') } 'Update compatibility.*Non-public update file'
    foreach ($linuxTarget in @('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')) {
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $linuxTarget -Asio } '-Asio requires'
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $linuxTarget -Asio } '-Asio requires'
    }
    $otherLinuxTarget = if ($IsLinux -and $taskProcessArchitecture -eq [Runtime.InteropServices.Architecture]::Arm64) { 'x86_64-unknown-linux-gnu' } else { 'aarch64-unknown-linux-gnu' }
    $otherLinuxPlatform = if ($otherLinuxTarget -ceq 'x86_64-unknown-linux-gnu') { 'Linux x64' } else { 'Linux arm64' }
    Expect-Rejection { & $packager -Tag v0.4.4 -Target $otherLinuxTarget } $otherLinuxPlatform
    Expect-Rejection { & $validator -Tag v0.4.4 -Target $otherLinuxTarget } $otherLinuxPlatform
    & $packager -Tag v0.4.4 -Target $target
    if ((Get-Content -LiteralPath (Join-Path $payload 'update-channel.txt') -Raw).Trim() -cne 'stable' -or
        (Get-Content -LiteralPath (Join-Path $payload 'update-version.txt') -Raw).Trim() -cne '0.4.4') { throw 'Stable server update metadata differs.' }
    & $packager -Tag v0.4.4-beta.12 -Target $target
    if ((Get-Content -LiteralPath (Join-Path $payload 'update-channel.txt') -Raw).Trim() -cne 'beta' -or
        (Get-Content -LiteralPath (Join-Path $payload 'update-version.txt') -Raw).Trim() -cne '0.4.4-beta.12') { throw 'Beta server lost its exact release identity.' }
    & $packager -Tag v0.4.4 -Target $target
    if (Test-Path -LiteralPath (Join-Path $payload 'settings.json')) { throw 'Stale server runtime file survived packaging.' }
    foreach ($protected in @('dist/latest/portable/preserve.txt', 'local_release/preserve.txt', 'doc/unlisted-private-report.json')) {
        if ((Get-Content -LiteralPath (Join-Path $testRoot $protected) -Raw).Trim() -ne 'Owned sentinel.') { throw 'Packaging changed an unrelated output.' }
    }
    if (Test-Path -LiteralPath (Join-Path $payload 'doc/unlisted-private-report.json')) { throw 'Unlisted private report was packaged.' }
    $dependencyText = Get-Content -LiteralPath (Join-Path $payload 'licenses/dependencies.json') -Raw
    if ($dependencyText.Contains($testRoot) -or $dependencyText.Contains('manifest_path') -or $dependencyText.Contains('private-dev-dependency')) { throw 'Dependency metadata leaked private fields or dev-only dependencies.' }
    $archive = Join-Path $latest "AudioLibServer-v0.4.4-$platformName.$extension"
    $archiveHash = (Get-FileHash -LiteralPath $archive).Hash
    if ((Get-Content -LiteralPath ($archive + '.sha256') -Raw).Trim() -cne ($archiveHash + '  ' + [IO.Path]::GetFileName($archive))) { throw 'Server archive checksum differs.' }
    $reportText = Get-Content -LiteralPath (Join-Path $latest "AudioLibServer-v0.4.4-$platformName-report.json") -Raw
    $report = $reportText | ConvertFrom-Json
    if (@($report.PSObject.Properties.Name | Sort-Object) -join ',' -cne 'archive_sha256,binary_sha256,checks,commit,success,tag,target,version' -or !$report.success -or
        $report.archive_sha256 -cne $archiveHash -or $report.binary_sha256 -cne (Get-FileHash -LiteralPath $binary).Hash -or
        $reportText.Contains($testRoot) -or @($report.checks.PSObject.Properties.Value | Where-Object { $_ -ne $true }).Count) { throw 'Server public report schema/identity failed.' }
    if (!$IsWindows) {
        $extracted = Join-Path $testRoot 'extracted'
        New-Item -ItemType Directory -Path $extracted | Out-Null
        & tar -xzf $archive -C $extracted
        if ($LASTEXITCODE -ne 0 -or ([int][IO.File]::GetUnixFileMode((Join-Path $extracted $binaryName)) -band 73) -ne 73 -or
            ([int][IO.File]::GetUnixFileMode((Join-Path $extracted 'server/deploy/redeploy-server.sh')) -band 73) -ne 73) { throw 'Unix archive did not preserve executable permissions.' }
    }
    if ($IsWindows) {
        # Test ASIO before CI prepares the real SDK. The pinned ZIP checksum is
        # mocked ONLY for this synthetic owned ZIP; every notice/release hash
        # still uses the real SHA256 cmdlet. Production has no checksum override.
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $target -Asio } 'prepared ASIO SDK archive/license is missing'
        $sdkDirectory = Join-Path $testRoot 'tools/.build-tools/asio-2.3.4/ASIOSDK'
        New-Item -ItemType Directory -Path $sdkDirectory -Force | Out-Null
        $sdkLicense = Join-Path $sdkDirectory 'LICENSE.txt'
        $sdkText = 'Synthetic prepared SDK notice.'
        [IO.File]::WriteAllText($sdkLicense, $sdkText, [Text.UTF8Encoding]::new($false))
        $sdkArchive = Join-Path $testRoot 'tools/.build-tools/ASIO-SDK_2.3.4_2025-10-15.zip'
        $sdkZip = [IO.Compression.ZipFile]::Open($sdkArchive, [IO.Compression.ZipArchiveMode]::Create)
        try {
            $stream = $sdkZip.CreateEntry('ASIOSDK/LICENSE.txt').Open()
            try { $bytes = [Text.Encoding]::UTF8.GetBytes($sdkText); $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
        } finally { $sdkZip.Dispose() }
        $sdkState = @{ Pinned=$true }
        $hashCommand = {
            param([string]$LiteralPath, [string]$Algorithm='SHA256')
            if ([IO.Path]::GetFullPath($LiteralPath).Equals($sdkArchive, [StringComparison]::OrdinalIgnoreCase)) {
                if ($Algorithm -cne 'SHA256') { throw 'SDK pin must be SHA256.' }
                return [pscustomobject]@{ Hash=$(if ($sdkState.Pinned) { 'D5EBF0C20DD2C5F43771FD0C1418F4B361BF52434EE670097CFA6B3A335E2ECA' } else { '0' * 64 }) }
            }
            Microsoft.PowerShell.Utility\Get-FileHash -LiteralPath $LiteralPath -Algorithm $Algorithm
        }.GetNewClosure()
        Set-Item -LiteralPath Function:Get-FileHash -Value $hashCommand
        & $packager -Tag v0.4.4 -Target $target -Asio
        $asioManifest = @(Get-Content -LiteralPath (Join-Path $payload 'licenses/dependencies.json') -Raw | ConvertFrom-Json)
        if ($metadataState.AsioRequests -ne 1 -or !($asioManifest | Where-Object name -CEQ 'asio-sys') -or
            !($asioManifest | Where-Object name -CEQ 'asio-build-dependency') -or ($asioManifest | Where-Object name -CEQ 'private-dev-dependency')) { throw 'ASIO normal/build closure failed to match release features.' }
        $sdkRelative = 'licenses/Steinberg-ASIO-SDK-2.3.4.txt'
        $packagedSdk = Join-Path $payload $sdkRelative
        if ((Get-FileHash -LiteralPath $packagedSdk).Hash -cne (Get-FileHash -LiteralPath $sdkLicense).Hash -or
            !(Test-Path -LiteralPath (Join-Path $payload 'licenses/asio-sys-0.4.0/LICENSE-APACHE') -PathType Leaf)) { throw 'ASIO SDK/crate notices were not copied from their verified sources.' }
        $asioArchiveHash = (Get-FileHash -LiteralPath $archive).Hash
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try { if ($null -eq $zip.GetEntry($sdkRelative) -or $null -eq $zip.GetEntry('licenses/asio-sys-0.4.0/LICENSE-APACHE')) { throw 'ASIO notices are missing from the release archive.' } }
        finally { $zip.Dispose() }
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target } 'ASIO notices differ'
        # Synthetic bytes cannot pass the official SDK notice hash: rejection
        # must happen before invoking a benchmark or starting the server fixture.
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'SDK notice differs from the pinned SDK'
        $sdkState.Pinned = $false
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $target -Asio } 'archive checksum mismatch'
        $sdkState.Pinned = $true
        [IO.File]::WriteAllText($sdkLicense, 'Modified prepared SDK notice.', [Text.UTF8Encoding]::new($false))
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $target -Asio } 'license differs from the pinned archive'
        [IO.File]::WriteAllText($sdkLicense, $sdkText, [Text.UTF8Encoding]::new($false))
        Remove-Item -LiteralPath $sdkLicense
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $target -Asio } 'prepared ASIO SDK archive/license is missing'
        if ((Get-FileHash -LiteralPath $archive).Hash -cne $asioArchiveHash) { throw 'An ASIO rejection replaced the verified archive.' }
        # Public audited license bytes are gzip-encoded to preserve exact upstream
        # whitespace on every checkout. These are fixture data, not product pins
        # or downloaded content; their actual SHA256 is verified before use.
        function Expand-PublicNotice([string]$Encoded, [string]$ExpectedHash) {
            $input = [IO.MemoryStream]::new([Convert]::FromBase64String($Encoded))
            $output = [IO.MemoryStream]::new()
            $gzip = [IO.Compression.GZipStream]::new($input, [IO.Compression.CompressionMode]::Decompress)
            try {
                $gzip.CopyTo($output)
                $bytes = $output.ToArray()
                if ([Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)) -cne $ExpectedHash) { throw 'Public license fixture bytes changed.' }
                return ,$bytes
            } finally { $gzip.Dispose(); $input.Dispose(); $output.Dispose() }
        }
        $publicCpalNotice = Expand-PublicNotice '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' 'C71D239DF91726FC519C6EB72D318EC65820627232B2F796219E87DCF35D0AB4'
        $publicSdkNotice = Expand-PublicNotice 'H4sIAAAAAAACCq1WwZLaOBC9+yu6chqqGEgltZfsyWANqGJs1jYzmaOxG0ZbxqJkeVj+fp8EDJNNJpvDcMG2Wt2vX79Wazy+fc9fMB5TLKciyYV7vKkG9Onjpz+GlFtW7ZrNlhZcq5IKrp5a3eit4o5mu/V8SGHTUKa2T7ajjDs2z1zDx/vCK55UR42quO2Yyv2+ceF12xzJatqoBm+GN2y4rVS7JfvKfBhstCFtn9icLfUGBky53thDaZgifuZG73fcWvqqrF803O25suqZiXdrrmuuX+Jb/scGCOBxVOW64VFwQvimyyucmvq2BhQXxLLZXeG8MB3mMqX4gh7gywaWbenQIOM39s+4ZVM2tOzXCHXZTzezZTygezad0i19HgWPuqddCTcuk1/RUFaVNrWnUxMrT+ApFnaeswEJHfn8gp3urKPE6L1RpWVytB91bwif/gaXqBeVVJUd366Pt+6f1mUHZm4qvduxqRTQY0+r7QCElgNael9sS3N8gx7Q/osMXJrwRmumWnXWqHVvUQDV0r40kCuCKfzBXBm2x+CAJHVvAVhh6WCUtQzMW8Psfa6P/98PoxOk6CvtehByDt+7woNGw7fcblXLjksDgK3a941jq2yh5YszX506AFD3+RoTW6AzU986/MeLAK2rLJa6Cw87XfcNpNO3kHtHe8jEpVK7BJryMAqSczld/dtyxz9q8I3skM91l5ceCAwq3Z7I1abzpL/KmNsaX9nhgw52Gqniv+4rEA8dQ9I1bbDgezZ4SeFSCteFagM9f18Tn1LnJD0KJgylwavTfffk9Fpeqbg2y/6VmC5Hg9MnoRgeqV7bUnmJ6v0xODNyaaPwRQSd2rYnKn9XC98r10njR69V2TrWNhqAqbRfgsPhMOouO0ct2zG3YzTKHooY1yeho6lHT3bX4PSZC+9Y5rTM0nsZiYgmj5QXQiYTkc1oISIZUiGm8ySN05kUOc0Wkzl9CHNs+kBhEgVh8kji2zITeU5pRnKxjCX8PIRZFiYFtgxJJtN4FclkNqTJqqAkLTA2FrKAWZEOycE4bwuu2yi9Q/xsOsdrOJGxLB5dPLqTReJi3SFYSMswK+R0FYcZLVfZMs0FhZmgSObTOJTAPwpkgogk7kVSUD4P4/g3EpwIIAwnsTiFQYqRzMS0GMLb+cllBcYALsbAW4qpdA/im0AmYfY4dGRMU4zGv1YwwiJF4SKcIcLNlY/gZ3ygFtNVJhYOMEjIV5O8kMWqEDRL08iznIvsHoM3/5PiNPdUrXIxDKKwCH1guABPWMbzZJVLz5hMCpFlq2Uh02RA8/QBnABjiK2RpzZNfKqoR5o9wmngOPDMD+lhLvAd9U1cVkUWOgryIpPTgl6ZIV6RZsWrHCkRs1jORDIVAVZT5+VB5mKAQsncGchT2IcQMVc+ZacIoDo9Qp15eldAGWLoy0jyjsLoHj6i4GyMwufyLBJP2XR+phszYT34zTEXRGh01XQ/GVj/7TRynbZt+5E22/HFarzdN7efRx/P7TVhHOVLU+JCULm7gR9Ybe1P3i/BsmE3zPwFxB0kP5nmfVdumba9qrnB6d/54ejs3BQGRmc0PJni/NA49uvTW+Bb3ipcMvxKN3rvu9XpgvUvd73K6lAKAAA=' '61E56814F24B32216884B2DB0B4DCC317111A5AB354D54D19DD9FC13D71B1C7A'
        [IO.File]::WriteAllBytes($sdkLicense, $publicSdkNotice)
        Assert-TestPath $sdkArchive
        Remove-Item -LiteralPath $sdkArchive
        $sdkZip = [IO.Compression.ZipFile]::Open($sdkArchive, [IO.Compression.ZipArchiveMode]::Create)
        try {
            $stream = $sdkZip.CreateEntry('ASIOSDK/LICENSE.txt').Open()
            try { $stream.Write($publicSdkNotice, 0, $publicSdkNotice.Length) } finally { $stream.Dispose() }
        } finally { $sdkZip.Dispose() }
        # Independent asio-sys notices retain precedence over the audited fallback.
        # The version-only executable passes validation up to the absent benchmark,
        # proving no synthetic server/data process was started by this regression.
        & $packager -Tag v0.4.4 -Target $target -Asio
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'Build the Windows release search-benchmark'
        $asioLicense = Join-Path $asioSource 'LICENSE-APACHE'
        Assert-TestPath $asioLicense
        Remove-Item -LiteralPath $asioLicense
        $cpalLicense = Join-Path $licenseRoot 'LICENSE'
        [IO.File]::WriteAllBytes($cpalLicense, $publicCpalNotice)
        $asioIdentity = $fakeAsioMetadata.packages | Where-Object { $_.name -ceq 'asio-sys' }
        $cpalIdentity = $fakeAsioMetadata.packages | Where-Object { $_.name -ceq 'cpal' }
        $asioIdentity.repository = 'https://github.com/RustAudio/cpal'
        $cpalIdentity.repository = 'https://github.com/RustAudio/cpal'
        & $packager -Tag v0.4.4 -Target $target -Asio
        if (Test-Path -LiteralPath (Join-Path $payload 'licenses/asio-sys-0.4.0')) { throw 'Shared-notice fixture unexpectedly contains an independent asio-sys notice.' }
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'Build the Windows release search-benchmark'
        foreach ($identity in @($asioIdentity, $cpalIdentity)) {
            foreach ($field in @('repository', 'license', 'version')) {
                $savedIdentity = $identity.$field
                $identity.$field = if ($field -eq 'repository') { 'https://example.com/unrelated' } elseif ($field -eq 'license') { 'MIT' } else { '9.9.9' }
                try {
                    & $packager -Tag v0.4.4 -Target $target -Asio
                    Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'shared CPAL license identity is invalid or missing'
                } finally { $identity.$field = $savedIdentity }
            }
        }
        [IO.File]::WriteAllText($cpalLicense, 'Altered shared upstream license.', [Text.UTF8Encoding]::new($false))
        & $packager -Tag v0.4.4 -Target $target -Asio
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'shared CPAL license differs from the audited upstream notice'
        Remove-Item -LiteralPath $cpalLicense
        & $packager -Tag v0.4.4 -Target $target -Asio
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'shared CPAL license identity is invalid or missing'
        Write-Host 'ASIO shared CPAL notice regression passed: independent precedence, audited fallback, both crate identities, absent notice and actual notice hash.'
        & $packager -Tag v0.4.4 -Target $target
        $archiveHash = (Get-FileHash -LiteralPath $archive).Hash
        if (Test-Path -LiteralPath (Join-Path $payload $sdkRelative)) { throw 'A non-ASIO package retained the previous SDK notice.' }
        Expect-Rejection { & $validator -Tag v0.4.4 -Target $target -Asio } 'public file allowlist'
    }
    Expect-Rejection { & $packager -Tag v0.4.5 -Target $target } 'version does not match'
    $document = Join-Path $testRoot $files[0]
    Set-Content -LiteralPath $document -Value $testRoot
    Expect-Rejection { & $packager -Tag v0.4.4 -Target $target -BinaryPath $binary } 'Local build/user path'
    Set-Content -LiteralPath $document -Value 'Synthetic public documentation.'
    $manifest = Join-Path $testRoot 'tools/server-files.json'
    $futureDocument = 'doc/Rust_Future_Update.md'
    Set-Content -LiteralPath (Join-Path $testRoot $futureDocument) -Value 'Synthetic public future documentation.' -Encoding utf8NoBOM
    try {
        $files + @($futureDocument) | ConvertTo-Json | Set-Content -LiteralPath $manifest -Encoding utf8NoBOM
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $target } 'Update compatibility.*Non-public update file'
    } finally { ConvertTo-Json -InputObject $files | Set-Content -LiteralPath $manifest -Encoding utf8NoBOM }
    if ((Get-FileHash -LiteralPath $archive).Hash -cne $archiveHash -or (Test-Path -LiteralPath (Join-Path $payload $futureDocument))) {
        throw 'An incompatible server package replaced the verified output.'
    }
    $files + @('../outside.txt') | ConvertTo-Json | Set-Content -LiteralPath $manifest -Encoding utf8NoBOM
    Expect-Rejection { & $packager -Tag v0.4.4 -Target $target } 'Unsafe relative'
    ConvertTo-Json -InputObject $files | Set-Content -LiteralPath $manifest -Encoding utf8NoBOM
    if ((Get-FileHash -LiteralPath $archive).Hash -cne $archiveHash) { throw 'A rejected package replaced the last verified archive.' }
    $dist = Join-Path $testRoot 'dist'
    $redirected = Join-Path $testRoot 'redirected'
    Assert-TestPath $dist; Assert-TestPath $redirected
    Move-Item -LiteralPath $dist -Destination (Join-Path $testRoot 'prior-output')
    New-Item -ItemType Directory -Path $redirected | Out-Null
    Set-Content -LiteralPath (Join-Path $redirected 'preserve.txt') -Value 'Owned sentinel.'
    try {
        if ($IsWindows) { New-Item -ItemType Junction -Path $dist -Target $redirected | Out-Null } else { $null = [IO.Directory]::CreateSymbolicLink($dist, $redirected) }
        Expect-Rejection { & $packager -Tag v0.4.4 -Target $target } 'Linked server package path'
        if (Test-Path -LiteralPath (Join-Path $redirected 'latest')) { throw 'Packaging followed a linked output ancestor.' }
    } finally {
        $link = Get-Item -LiteralPath $dist -Force -ErrorAction SilentlyContinue
        if ($null -ne $link -and ($link.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Assert-TestPath $dist; Remove-Item -LiteralPath $dist -Force }
    }
    Write-Host 'Server package boundary regression passed: allowlist, old-updater compatibility and licenses, dependency closure, safe report/checksums, incompatible public doc rejection/preservation, linked-path guard and ASIO target/SDK boundary (positive ASIO fixture on Windows).'
} finally {
    $env:CARGO_TARGET_DIR = $savedTargetDirectory
    if (Test-Path -LiteralPath $testRoot) {
        if (!(Split-Path -Leaf $testRoot).StartsWith('AudioLibServerPackage-', [StringComparison]::Ordinal) -or
            !$testRoot.StartsWith($testParent.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar, $comparison) -or
            @(Get-ChildItem -LiteralPath $testRoot -Recurse -Force -Attributes ReparsePoint).Count) { throw 'Unsafe server regression cleanup path.' }
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
