# tools/test-local-waveform-benchmark.ps1
# Exercise the benchmark wrapper with a synthetic runner; never invoke Cargo/audio.
[CmdletBinding()]
param([string]$ArtifactParent)
$ErrorActionPreference = 'Stop'
$taskRepo = Split-Path -Parent $PSScriptRoot
$taskWrapper = Join-Path $PSScriptRoot 'benchmark-local-waveforms.ps1'
$taskArtifactParent = if ([string]::IsNullOrWhiteSpace($ArtifactParent)) {
    $taskDefaultParent = Join-Path $taskRepo '.rust-validation/benchmark-script-tests'
    New-Item -ItemType Directory -Path $taskDefaultParent -Force | Out-Null
    $taskDefaultParent
} else {
    if (!(Test-Path -LiteralPath $ArtifactParent -PathType Container)) { throw 'ArtifactParent must be an existing directory.' }
    (Resolve-Path -LiteralPath $ArtifactParent).Path
}
$taskArtifacts = Join-Path ([IO.Path]::GetFullPath($taskArtifactParent)) ([guid]::NewGuid().ToString('N'))
# Keep the tiny owned JSON artifacts for diagnosis; never remove a caller's files.
New-Item -ItemType Directory -Path $taskArtifacts | Out-Null
$taskSavedMockVariable = Get-Variable -Name AudioLibWaveformBenchmarkTest -Scope Global -ErrorAction SilentlyContinue
$taskHadMockState = $null -ne $taskSavedMockVariable
$taskSavedMockState = if ($taskHadMockState) { $taskSavedMockVariable.Value } else { $null }
$global:AudioLibWaveformBenchmarkTest = @{ mode='fresh'; calls=0; seconds=1; wrongLabel=$false }

function New-MockReport([string]$Label, $Seconds) {
    @{
        label=$Label; workload_version=1; files=12; source_bytes=1000; source_frames=1000;
        sample_rate=48000; pcm_bits=@(16,24); channels=@(1,2,4); durations_seconds=@(0.25,1,4,16);
        source_parent='owned-fixture-parent'; database_parent='owned-database-parent';
        database_location='system_temp'; available_parallelism=1; os='windows'; architecture='x86_64';
        cold_seconds=$Seconds; cold_files_per_second=12; generated=12; failed=0;
        waveforms_matching_independent_oracle=12; warm_files_decoded=0
    }
}

# PowerShell functions take precedence over executables. Only this script's child
# scope sees the fake runner; the application and the real Cargo are never started.
function cargo {
    $global:AudioLibWaveformBenchmarkTest.calls++
    if ($global:AudioLibWaveformBenchmarkTest.mode -ne 'no-test') {
        $taskMockLabel = if ($global:AudioLibWaveformBenchmarkTest.wrongLabel) { 'wrong-label' } else { $env:AUDIOLIB_WAVEFORM_BENCH_LABEL }
        New-MockReport $taskMockLabel $global:AudioLibWaveformBenchmarkTest.seconds |
            ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $env:AUDIOLIB_WAVEFORM_BENCH_REPORT -Encoding utf8
    }
    $global:LASTEXITCODE = if ($global:AudioLibWaveformBenchmarkTest.mode -eq 'cargo-failure') { 1 } else { 0 }
}
if ((Get-Command cargo).CommandType -ne 'Function') { throw 'The synthetic runner is not active.' }

function Set-TestEnvironment([string]$Name, $Value) {
    if ($null -eq $Value) {
        Remove-Item -LiteralPath "Env:$Name" -ErrorAction SilentlyContinue
    } else {
        [Environment]::SetEnvironmentVariable($Name, $Value, 'Process')
    }
}

$taskEnvironmentKeys = @('AUDIOLIB_WAVEFORM_BENCH_FILES','AUDIOLIB_WAVEFORM_BENCH_LABEL','AUDIOLIB_WAVEFORM_BENCH_REPORT','AUDIOLIB_WAVEFORM_BENCH_PARENT','RUSTUP_TOOLCHAIN')
$taskOriginalEnvironment = @{}
foreach ($taskKey in $taskEnvironmentKeys) {
    $taskOriginalEnvironment[$taskKey] = [Environment]::GetEnvironmentVariable($taskKey, 'Process')
}

