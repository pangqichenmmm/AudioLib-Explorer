# tools/test-package-boundary.ps1
# Regression for stale-output and ignored-file leaks. Runs the real packager in
# an owned synthetic checkout with fake build outputs and dependency metadata.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$testParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$testRoot = [IO.Path]::GetFullPath((Join-Path $testParent ('AudioLibPackageBoundary-' + [guid]::NewGuid().ToString('N'))))
if (!$testRoot.StartsWith($testParent + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Package regression escaped owned test parent.' }
$savedTargetDirectory = $env:CARGO_TARGET_DIR
try {
    $env:CARGO_TARGET_DIR = Join-Path $testRoot 'target'
    New-Item -ItemType Directory -Path (Join-Path $testRoot 'tools') | Out-Null
    Set-Content -LiteralPath (Join-Path $testRoot 'Cargo.toml') -Value "[workspace.package]`nversion = `"0.4.4`"" -Encoding ASCII
    foreach ($script in @('package-portable.ps1','console-utf8.ps1','validate-portable.ps1','validate-portable-licenses.ps1','validate-portable-zip.ps1','validate-package-privacy.ps1','validate-update-compatibility.ps1','update-compatible-files.json','portable-files.json','Start-AudioLibExplorer.cmd','install-beta.ps1','AudioLibExplorer-beta.cmd')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $script) -Destination (Join-Path $testRoot 'tools')
    }
    $files = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'portable-files.json') -Raw | ConvertFrom-Json
    foreach ($file in $files) {
        $destination = Join-Path $testRoot $file
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
        Set-Content -LiteralPath $destination -Value 'synthetic public document' -Encoding UTF8
    }
    $buildRoot = Join-Path $testRoot 'target/x86_64-pc-windows-msvc/release'
    New-Item -ItemType Directory -Force -Path $buildRoot | Out-Null
    foreach ($binary in @('AudioLibExplorer.exe','AudioLibCli.exe')) { Set-Content -LiteralPath (Join-Path $buildRoot $binary) -Value 'synthetic build' -Encoding ASCII }
    $staleRoot = Join-Path $testRoot 'dist/latest/portable'
    New-Item -ItemType Directory -Force -Path (Join-Path $staleRoot 'doc/assets/issues') | Out-Null
    Set-Content -LiteralPath (Join-Path $staleRoot 'settings.json') -Value 'stale private state'
    Set-Content -LiteralPath (Join-Path $staleRoot 'doc/Rust_Old_Report.json') -Value 'stale private report'
    Set-Content -LiteralPath (Join-Path $staleRoot 'doc/assets/issues/private.png') -Value 'private image'
    Set-Content -LiteralPath (Join-Path $testRoot 'doc/Rust_CI_Release.json') -Value 'ignored private report'
    New-Item -ItemType Directory -Force -Path (Join-Path $testRoot 'resources/private') | Out-Null
    Set-Content -LiteralPath (Join-Path $testRoot 'resources/private/settings.json') -Value 'untracked private state'
    # SMB can mark dot-prefixed staging directories hidden. Existing hidden
    # ancestors must still be inspected rather than silently skipped or failing.
    $hiddenAncestor = Get-Item -LiteralPath (Split-Path -Parent $staleRoot) -Force
    $hiddenAncestor.Attributes = $hiddenAncestor.Attributes -bor [IO.FileAttributes]::Hidden
    # Minimal license fixture; Cargo and application binaries remain synthetic.
    $fixturePackages = @(
        @{name='slint';version='1.17.1';manifest_path=(Join-Path $testRoot 'registry/slint/Cargo.toml')},
        @{name='soundtouch';version='0.5.4';manifest_path=(Join-Path $testRoot 'registry/soundtouch/Cargo.toml')},
        @{name='soundtouch-ffi';version='0.4.0';manifest_path=(Join-Path $testRoot 'registry/soundtouch-ffi/Cargo.toml')}
    )
    foreach ($package in $fixturePackages) {
        New-Item -ItemType Directory -Path (Split-Path -Parent $package.manifest_path) -Force | Out-Null
    }
    $fixtureSlint = Join-Path $testRoot 'registry/slint/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md'
    $fixtureSoundTouch = Join-Path $testRoot 'registry/soundtouch-ffi/soundtouch-2_3_2/COPYING.TXT'
    $fixtureAsio = Join-Path $testRoot 'tools/.build-tools/asio-2.3.4/ASIOSDK/LICENSE.txt'
    foreach ($license in @($fixtureSlint, $fixtureSoundTouch, $fixtureAsio)) {
        New-Item -ItemType Directory -Path (Split-Path -Parent $license) -Force | Out-Null
        Set-Content -LiteralPath $license -Value 'Synthetic public license.' -Encoding ASCII
    }
    function cargo { @{packages=$fixturePackages} | ConvertTo-Json -Depth 5; $global:LASTEXITCODE = 0 }
    $compatibilityValidator = Join-Path $testRoot 'tools/validate-update-compatibility.ps1'
    & $compatibilityValidator -Product Desktop -Entries @('AudioLibExplorer.exe', 'licenses/future-dependency-9.0/LICENSE-MIT', 'licenses/dependencies.json')
    foreach ($invalid in @('doc/Rust_Future_Update.md', 'doc/../LICENSE', 'licenses/evil.exe', 'licenses/library.sqlite3-shm', 'licenses/private.settings.json', 'licenses/tokens.json', 'licenses/.env', 'licenses/cache.server-state.json', ('licenses/COM' + [char]0x00b9 + '.txt'), 'licenses/notice?.txt')) {
        $rejected = $false
        try { & $compatibilityValidator -Product Desktop -Entries @('LICENSE', $invalid) } catch { $rejected = $_.Exception.Message -match 'update compatibility|Update compatibility' }
        if (!$rejected) { throw "Old-updater file policy accepted an unsupported entry: $invalid" }
    }
    $rejected = $false
    try { & $compatibilityValidator -Product Desktop -Entries @('LICENSE', 'license') } catch { $rejected = $_.Exception.Message -match 'Duplicate update compatibility entry' }
    if (!$rejected) { throw 'Old-updater file policy accepted case-alias duplicate names.' }
    & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio
    $licenseValidator = Join-Path $testRoot 'tools/validate-portable-licenses.ps1'
    & $licenseValidator -BinaryDirectory $staleRoot -WithoutAsio
    $rejected = $false
    try { & $licenseValidator -BinaryDirectory $staleRoot } catch { $rejected = $_.Exception.Message -match 'ASIO SDK license missing' }
    if (!$rejected) { throw 'Default ASIO acceptance allowed a missing SDK license.' }
    # Incomplete packages must fail before looking for a legacy DB or launching
    # the fake executables. No application process can satisfy this check.
    $rejected = $false
    try { & (Join-Path $testRoot 'tools/validate-portable.ps1') -BinaryDirectory $staleRoot } catch { $rejected = $_.Exception.Message -match 'ASIO SDK license missing' }
    if (!$rejected) { throw 'Portable license preflight did not run before runtime work.' }
    function Start-Process { throw 'Synthetic runtime boundary reached.' }
    try {
        foreach ($explicitSynthetic in @($false, $true)) {
            $reachedRuntime = $false
            try { & (Join-Path $testRoot 'tools/validate-portable.ps1') -BinaryDirectory $staleRoot -WithoutAsio -CleanCheckout:$explicitSynthetic } catch { $reachedRuntime = $_.Exception.Message -eq 'Synthetic runtime boundary reached.' }
            if (!$reachedRuntime) { throw 'Synthetic portable acceptance unexpectedly required the legacy fixture.' }
        }
        $rejected = $false
        try { & (Join-Path $testRoot 'tools/validate-portable.ps1') -BinaryDirectory $staleRoot -CleanCheckout -LegacyImport } catch { $rejected = $_.Exception.Message -match 'not both' }
        if (!$rejected) { throw 'Conflicting fixture modes were accepted.' }
    } finally { Remove-Item Function:\Start-Process }
    # A fresh ASIO package includes the SDK license; stale non-ASIO output is replaced.
    & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild
    & $licenseValidator -BinaryDirectory $staleRoot
    foreach ($missing in @('slint-1.17.1/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md', 'SoundTouch-2.3.2-LGPL.txt')) {
        $licensePath = Join-Path (Join-Path $staleRoot 'licenses') $missing
        $licenseText = [IO.File]::ReadAllText($licensePath)
        Remove-Item -LiteralPath $licensePath
        try {
            foreach ($withoutAsio in @($true, $false)) {
                $rejected = $false
                try { & $licenseValidator -BinaryDirectory $staleRoot -WithoutAsio:$withoutAsio } catch { $rejected = $_.Exception.Message -match 'license.*missing' }
                if (!$rejected) { throw "Missing mandatory license accepted: $missing" }
            }
        } finally { [IO.File]::WriteAllText($licensePath, $licenseText) }
    }
    & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio
    # Reject missing required materials before replacing the last good archive.
    $acceptedArchive = Join-Path $testRoot 'dist/latest/AudioLibExplorer-latest-win-x64-portable.zip'
    $acceptedHash = (Get-FileHash -LiteralPath $acceptedArchive).Hash
    $fixtureLicenseText = [IO.File]::ReadAllText($fixtureSlint)
    Remove-Item -LiteralPath $fixtureSlint
    try {
        $rejected = $false
        try { & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio } catch { $rejected = $_.Exception.Message -match 'Selected Slint license text missing' }
        if (!$rejected -or (Get-FileHash -LiteralPath $acceptedArchive).Hash -cne $acceptedHash) { throw 'License failure replaced a validated package.' }
    } finally { [IO.File]::WriteAllText($fixtureSlint, $fixtureLicenseText) }
    # Regression: entries must be forward-slash file names (no `\`, no directory entries).
    Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
    $portableZip = [IO.Compression.ZipFile]::OpenRead((Join-Path $testRoot 'dist/latest/AudioLibExplorer-latest-win-x64-portable.zip'))
    try { $zipNames = @($portableZip.Entries | ForEach-Object { $_.FullName }) } finally { $portableZip.Dispose() }
    if (@($zipNames | Where-Object { $_.Contains('\') -or $_.EndsWith('/') }).Count -or !($zipNames -ccontains 'doc/INDEX.md')) { throw 'Portable ZIP entry names are not updater-safe forward-slash paths.' }
    $badZip = Join-Path $testRoot 'backslash.zip'
    $badArchive = [IO.Compression.ZipFile]::Open($badZip, [IO.Compression.ZipArchiveMode]::Create)
    try { $null = $badArchive.CreateEntry('config\rust_settings.example.json') } finally { $badArchive.Dispose() }
    $rejected = $false
    try { & (Join-Path $testRoot 'tools/validate-portable-zip.ps1') -Archive $badZip } catch { $rejected = $_.Exception.Message -match 'separators' }
    if (!$rejected) { throw 'Backslash ZIP entry names were accepted.' }
    & (Join-Path $testRoot 'tools/install-beta.ps1') -Source $staleRoot -Destination (Join-Path $testRoot 'development-beta') -Development
    if ((Get-Content -LiteralPath (Join-Path $testRoot 'development-beta/update-version.txt') -Raw).Trim() -cne '0.4.4-0' -or
        (Get-Content -LiteralPath (Join-Path $testRoot 'development-beta/update-channel.txt') -Raw).Trim() -cne 'beta') { throw 'Development beta baseline does not sort before a same-version prerelease.' }
    if ((Get-Content -LiteralPath (Join-Path $staleRoot 'update-channel.txt') -Raw).Trim() -cne 'stable' -or
        (Get-Content -LiteralPath (Join-Path $staleRoot 'update-version.txt') -Raw).Trim() -cne '0.4.4') { throw 'Stable package update metadata differs.' }
    & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio -Tag v0.4.4-beta.12
    if ((Get-Content -LiteralPath (Join-Path $staleRoot 'update-channel.txt') -Raw).Trim() -cne 'beta' -or
        (Get-Content -LiteralPath (Join-Path $staleRoot 'update-version.txt') -Raw).Trim() -cne '0.4.4-beta.12') { throw 'Beta package lost its full release identity.' }
    $betaInstall = Join-Path $testRoot 'beta-install'
    New-Item -ItemType Directory -Path $betaInstall | Out-Null
    Set-Content -LiteralPath (Join-Path $betaInstall 'settings.json') -Value 'owned runtime sentinel' -Encoding ASCII
    & (Join-Path $testRoot 'tools/install-beta.ps1') -Source $staleRoot -Destination $betaInstall
    if ((Get-Content -LiteralPath (Join-Path $betaInstall 'update-version.txt') -Raw).Trim() -cne '0.4.4-beta.12' -or
        (Get-Content -LiteralPath (Join-Path $betaInstall 'update-channel.txt') -Raw).Trim() -cne 'beta' -or
        (Get-Content -LiteralPath (Join-Path $betaInstall 'settings.json') -Raw).Trim() -cne 'owned runtime sentinel') { throw 'Beta installation lost release identity or changed owned runtime data.' }
    foreach ($invalidTag in @('v0.4.5-beta.1', 'v0.4.4-beta.01', 'v0.4.4-dev.1')) {
        $rejected = $false
        try { & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio -Tag $invalidTag } catch { $rejected = $_.Exception.Message -match 'Portable tag must match' }
        if (!$rejected) { throw 'Invalid/mismatched portable release identity was accepted.' }
    }
    Set-Content -LiteralPath (Join-Path $staleRoot 'update-channel.txt') -Value 'stable' -Encoding ASCII
    $rejected = $false
    try { & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot } catch { $rejected = $_.Exception.Message -match 'update metadata is invalid' }
    if (!$rejected) { throw 'Stable channel accepted a prerelease update identity.' }
    & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio
    # Current-public does not mean old-updater-compatible. Adding an otherwise
    # permitted document must fail before replacing the verified payload/archive.
    $archive = Join-Path $testRoot 'dist/latest/AudioLibExplorer-latest-win-x64-portable.zip'
    $archiveHash = (Get-FileHash -LiteralPath $archive).Hash
    $futureDocument = 'doc/Rust_Future_Update.md'
    Set-Content -LiteralPath (Join-Path $testRoot $futureDocument) -Value 'Synthetic public future documentation.' -Encoding UTF8
    $manifest = Join-Path $testRoot 'tools/portable-files.json'
    $originalManifest = [IO.File]::ReadAllText($manifest)
    try {
        $files + @($futureDocument) | ConvertTo-Json | Set-Content -LiteralPath $manifest -Encoding UTF8
        $rejected = $false
        try { & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio } catch { $rejected = $_.Exception.Message -match 'Update compatibility.*Non-public update file' }
        if (!$rejected) { throw 'Portable packaging accepted a new public document unsupported by the old updater.' }
    } finally { [IO.File]::WriteAllText($manifest, $originalManifest) }
    if ((Get-FileHash -LiteralPath $archive).Hash -cne $archiveHash -or (Test-Path -LiteralPath (Join-Path $staleRoot $futureDocument))) {
        throw 'An incompatible portable package replaced the verified output.'
    }
    if (Test-Path -LiteralPath (Join-Path $staleRoot 'settings.json')) { throw 'Stale runtime state survived clean staging.' }
    foreach ($file in @('doc/Rust_Old_Report.json','doc/Rust_CI_Release.json','doc/assets/issues/private.png','resources/private/settings.json')) {
        if (Test-Path -LiteralPath (Join-Path $staleRoot $file)) { throw "Private file leaked through package: $file" }
    }
    $hiddenLeak = Join-Path $staleRoot '.private-report.json'
    Set-Content -LiteralPath $hiddenLeak -Value 'hidden private report'
    (Get-Item -LiteralPath $hiddenLeak -Force).Attributes = [IO.FileAttributes]::Hidden
    $rejected = $false
    try { & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot } catch { $rejected = $_.Exception.Message -match 'outside the public payload allowlist' }
    if (!$rejected) { throw 'Hidden payload leak was not rejected.' }
    Remove-Item -LiteralPath $hiddenLeak -Force
    # The privacy gate must reject actual machine paths embedded in a build.
    Set-Content -LiteralPath (Join-Path $staleRoot 'AudioLibCli.exe') -Value $env:USERPROFILE -Encoding ASCII
    $rejected = $false
    try { & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot } catch { $rejected = $_.Exception.Message -match 'Local build/user path' }
    if (!$rejected) { throw 'Machine-path regression was not rejected.' }
    Set-Content -LiteralPath (Join-Path $staleRoot 'AudioLibCli.exe') -Value 'synthetic build' -Encoding ASCII
    $report = @{
        success=$true; tag='v0.4.4'; version='0.4.4'; commit=('a' * 40)
        checked_at=[DateTime]::UtcNow.ToString('o'); checks=@('synthetic validation')
        binaries=@{}
        portable=@{ success=$true; mode='clean-checkout-synthetic'; records=1; source_unchanged=$true; python_runtime_required=$false }
    }
    foreach ($binary in @('AudioLibExplorer.exe','AudioLibCli.exe')) { $report.binaries[$binary] = (Get-FileHash -LiteralPath (Join-Path $staleRoot $binary)).Hash }
    $reportPath = Join-Path $testRoot 'public-report.json'
    $report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding UTF8
    & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReleaseReport $reportPath
    & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReleaseReport $reportPath -ReportOnly
    $rejected = $false
    try { & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReportOnly } catch { $rejected = $_.Exception.Message -match 'ReportOnly requires -ReleaseReport' }
    if (!$rejected) { throw 'Report-only validation accepted a missing report.' }
    foreach ($leak in @('unknown-field','escaped-path','wrong-binary','wrong-desktop-binary')) {
        $bad = $report | ConvertTo-Json -Depth 5 | ConvertFrom-Json
        switch ($leak) {
            'unknown-field' { $bad.portable | Add-Member -NotePropertyName screenshot -NotePropertyValue 'private.png' }
            'escaped-path' { $bad.checks = @($env:USERPROFILE) }
            'wrong-binary' { $bad.binaries.'AudioLibCli.exe' = ('0' * 64) }
            'wrong-desktop-binary' { $bad.binaries.'AudioLibExplorer.exe' = ('0' * 64) }
        }
        $bad | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding UTF8
        foreach ($reportOnly in @($false, $true)) {
            $rejected = $false
            try { & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReleaseReport $reportPath -ReportOnly:$reportOnly } catch { $rejected = $true }
            if (!$rejected) { throw "Public report leak was not rejected (ReportOnly=$reportOnly): $leak" }
        }
    }
    # Build-only reports keep the schema but leave runtime validation unrun.
    # These nulls must never be accepted as measured success, counts or flags.
    $notRun = $report | ConvertTo-Json -Depth 5 | ConvertFrom-Json
    $notRun.portable.mode = 'not-run'
    foreach ($field in @('success','records','source_unchanged','python_runtime_required')) { $notRun.portable.$field = $null }
    $notRun | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding UTF8
    & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReleaseReport $reportPath
    & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReleaseReport $reportPath -ReportOnly
    foreach ($invalid in @('success','records','source_unchanged','python_runtime_required','mode')) {
        $bad = $notRun | ConvertTo-Json -Depth 5 | ConvertFrom-Json
        switch ($invalid) {
            'success' { $bad.portable.success = $true }
            'records' { $bad.portable.records = 0 }
            'source_unchanged' { $bad.portable.source_unchanged = $false }
            'python_runtime_required' { $bad.portable.python_runtime_required = $false }
            'mode' { $bad.portable.mode = 'unknown' }
        }
        $bad | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding UTF8
        foreach ($reportOnly in @($false, $true)) {
            $rejected = $false
            try { & (Join-Path $testRoot 'tools/validate-package-privacy.ps1') -BinaryDirectory $staleRoot -ReleaseReport $reportPath -ReportOnly:$reportOnly } catch { $rejected = $true }
            if (!$rejected) { throw "Unrun portable report accepted invalid state (ReportOnly=$reportOnly): $invalid" }
        }
    }
    $dist = [IO.Path]::GetFullPath((Join-Path $testRoot 'dist'))
    $redirected = [IO.Path]::GetFullPath((Join-Path $testRoot 'redirected-output'))
    if (!$dist.StartsWith($testRoot + '\', [StringComparison]::OrdinalIgnoreCase) -or
        !$redirected.StartsWith($testRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Junction regression escaped test root.' }
    Move-Item -LiteralPath $dist -Destination (Join-Path $testRoot 'previous-output')
    New-Item -ItemType Directory -Path $redirected | Out-Null
    Set-Content -LiteralPath (Join-Path $redirected 'preserve.txt') -Value 'owned sentinel'
    try {
        New-Item -ItemType Junction -Path $dist -Target $redirected | Out-Null
        $rejected = $false
        try { & (Join-Path $testRoot 'tools/package-portable.ps1') -SkipBuild -WithoutAsio } catch { $rejected = $_.Exception.Message -match 'Linked package path' }
        if (!$rejected -or (Test-Path -LiteralPath (Join-Path $redirected 'latest')) -or
            (Get-Content -LiteralPath (Join-Path $redirected 'preserve.txt') -Raw).Trim() -ne 'owned sentinel') { throw 'Junction ancestor was followed or changed.' }
    } finally {
        if ((Test-Path -LiteralPath $dist) -and ((Get-Item -LiteralPath $dist -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            # Unlink only the exact test-created junction, without recursive deletion.
            if ($dist -ne [IO.Path]::GetFullPath((Join-Path $testRoot 'dist'))) { throw 'Unexpected junction cleanup target.' }
            # Windows PowerShell 5.1 Remove-Item fails on a junction to a populated
            # directory. Nonrecursive Directory.Delete removes only the link.
            [IO.Directory]::Delete($dist)
            if ((Get-Content -LiteralPath (Join-Path $redirected 'preserve.txt') -Raw).Trim() -ne 'owned sentinel') { throw 'Junction cleanup changed its target.' }
        }
    }
    Write-Host 'Package boundary regression passed: stale state/ignored reports/untracked assets excluded; old-updater compatibility and licenses checked; completed and unrun reports accepted; incompatible public docs, local paths, unsafe reports, false runtime results, wrong binary identities and junction ancestors rejected.'
} finally {
    $env:CARGO_TARGET_DIR = $savedTargetDirectory
    if (Test-Path -LiteralPath $testRoot) {
        if (!$testRoot.StartsWith($testParent + '\', [StringComparison]::OrdinalIgnoreCase) -or
            (Split-Path -Leaf $testRoot) -notmatch '^AudioLibPackageBoundary-[0-9a-f]{32}$' -or
            @(Get-ChildItem -LiteralPath $testRoot -Recurse -Force -Attributes ReparsePoint).Count -gt 0) { throw 'Unexpected regression cleanup path.' }
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
