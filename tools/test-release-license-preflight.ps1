# tools/test-release-license-preflight.ps1
# Exercise the real notice resolver with owned metadata and a function-only Cargo
# fixture. No compiler, network, package or application process is invoked.
#Requires -Version 7.3
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$taskOwned = Join-Path ([IO.Path]::GetTempPath()) ('AudioLibLicensePreflight-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $taskOwned 'tools') -Force | Out-Null
foreach ($name in @('check-release-licenses.ps1','dependency-licenses.ps1')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $taskOwned 'tools')
}
$taskFixturePackages = @()
foreach ($name in @('audiolib-desktop','audiolib-cli','audiolib-server','audiolib-db','synthetic-common','synthetic-asio')) {
    $directory = Join-Path $taskOwned ('registry/' + $name)
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $directory 'Cargo.toml'),'synthetic package')
    [IO.File]::WriteAllText((Join-Path $directory 'LICENSE'),'synthetic original license')
    $taskFixturePackages += [pscustomobject]@{name=$name;version='1.0.0';manifest_path=(Join-Path $directory 'Cargo.toml');source='registry+synthetic';license='MIT';license_file=$null;repository=$null}
}
$taskFixtureState = @{Packages=$taskFixturePackages;Target='';Features=@();NoDefault=$false;Calls=0}
function cargo {
    $taskFixtureState.Calls++
    if ($args[0] -cnotin @('metadata','tree') -or $args -notcontains '--offline' -or $args -notcontains '--locked') { throw 'Preflight attempted an online or compiling Cargo command.' }
    if (($args -contains '--no-default-features') -ne $taskFixtureState.NoDefault) { throw 'Preflight resolved the wrong default features.' }
    $targetFlag = if ($args[0] -ceq 'metadata') { '--filter-platform' } else { '--target' }
    $targetIndex = [Array]::IndexOf($args,$targetFlag)
    if ($targetIndex -lt 0 -or $args[$targetIndex+1] -cne $taskFixtureState.Target) { throw 'Preflight resolved the wrong target.' }
    $featureIndex = [Array]::IndexOf($args,'--features')
    $features = if ($featureIndex -ge 0) { @($args[$featureIndex+1] -split ',') } else { @() }
    if (($features -join ',') -cne ($taskFixtureState.Features -join ',')) { throw 'Preflight resolved the wrong ASIO features.' }
    if ($args[0] -ceq 'metadata') {
        @{packages=$taskFixtureState.Packages} | ConvertTo-Json -Depth 8
    } else {
        if ($args -notcontains 'normal,build') { throw 'Preflight selected development dependencies.' }
        for ($index = 0; $index -lt $args.Count; $index++) {
            if ($args[$index] -ceq '-p') { "$($args[$index+1]) v1.0.0" }
        }
        'synthetic-common v1.0.0'
        if ($features.Count) { 'synthetic-asio v1.0.0' }
    }
    $global:LASTEXITCODE = 0
}
function Invoke-NoticePreflight([string]$Profile,[string]$Target,[bool]$WithoutAsio=$false) {
    $taskFixtureState.Target = $Target
    $taskFixtureState.Features = if ($Profile -ceq 'WindowsClient' -and !$WithoutAsio) { @('audiolib-desktop/asio','audiolib-cli/asio') } else { @() }
    $taskFixtureState.NoDefault = $Profile -cne 'WindowsClient'
    $result = & (Join-Path $taskOwned 'tools/check-release-licenses.ps1') -Profile $Profile -Target $Target -WithoutAsio:$WithoutAsio | ConvertFrom-Json
    if ($result.status -cne 'passed' -or $result.target -cne $Target -or $result.packages -ne $(if ($taskFixtureState.Features.Count) {4} else {3})) { throw 'Preflight omitted a selected dependency notice.' }
}
try {
    Invoke-NoticePreflight 'WindowsClient' 'x86_64-pc-windows-msvc'
    Invoke-NoticePreflight 'WindowsClient' 'x86_64-pc-windows-msvc' $true
    Invoke-NoticePreflight 'MacosClient' 'aarch64-apple-darwin'
    foreach ($target in @('x86_64-pc-windows-msvc','x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu','aarch64-apple-darwin')) { Invoke-NoticePreflight 'Server' $target }
    if ($taskFixtureState.Calls -ne 14) { throw 'Each profile must resolve one metadata/tree pair.' }
    Write-Host 'PASS: Windows ASIO/non-ASIO, Mac NoDefault and four Server NoDefault targets use locked/offline normal/build closures.'
    $taskCommon = $taskFixturePackages | Where-Object name -CEQ 'synthetic-common'
    $taskLicense = Join-Path (Split-Path -Parent $taskCommon.manifest_path) 'LICENSE'
    Remove-Item -LiteralPath $taskLicense
    $rejected = $false
    try { Invoke-NoticePreflight 'MacosClient' 'aarch64-apple-darwin' } catch { $rejected = $_.Exception.Message -like '*Required license text unavailable*' }
    if (!$rejected) { throw 'A previously accepted external notice disappeared without failing preflight.' }
    [IO.File]::WriteAllText($taskLicense,'restored original license')
    Invoke-NoticePreflight 'MacosClient' 'aarch64-apple-darwin'
    $taskCommon.license_file = Join-Path (Split-Path -Parent $taskCommon.manifest_path) 'missing-declared-license.txt'
    $rejected = $false
    try { Invoke-NoticePreflight 'MacosClient' 'aarch64-apple-darwin' } catch { $rejected = $_.Exception.Message -like '*missing-declared-license.txt*' }
    if (!$rejected) { throw 'An absent declared license_file was accepted.' }
    Write-Host 'PASS: removed external notices and missing declared license files fail before any payload work; restored notices pass.'
} finally {
    Remove-Item Function:\cargo -ErrorAction SilentlyContinue
    $taskTempParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\','/')
    $taskResolved = [IO.Path]::GetFullPath($taskOwned)
    if (!$taskResolved.StartsWith($taskTempParent + [IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($taskResolved) -notmatch '^AudioLibLicensePreflight-[0-9a-f]{32}$') { throw 'Refusing cleanup outside the owned license fixture.' }
    Remove-Item -LiteralPath $taskResolved -Recurse -Force
}
Write-Host 'Release license preflight synthetic regressions passed.'
