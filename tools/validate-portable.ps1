# tools/validate-portable.ps1
[CmdletBinding()]
param(
    [string]$BinaryDirectory = 'dist/latest/portable',
    [switch]$CleanCheckout,
    # Routine acceptance uses synthetic data. Opt in only for legacy-import changes.
    [switch]$LegacyImport,
    # Match build-release/package-portable; ASIO remains required by default.
    [switch]$WithoutAsio,
    [string]$PublishedReleaseReport,
    [string]$ExpectedTag,
    [string]$ExpectedCommit
)
$ErrorActionPreference = 'Stop'
if ($CleanCheckout -and $LegacyImport) { throw 'Choose synthetic acceptance or -LegacyImport, not both.' }
# Keep -CleanCheckout as a compatible explicit spelling of the default.
$CleanCheckout = !$LegacyImport
. (Join-Path $PSScriptRoot 'console-utf8.ps1')
try {
if ($PublishedReleaseReport -and $PSVersionTable.PSVersion -lt [version]'7.3') { throw 'Published release validation requires pwsh (PowerShell 7.3 or newer).' }
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$binaryRoot = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($BinaryDirectory)) { $BinaryDirectory } else { Join-Path $taskRoot $BinaryDirectory }))
$cli = Join-Path $binaryRoot 'AudioLibCli.exe'
$desktop = Join-Path $binaryRoot 'AudioLibExplorer.exe'
# Reject incomplete packages before reading the legacy fixture or starting apps.
& (Join-Path $PSScriptRoot 'validate-portable-licenses.ps1') -BinaryDirectory $binaryRoot -WithoutAsio:$WithoutAsio
$tempParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([IO.Path]::DirectorySeparatorChar)
$testRoot = [IO.Path]::GetFullPath((Join-Path $tempParent ('AudioLibPortable-' + [guid]::NewGuid().ToString('N'))))
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $taskRoot '.rust-validation/portable-smoke'))
$savedDataRoot = $env:AUDIOLIB_DATA_DIR
$legacyPath = Join-Path $taskRoot 'data/audiolib.db'
$originalHash = if (!$CleanCheckout) { (Get-FileHash -LiteralPath $legacyPath -Algorithm SHA256).Hash } else { $null }
# Windows PowerShell 5 turns native stderr into terminating NativeCommandError records
# under ErrorActionPreference=Stop (notably when the caller redirects with *>&1), although
# the CLI writes informational notes there. Run the CLI with a local 'Continue' and judge
# success only by its exit code.
function Invoke-Cli([string[]]$Arguments) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $all = @(& $cli @Arguments 2>&1)
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $saved }
    [PSCustomObject]@{
        ExitCode=$code
        Output=($all | Where-Object { $_ -isnot [Management.Automation.ErrorRecord] } | Out-String)
        Error=($all | Where-Object { $_ -is [Management.Automation.ErrorRecord] } | ForEach-Object { $_.ToString() } | Out-String)
    }
}
function Invoke-DesktopSmoke([string[]]$Arguments, [string]$Name) {
    $stdout = Join-Path $testRoot ($Name + '.stdout')
    $stderr = Join-Path $testRoot ($Name + '.stderr')
    $process = Start-Process -FilePath $desktop -ArgumentList $Arguments -WorkingDirectory $binaryRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    # Windows PowerShell 5 reports no ExitCode unless the handle is opened before exit.
    $null = $process.Handle
    try {
        if (!$process.WaitForExit(30000)) { throw "Portable desktop $Name timed out." }
        return [PSCustomObject]@{ ExitCode=$process.ExitCode; Output=(Get-Content -LiteralPath $stdout -Raw); Error=(Get-Content -LiteralPath $stderr -Raw) }
    } finally {
        if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
}
try {
    if (!$testRoot.StartsWith($tempParent + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Portable smoke path escaped its owned temporary root.' }
    if (!$artifactRoot.StartsWith($taskRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Portable artifacts escaped the workspace.' }
    New-Item -ItemType Directory -Force -Path $testRoot,$artifactRoot | Out-Null
    $env:AUDIOLIB_DATA_DIR = Join-Path $testRoot 'userdata'
    if ($CleanCheckout) {
        # Explicit isolated preferences prevent discovery/import of a developer's
        # legacy database, even when this mode is invoked from their checkout.
        New-Item -ItemType Directory -Force -Path $env:AUDIOLIB_DATA_DIR | Out-Null
        # Without a BOM: Windows PowerShell 5's `-Encoding UTF8` writes one, which the
        # settings parser rejects.
        $settingsJson = @{active_database=(Join-Path $env:AUDIOLIB_DATA_DIR 'library.db');language='en';playback=@{volume=0.0};allowed_scan_roots=@()} |
            ConvertTo-Json -Depth 4
        [IO.File]::WriteAllText((Join-Path $env:AUDIOLIB_DATA_DIR 'settings.json'), $settingsJson, [Text.UTF8Encoding]::new($false))
    }
    $identityCheck = 'packaged EXE hashes match release'
    if ($PublishedReleaseReport) {
        # Download and verify the published archive/checksum first. CI builds may
        # differ from a local build; retain the published report as their baseline
        # instead of replacing local build outputs to satisfy the default check.
        if (!$CleanCheckout -or $ExpectedCommit -notmatch '^[0-9a-f]{40}$' -or $ExpectedTag -notmatch '^v\d+\.\d+\.\d+(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$') { throw 'Published validation requires CleanCheckout and an explicit tag/commit identity.' }
        & (Join-Path $PSScriptRoot 'validate-package-privacy.ps1') -BinaryDirectory $binaryRoot -ReleaseReport $PublishedReleaseReport
        $published = Get-Content -LiteralPath $PublishedReleaseReport -Raw | ConvertFrom-Json
        if ($published.tag -cne $ExpectedTag -or $published.commit -cne $ExpectedCommit) { throw 'Published release report does not match the expected tag/commit.' }
        $version = Invoke-Cli @('--version')
        $versionText = $version.Output
        if ($version.ExitCode -ne 0 -or $versionText.Trim() -cne ('AudioLibCli ' + $published.version)) { throw 'Published CLI version does not match its release report.' }
        $identityCheck = 'packaged EXE hashes match published release report'
    } else {
        if ($ExpectedTag -or $ExpectedCommit) { throw 'Expected tag/commit requires a published release report.' }
        # Same release output as package-portable.ps1: CARGO_TARGET_DIR when set, else <repo>/target.
        $targetRoot = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { $env:CARGO_TARGET_DIR } else { Join-Path $taskRoot $env:CARGO_TARGET_DIR })) } else { Join-Path $taskRoot 'target' }
        foreach ($binary in @('AudioLibExplorer.exe','AudioLibCli.exe')) {
            $sourceHash = (Get-FileHash -LiteralPath (Join-Path $targetRoot ('x86_64-pc-windows-msvc/release/' + $binary)) -Algorithm SHA256).Hash
            $packageHash = (Get-FileHash -LiteralPath (Join-Path $binaryRoot $binary) -Algorithm SHA256).Hash
            if ($sourceHash -ne $packageHash) { throw "Packaged binary differs from release: $binary" }
            # package-portable.ps1 writes `<exe>.sha256` as `<lowercase hex>  <file name>`.
            $sidecar = Join-Path $binaryRoot ($binary + '.sha256')
            if (!(Test-Path -LiteralPath $sidecar -PathType Leaf)) { throw "Missing checksum sidecar: $binary.sha256" }
            $sidecarText = [IO.File]::ReadAllText($sidecar).TrimEnd("`r", "`n")
            if ($sidecarText -cnotmatch '^[0-9a-f]{64}  (\S.*)$' -or $Matches[1] -cne $binary) { throw "Checksum sidecar must read '<lowercase hex>  $binary': $binary.sha256" }
            if ($sidecarText.Substring(0, 64) -cne $packageHash.ToLowerInvariant()) { throw "Checksum sidecar does not match the packaged binary: $binary.sha256" }
        }
        $identityCheck = 'packaged EXE hashes match release and their .sha256 sidecars'
    }
    # The archive next to the default package output must use updater-safe entry names.
    $portableArchive = Join-Path (Split-Path -Parent $binaryRoot) 'AudioLibExplorer-latest-win-x64-portable.zip'
    $zipCheck = $null
    if (Test-Path -LiteralPath $portableArchive) {
        $expectedEntries = @(Get-ChildItem -LiteralPath $binaryRoot -Recurse -Force -File | Where-Object { $_.DirectoryName -ne $binaryRoot -or $_.Name -notmatch '^AudioLib(Explorer|Cli)\.exe\.sha256$' } | ForEach-Object { $_.FullName.Substring($binaryRoot.Length).TrimStart('\', '/').Replace('\', '/') })
        & (Join-Path $PSScriptRoot 'validate-portable-zip.ps1') -Archive $portableArchive -ExpectedEntries $expectedEntries
        $zipCheck = 'portable ZIP entry names use forward slashes'
    }
    $initial = Invoke-DesktopSmoke @('--headless-smoke') 'headless'
    if ($initial.ExitCode -ne 0) { throw "Portable headless startup failed: $($initial.Error)" }
    $library = $initial.Output | ConvertFrom-Json
    $initialCount = if ($CleanCheckout) { 0 } else { 34 }
    if (!$library.success -or $library.total_count -ne $initialCount) { throw "Portable startup record count differs from expected $initialCount." }
    $expectedCount = $initialCount
    if ($CleanCheckout) {
        $fixtureRoot = Join-Path $testRoot 'fixtures'
        New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
        $fixture = Join-Path $fixtureRoot 'CI synthetic tone.wav'
        $writer = [IO.BinaryWriter]::new([IO.File]::Create($fixture))
        try {
            $rate = 48000; $bytes = $rate * 2
            $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $writer.Write([uint32]($bytes + 36))
            $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $writer.Write([uint32]16)
            $writer.Write([uint16]1); $writer.Write([uint16]1); $writer.Write([uint32]$rate)
            $writer.Write([uint32]($rate * 2)); $writer.Write([uint16]2); $writer.Write([uint16]16)
            $writer.Write([Text.Encoding]::ASCII.GetBytes('data')); $writer.Write([uint32]$bytes)
            for ($frame = 0; $frame -lt $rate; $frame++) {
                $writer.Write([int16](12000 * [Math]::Sin(2 * [Math]::PI * 619 * $frame / $rate)))
            }
        } finally { $writer.Dispose() }
        $fixtureHash = (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash
        $scanRun = Invoke-Cli @('scan', $fixtureRoot)
        if ($scanRun.ExitCode -ne 0) { throw "Packaged CLI synthetic fixture scan failed: $($scanRun.Error)" }
        $scan = $scanRun.Output | ConvertFrom-Json
        if (!$scan.success) { throw 'Packaged CLI synthetic fixture scan failed.' }
        $expectedCount = 1
    }
    $statsRun = Invoke-Cli @('stats')
    if ($statsRun.ExitCode -ne 0) { throw "Packaged CLI statistics failed: $($statsRun.Error)" }
    $stats = $statsRun.Output | ConvertFrom-Json
    if (!$stats.success -or $stats.data.audio_files -ne $expectedCount) { throw 'Packaged CLI statistics failed.' }
    $snapshot = Join-Path $artifactRoot 'chinese.png'
    $ui = Invoke-DesktopSmoke @('--smoke-ui','--smoke-language','cn','--screenshot',('"' + $snapshot + '"')) 'chinese'
    if ($ui.ExitCode -ne 0 -or !(Test-Path -LiteralPath $snapshot)) { throw "Portable Chinese UI smoke failed: $($ui.Error)" }
    $badRoot = Join-Path $testRoot 'bad-preferences'
    New-Item -ItemType Directory -Force -Path $badRoot | Out-Null
    Set-Content -LiteralPath (Join-Path $badRoot 'settings.json') -Value '{invalid JSON' -Encoding UTF8
    $env:AUDIOLIB_DATA_DIR = $badRoot
    $failure = Invoke-DesktopSmoke @('--headless-smoke') 'expected-failure'
    $fatalLog = Join-Path $badRoot 'logs/AudioLibExplorer-startup-error.log'
    if ($failure.ExitCode -ne 1 -or !(Test-Path -LiteralPath $fatalLog)) { throw 'Fatal preference failure did not return code 1 and persist a startup diagnostic.' }
    if ((Get-Content -LiteralPath $fatalLog -Raw) -notmatch 'Load AudioLib preferences') { throw 'Fatal log does not contain the startup context.' }
    $unchanged = if ($CleanCheckout) {
        $fixtureHash -eq (Get-FileHash -LiteralPath $fixture -Algorithm SHA256).Hash
    } else { $originalHash -eq (Get-FileHash -LiteralPath $legacyPath -Algorithm SHA256).Hash }
    if (!$unchanged) { throw 'Source changed during portable smoke.' }
    $report = [PSCustomObject]@{
        success=$true
        mode=$(if ($CleanCheckout) { 'clean-checkout-synthetic' } else { 'legacy-import' })
        checks=@($identityCheck,$(if ($CleanCheckout) { 'isolated empty first launch and synthetic WAV scan' } else { 'headless first launch imports 34 records' }),'packaged CLI statistics','Chinese UI snapshot and clean exit','invalid preferences return failure and synchronous diagnostic',$(if ($WithoutAsio) { 'Slint and SoundTouch license materials (non-ASIO build)' } else { 'Slint, SoundTouch and ASIO license materials' }),'source unchanged')
        records=$expectedCount
        screenshot='.rust-validation/portable-smoke/chinese.png'
        original_database_unchanged=$(if ($CleanCheckout) { $null } else { $unchanged })
        source_unchanged=$unchanged
        python_runtime_required=$false
    }
    $report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $taskRoot 'doc/Rust_Portable.json') -Encoding UTF8
    $report | ConvertTo-Json -Depth 5
} finally {
    $env:AUDIOLIB_DATA_DIR = $savedDataRoot
    if (Test-Path -LiteralPath $testRoot) {
        if (!$testRoot.StartsWith($tempParent + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Refusing cleanup outside owned portable test root.' }
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
} finally {
    Restore-AudioLibConsoleEncoding
}