function Invoke-WrapperCase([string]$Name, [hashtable]$Arguments, [bool]$ShouldPass, [string]$ErrorPattern, [int]$ExpectedCalls = 1) {
    $taskBeforeEnvironment = @{}
    foreach ($taskKey in $taskEnvironmentKeys) {
        $taskBeforeEnvironment[$taskKey] = [Environment]::GetEnvironmentVariable($taskKey, 'Process')
    }
    $taskBeforeLocation = (Get-Location).Path
    $taskBeforeConsole = [Console]::OutputEncoding.CodePage
    $taskCallsBefore = $global:AudioLibWaveformBenchmarkTest.calls
    $taskFailure = $null
    try { & $taskWrapper @Arguments } catch { $taskFailure = $_ }
    if ($ShouldPass -and $taskFailure) { throw "$Name unexpectedly failed: $taskFailure" }
    if (!$ShouldPass -and !$taskFailure) { throw "$Name unexpectedly passed." }
    if (!$ShouldPass -and $taskFailure.ToString() -notmatch $ErrorPattern) { throw "$Name failed for the wrong reason: $taskFailure" }
    if ($global:AudioLibWaveformBenchmarkTest.calls - $taskCallsBefore -ne $ExpectedCalls) { throw "$Name invoked the wrong number of synthetic runs." }
    foreach ($taskKey in $taskEnvironmentKeys) {
        $taskAfter = [Environment]::GetEnvironmentVariable($taskKey, 'Process')
        if ($taskAfter -cne $taskBeforeEnvironment[$taskKey]) { throw "$Name did not restore $taskKey." }
    }
    if ((Get-Location).Path -cne $taskBeforeLocation) { throw "$Name did not restore the current directory." }
    if ([Console]::OutputEncoding.CodePage -ne $taskBeforeConsole) { throw "$Name did not restore the console encoding." }
    if (@(Get-ChildItem -LiteralPath $taskArtifacts -Filter '.local-waveforms-*.raw.json' -File -Force).Count) { throw "$Name left its private raw report behind." }
    Write-Host "PASS: $Name"
}

function Assert-SameHash([string]$Path, [string]$Expected) {
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -cne $Expected) { throw "A failed run changed the existing report: $Path" }
}

