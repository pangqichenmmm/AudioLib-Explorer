# tools/build-macos-client-pipeline.ps1
# Keep immutable per-attempt build proof; publish the successful proof for the
# next pipeline stage only. A failed attempt never prevents a bounded retry.
#Requires -Version 7.3
param([Parameter(Mandatory)][string]$ExpectedCommit, [Parameter(Mandatory)][string]$ReportPath)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskOwned = [IO.Path]::GetFullPath((Join-Path $taskRoot '.rust-validation/release-pipeline'))
$ReportPath = [IO.Path]::GetFullPath($ReportPath, $taskRoot)
if (!$ReportPath.StartsWith($taskOwned + [IO.Path]::DirectorySeparatorChar, [StringComparison]::Ordinal)) { throw 'Pipeline build proof must stay in the owned pipeline directory.' }
$taskCursor = $ReportPath
while ($taskCursor) {
    $taskItem = Get-Item -LiteralPath $taskCursor -Force -ErrorAction SilentlyContinue
    if ($taskItem -and ($taskItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked build proof path.' }
    $taskCursor = [IO.Path]::GetDirectoryName($taskCursor)
}
$taskAttempt = Join-Path ([IO.Path]::GetDirectoryName($ReportPath)) ('build-' + [guid]::NewGuid().ToString('N') + '.json')
& (Join-Path $PSScriptRoot 'build-macos-client.ps1') -ExpectedCommit $ExpectedCommit -ReportPath $taskAttempt
if (!$?) { throw 'Native build failed.' }
[IO.File]::Copy($taskAttempt, $ReportPath, $true)
