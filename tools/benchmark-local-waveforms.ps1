# tools/benchmark-local-waveforms.ps1
# Measure the actual local pre-generation worker with owned WAVs and an empty cache.
# FixtureParent moves only the source WAVs; the library DB remains in system temp.
param(
    [ValidatePattern('^[A-Za-z0-9_-]+$')][string]$Label = 'current',
    [ValidateRange(12,1200)][int]$Files = 120,
    [string]$ReportPath,
    [string]$FixtureParent,
    [string]$BaselinePath,
    [ValidateRange(1,1000)][double]$MinimumSpeedup = 10,
    [string]$Toolchain = '1.97.1-x86_64-pc-windows-msvc'
)
$ErrorActionPreference = 'Stop'
$taskSavedEnvironment = @{}
$taskLocationPushed = $false
$taskConsoleInitialized = $false
$taskRawReport = $null
try {
$taskRepo = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $taskRepo ".rust-validation/local-waveforms/$Label.json"
}
$taskReport = [IO.Path]::GetFullPath($ReportPath)
$taskBaselinePath = $null
if (![string]::IsNullOrWhiteSpace($BaselinePath)) {
    $taskBaselinePath = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($BaselinePath)) { $BaselinePath } else { Join-Path $taskRepo $BaselinePath }))
    $taskPathComparison = if ($env:OS -eq 'Windows_NT') { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
    if ($taskReport.Equals($taskBaselinePath, $taskPathComparison)) {
        throw 'ReportPath must differ from BaselinePath; the baseline must be preserved.'
    }
}
$taskReportParent = Split-Path -Parent $taskReport
New-Item -ItemType Directory -Force -Path $taskReportParent | Out-Null
# A no-match Cargo filter exits successfully. Give this invocation an unguessable
# output name so an older raw or accepted report cannot stand in for a new run.
$taskRawReport = Join-Path $taskReportParent ('.local-waveforms-' + [guid]::NewGuid().ToString('N') + '.raw.json')
$taskEnvironment = @{
    AUDIOLIB_WAVEFORM_BENCH_FILES = [string]$Files
    AUDIOLIB_WAVEFORM_BENCH_LABEL = $Label
    AUDIOLIB_WAVEFORM_BENCH_REPORT = $taskRawReport
    AUDIOLIB_WAVEFORM_BENCH_PARENT = $null
    RUSTUP_TOOLCHAIN = $Toolchain
}
if (![string]::IsNullOrWhiteSpace($FixtureParent)) {
    $taskFixtureParent = (Resolve-Path -LiteralPath $FixtureParent).Path
    if (!(Test-Path -LiteralPath $taskFixtureParent -PathType Container)) {
        throw 'FixtureParent must be an existing directory.'
    }
    $taskEnvironment.AUDIOLIB_WAVEFORM_BENCH_PARENT = $taskFixtureParent
}
. (Join-Path $PSScriptRoot 'console-utf8.ps1')
$taskConsoleInitialized = $true
foreach ($taskKey in $taskEnvironment.Keys) {
    $taskSavedEnvironment[$taskKey] = [Environment]::GetEnvironmentVariable($taskKey, 'Process')
    if ($null -eq $taskEnvironment[$taskKey]) {
        Remove-Item -LiteralPath "Env:$taskKey" -ErrorAction SilentlyContinue
    } else {
        [Environment]::SetEnvironmentVariable($taskKey, $taskEnvironment[$taskKey], 'Process')
    }
}
Push-Location -LiteralPath $taskRepo
$taskLocationPushed = $true
    & cargo test -p audiolib-desktop --release --locked local_waveform_pregeneration_benchmark -- --ignored --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Local Waveform benchmark failed: cargo exit $LASTEXITCODE" }
    if (!(Test-Path -LiteralPath $taskRawReport -PathType Leaf)) { throw 'Benchmark did not produce a fresh report; no matching test may have run.' }
    $taskResult = Get-Content -Raw -LiteralPath $taskRawReport | ConvertFrom-Json
    if ($taskResult.label -cne $Label -or $taskResult.files -ne $Files -or
        $taskResult.generated -ne $Files -or $taskResult.waveforms_matching_independent_oracle -ne $Files -or
        $taskResult.failed -ne 0 -or $taskResult.warm_files_decoded -ne 0) {
        throw 'The fresh report does not describe a successful run of the requested workload.'
    }
    $taskColdSeconds = [double]$taskResult.cold_seconds
    if ([double]::IsNaN($taskColdSeconds) -or [double]::IsInfinity($taskColdSeconds) -or $taskColdSeconds -le 0) {
        throw 'The measured cold duration must be positive and finite.'
    }
    $taskCommit = & git rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Unable to identify source commit.' }
    $taskResult | Add-Member -NotePropertyName source_commit -NotePropertyValue $taskCommit.Trim()
    $taskChanges = @(& git status --porcelain)
    if ($LASTEXITCODE -ne 0) { throw 'Unable to identify source changes.' }
    $taskResult | Add-Member -NotePropertyName source_worktree_dirty -NotePropertyValue ($taskChanges.Count -gt 0)
    $taskResult | Add-Member -NotePropertyName measured_at_utc -NotePropertyValue ([DateTime]::UtcNow.ToString('o'))
    if ($env:OS -eq 'Windows_NT') {
        $taskProcessors = @(Get-CimInstance -ClassName Win32_Processor | ForEach-Object {
            @{ name = $_.Name; cores = $_.NumberOfCores; logical_processors = $_.NumberOfLogicalProcessors }
        })
        $taskResult | Add-Member -NotePropertyName processors -NotePropertyValue $taskProcessors
    }
    if ($taskBaselinePath) {
        $taskBaseline = Get-Content -Raw -LiteralPath $taskBaselinePath | ConvertFrom-Json
        foreach ($taskField in @('workload_version','files','source_bytes','source_frames','sample_rate','pcm_bits','channels','durations_seconds','source_parent','database_parent','database_location','available_parallelism','os','architecture')) {
            $taskBefore = ConvertTo-Json -InputObject $taskBaseline.$taskField -Compress
            $taskAfter = ConvertTo-Json -InputObject $taskResult.$taskField -Compress
            if ($taskBefore -ne $taskAfter) { throw "Baseline workload differs: $taskField" }
        }
        if ($env:OS -eq 'Windows_NT') {
            $taskBeforeCpu = (@($taskBaseline.processors) | ForEach-Object {
                '{0}|{1}|{2}' -f $_.name, $_.cores, $_.logical_processors
            } | Sort-Object) -join ';'
            $taskAfterCpu = (@($taskResult.processors) | ForEach-Object {
                '{0}|{1}|{2}' -f $_.name, $_.cores, $_.logical_processors
            } | Sort-Object) -join ';'
            if ($taskBeforeCpu -ne $taskAfterCpu) { throw 'Baseline processor hardware differs.' }
        }
        $taskBaselineSeconds = [double]$taskBaseline.cold_seconds
        if ([double]::IsNaN($taskBaselineSeconds) -or [double]::IsInfinity($taskBaselineSeconds) -or $taskBaselineSeconds -le 0) {
            throw 'The baseline cold duration must be positive and finite.'
        }
        $taskSpeedup = $taskBaselineSeconds / $taskColdSeconds
        if ([double]::IsNaN($taskSpeedup) -or [double]::IsInfinity($taskSpeedup)) { throw 'The measured speedup must be finite.' }
        $taskResult | Add-Member -NotePropertyName baseline_label -NotePropertyValue $taskBaseline.label
        $taskResult | Add-Member -NotePropertyName baseline_cold_seconds -NotePropertyValue $taskBaseline.cold_seconds
        $taskResult | Add-Member -NotePropertyName speedup -NotePropertyValue $taskSpeedup
        $taskResult | Add-Member -NotePropertyName minimum_speedup -NotePropertyValue $MinimumSpeedup
        Write-Host ('Speedup: {0:N2}x ({1:N3}s -> {2:N3}s)' -f $taskSpeedup, $taskBaseline.cold_seconds, $taskResult.cold_seconds)
        if ($taskSpeedup -lt $MinimumSpeedup) {
            throw "Measured speedup $taskSpeedup is below required $MinimumSpeedup. The previous accepted report was preserved."
        }
    }
    # Publish only after every check passes. A failed run keeps the prior accepted
    # report intact; replace on the same filesystem instead of truncating it.
    $taskResult | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $taskRawReport -Encoding utf8
    if ([IO.File]::Exists($taskReport)) {
        [IO.File]::Replace($taskRawReport, $taskReport, [NullString]::Value)
    } else {
        [IO.File]::Move($taskRawReport, $taskReport)
    }
    Write-Host ('PASS: {0} files, {1:N3}s, {2:N2} files/s; all peaks match; warm pass decodes zero files.' -f $Files, $taskResult.cold_seconds, $taskResult.cold_files_per_second)
    Write-Host "Report: $taskReport"
} finally {
    if ($taskLocationPushed) { Pop-Location }
    foreach ($taskKey in $taskSavedEnvironment.Keys) {
        if ($null -eq $taskSavedEnvironment[$taskKey]) {
            Remove-Item -LiteralPath "Env:$taskKey" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($taskKey, $taskSavedEnvironment[$taskKey], 'Process')
        }
    }
    # Delete only this invocation's unique raw file, never a prior report/baseline.
    if ($taskRawReport -and [IO.File]::Exists($taskRawReport)) {
        Remove-Item -LiteralPath $taskRawReport -Force
    }
    if ($taskConsoleInitialized) { Restore-AudioLibConsoleEncoding }
}