try {
    Set-TestEnvironment AUDIOLIB_WAVEFORM_BENCH_FILES 'original-files'
    Set-TestEnvironment AUDIOLIB_WAVEFORM_BENCH_LABEL 'original-label'
    Set-TestEnvironment AUDIOLIB_WAVEFORM_BENCH_REPORT $null
    Set-TestEnvironment AUDIOLIB_WAVEFORM_BENCH_PARENT ''
    Set-TestEnvironment RUSTUP_TOOLCHAIN 'original-toolchain'

    $taskOutput = Join-Path $taskArtifacts 'accepted.json'
    $taskBaseline = Join-Path $taskArtifacts 'baseline.json'
    # A real run that fails baseline comparison leaves an unannotated raw report
    # under the old implementation. Such a file must not satisfy a later 0-test run.
    New-MockReport 'previous-run' 1 | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $taskOutput -Encoding utf8
    $taskOldHash = (Get-FileHash -LiteralPath $taskOutput -Algorithm SHA256).Hash
    $global:AudioLibWaveformBenchmarkTest.mode = 'no-test'
    Invoke-WrapperCase 'zero matching tests rejects a stale report' @{Label='current';Files=12;ReportPath=$taskOutput} $false 'fresh report'
    Assert-SameHash $taskOutput $taskOldHash

    Invoke-WrapperCase 'baseline cannot be the output report' @{Label='current';Files=12;ReportPath=$taskOutput;BaselinePath=$taskOutput} $false 'must differ' 0
    Assert-SameHash $taskOutput $taskOldHash

    Invoke-WrapperCase 'invalid fixture parent preserves caller state' @{Label='current';Files=12;ReportPath=$taskOutput;FixtureParent=(Join-Path $taskArtifacts 'missing')} $false 'does not exist|Cannot find path' 0
    Assert-SameHash $taskOutput $taskOldHash

    $global:AudioLibWaveformBenchmarkTest.mode = 'cargo-failure'
    Invoke-WrapperCase 'failed runner cannot publish partial output' @{Label='current';Files=12;ReportPath=$taskOutput} $false 'cargo exit 1'
    Assert-SameHash $taskOutput $taskOldHash

    $global:AudioLibWaveformBenchmarkTest.mode = 'fresh'
    $global:AudioLibWaveformBenchmarkTest.wrongLabel = $true
    Invoke-WrapperCase 'wrong run identity cannot publish a report' @{Label='current';Files=12;ReportPath=$taskOutput} $false 'requested workload'
    Assert-SameHash $taskOutput $taskOldHash
    $global:AudioLibWaveformBenchmarkTest.wrongLabel = $false

    # The fixture/CPU metadata are identical in each synthetic baseline comparison.
    $taskBaselineReport = New-MockReport 'before' 20
    if ($env:OS -eq 'Windows_NT') {
        $taskBaselineReport.processors = @(Get-CimInstance -ClassName Win32_Processor | ForEach-Object {
            @{ name=$_.Name; cores=$_.NumberOfCores; logical_processors=$_.NumberOfLogicalProcessors }
        })
    }
    foreach ($taskBadSeconds in @('NaN','Infinity',0,-1)) {
        $taskBaselineReport.cold_seconds = $taskBadSeconds
        $taskBaselineReport | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $taskBaseline -Encoding utf8
        $taskBaselineHash = (Get-FileHash -LiteralPath $taskBaseline -Algorithm SHA256).Hash
        Invoke-WrapperCase "invalid baseline duration ($taskBadSeconds) is rejected" @{Label='current';Files=12;ReportPath=$taskOutput;BaselinePath=$taskBaseline} $false 'positive and finite'
        Assert-SameHash $taskOutput $taskOldHash
        Assert-SameHash $taskBaseline $taskBaselineHash
    }

    $taskBaselineReport.cold_seconds = 20
    $taskBaselineReport | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $taskBaseline -Encoding utf8
    $taskBaselineHash = (Get-FileHash -LiteralPath $taskBaseline -Algorithm SHA256).Hash
    $global:AudioLibWaveformBenchmarkTest.seconds = 3
    Invoke-WrapperCase 'below-target result preserves the accepted report' @{Label='current';Files=12;ReportPath=$taskOutput;BaselinePath=$taskBaseline} $false 'below required'
    Assert-SameHash $taskOutput $taskOldHash
    Assert-SameHash $taskBaseline $taskBaselineHash

    $global:AudioLibWaveformBenchmarkTest.seconds = 1
    Invoke-WrapperCase 'a fresh passing run replaces the accepted report' @{Label='current';Files=12;ReportPath=$taskOutput;BaselinePath=$taskBaseline} $true ''
    $taskPublished = Get-Content -LiteralPath $taskOutput -Raw | ConvertFrom-Json
    if ($taskPublished.label -cne 'current' -or $taskPublished.speedup -ne 20 -or !$taskPublished.source_commit) { throw 'Published report is incomplete.' }
    Assert-SameHash $taskBaseline $taskBaselineHash

    $taskNewOutput = Join-Path $taskArtifacts 'new-report.json'
    Invoke-WrapperCase 'a fresh passing run publishes a new report' @{Label='new';Files=12;ReportPath=$taskNewOutput} $true ''
    if ((Get-Content -LiteralPath $taskNewOutput -Raw | ConvertFrom-Json).label -cne 'new') { throw 'New report was not published.' }
    Write-Host "PASS: benchmark wrapper regressions used only a synthetic runner; no Cargo/audio was run. Artifacts: $taskArtifacts"
} finally {
    foreach ($taskKey in $taskEnvironmentKeys) { Set-TestEnvironment $taskKey $taskOriginalEnvironment[$taskKey] }
    if ($taskHadMockState) {
        $global:AudioLibWaveformBenchmarkTest = $taskSavedMockState
    } else {
        Remove-Variable -Name AudioLibWaveformBenchmarkTest -Scope Global
    }
}
