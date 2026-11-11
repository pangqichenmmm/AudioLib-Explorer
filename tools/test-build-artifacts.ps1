# tools/test-build-artifacts.ps1
# Destructive cases use only synthetic caches beneath this invocation's owned
# temporary Git checkout. No real cache, SDK, application or library is changed.
#Requires -Version 7.3
$ErrorActionPreference='Stop'
$taskRoot=Join-Path ([IO.Path]::GetTempPath()) ('AudioLibCacheTest-'+[guid]::NewGuid().ToString('N'))
$taskTarget=Join-Path $taskRoot 'target'
$taskCleaner=Join-Path $PSScriptRoot 'clean-build-artifacts.ps1'
$taskSignature="Signature: 8a477f597d28d172789f06886806bc55`n# synthetic Cargo fixture"
New-Item -ItemType Directory -Path $taskRoot | Out-Null
& git -C $taskRoot init -q
if($LASTEXITCODE -ne 0){throw 'Fixture Git initialization failed.'}
& git -C $taskRoot config core.autocrlf false
[IO.File]::WriteAllText((Join-Path $taskRoot 'Cargo.toml'),"[workspace]`nmembers=[`"crates/audiolib-core`"]`n")
. (Join-Path $PSScriptRoot 'build-cache.ps1')
$global:taskCacheTestActive=$false
$global:taskCacheTestProbeCount=0
$global:taskCacheTestStartAt=0
function Get-Process {$global:taskCacheTestProbeCount++;if($global:taskCacheTestActive -or ($global:taskCacheTestStartAt -gt 0 -and $global:taskCacheTestProbeCount -ge $global:taskCacheTestStartAt)){[pscustomobject]@{ProcessName='rustc';Id=123;Path=(Join-Path $taskTarget 'debug/rustc.exe')}}}
function New-Cache([string]$Path){
    New-Item -ItemType Directory -Path $Path -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $Path 'CACHEDIR.TAG'),$taskSignature)
    [IO.File]::WriteAllText((Join-Path $Path '.rustc_info.json'),'{}')
    foreach($name in @('debug/deps/test.exe','debug/incremental/session/cache.bin','x86_64-pc-windows-msvc/debug/deps/test.exe','x86_64-pc-windows-msvc/release/app.exe','doc/index.html','tmp/temp.bin')){
        $file=Join-Path $Path $name
        New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($file)) -Force | Out-Null
        [IO.File]::WriteAllBytes($file,[byte[]]::new(256))
    }
}
function Invoke-Clean([hashtable]$Options=@{},[int]$ExpectedExit=0){
    $parameters=@{Root=$taskRoot;TargetDirectory=$taskTarget;Action='Run';Mode='Debug';MaxGiB=0}
    foreach($key in $Options.Keys){$parameters[$key]=$Options[$key]}
    $output=& $taskCleaner @parameters *>&1
    if($LASTEXITCODE -ne $ExpectedExit){throw "Unexpected cleanup result: $($output|Out-String)"}
    $json=@($output|ForEach-Object{$_.ToString()}|Where-Object{$_ -match '^\{'})
    if($json.Count -ne 1){throw 'Cleanup did not produce one compact JSON record.'}
    $json[0]|ConvertFrom-Json
}
New-Cache $taskTarget
$preview=Invoke-Clean @{Action='Plan'}
if($preview.status -ne 'planned' -or $preview.removed_bytes -ne 0 -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'debug/deps/test.exe'))){throw 'Preview changed artifacts.'}
$underBudget=Invoke-Clean @{MaxGiB=40}
if($underBudget.removed_bytes -ne 0 -or !$underBudget.budget_met){throw 'Small cache was needlessly removed.'}
$whatif=Invoke-Clean @{WhatIf=$true}
if($whatif.removed_bytes -ne 0 -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'debug/deps/test.exe'))){throw 'WhatIf deleted artifacts.'}
$lease=Enter-AudioLibBuildLease $taskRoot
try{$blocked=Invoke-Clean @{} 2;if($blocked.status -ne 'blocked'){throw 'Active build lease was ignored.'}}finally{$lease.Dispose()}
$global:taskCacheTestActive=$true
$blocked=Invoke-Clean @{} 2
$global:taskCacheTestActive=$false
if($blocked.status -ne 'blocked'){throw 'Active compiler was ignored.'}
$global:taskCacheTestProbeCount=0
$global:taskCacheTestStartAt=2
try{
    $blocked=Invoke-Clean @{} 2
    if($blocked.removed_bytes -ne 0 -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'debug/deps/test.exe'))){throw 'A compiler starting after inventory was ignored.'}
}finally{$global:taskCacheTestStartAt=0}
$incremental=Invoke-Clean @{Mode='Incremental'}
if($incremental.removed_bytes -ne 256 -or (Test-Path -LiteralPath (Join-Path $taskTarget 'debug/incremental')) -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'debug/deps/test.exe'))){throw 'Incremental cleanup exceeded its boundary.'}
$age=Invoke-Clean @{OlderThanDays=30}
if($age.removed_bytes -ne 0){throw 'Recent cache ignored retention age.'}
Write-Host 'PASS: preview/WhatIf/budget/age, shared build leases and active compilers; incremental cleanup preserves compiled outputs.'
$outside=Join-Path $taskRoot 'outside'
New-Item -ItemType Directory -Path $outside | Out-Null
[IO.File]::WriteAllText((Join-Path $outside 'keep.txt'),'outside untouched')
$link=Join-Path $taskTarget 'debug/linked'
if($IsWindows){New-Item -ItemType Junction -Path $link -Target $outside | Out-Null}else{$null=[IO.Directory]::CreateSymbolicLink($link,$outside)}
try{
    $null=Invoke-Clean @{} 1
    if([IO.File]::ReadAllText((Join-Path $outside 'keep.txt')) -cne 'outside untouched' -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'debug/deps/test.exe'))){throw 'Linked tree escaped cleanup scope.'}
}finally{
    if(![IO.Path]::GetFullPath($link).StartsWith($taskTarget+[IO.Path]::DirectorySeparatorChar,[StringComparison]::Ordinal)){throw 'Fixture cleanup escaped owned target.'}
    [IO.Directory]::Delete($link)
}
[IO.File]::WriteAllText((Join-Path $taskTarget 'working.db'),'not a cache')
$null=Invoke-Clean @{} 1
Remove-Item -LiteralPath (Join-Path $taskTarget 'working.db')
Remove-Item -LiteralPath (Join-Path $taskTarget 'CACHEDIR.TAG')
$null=Invoke-Clean @{} 1
[IO.File]::WriteAllText((Join-Path $taskTarget 'CACHEDIR.TAG'),$taskSignature)
foreach($protected in @($taskRoot,(Join-Path $taskRoot 'local_release'),(Join-Path $taskRoot 'tools/.build-tools'),(Join-Path $taskRoot 'data'))){$null=Invoke-Clean @{TargetDirectory=$protected} 1}
$debug=Invoke-Clean
if((Test-Path -LiteralPath (Join-Path $taskTarget 'debug')) -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'x86_64-pc-windows-msvc/release/app.exe')) -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'CACHEDIR.TAG'))){throw 'Debug cleanup removed release/root markers or retained debug outputs.'}
$all=Invoke-Clean @{Mode='All'}
if((Test-Path -LiteralPath (Join-Path $taskTarget 'x86_64-pc-windows-msvc/release')) -or (Test-Path -LiteralPath (Join-Path $taskTarget 'doc')) -or !(Test-Path -LiteralPath (Join-Path $taskTarget '.rustc_info.json'))){throw 'Explicit All scope is incorrect.'}
Write-Host 'PASS: markers/layout/links/protected paths checked; debug preserves release; explicit All only removes Cargo profiles/docs/tmp.'
$external=Join-Path $taskRoot 'external-target'
New-Cache $external
$preview=Invoke-Clean @{Action='Plan';TargetDirectory=$external}
if($preview.ownership -ne 'unregistered-preview-only'){throw 'External preview silently claimed ownership.'}
$null=Invoke-Clean @{TargetDirectory=$external} 1
$null=Invoke-Clean @{Action='Register';TargetDirectory=$external} 1
$registered=Invoke-Clean @{Action='Register';TargetDirectory=$external;AdoptExisting=$true}
if($registered.status -ne 'registered'){throw 'Explicit cache registration failed.'}
$marker=Join-Path $external '.audiolib-build-owner.json'
$markerAttributes=[IO.File]::GetAttributes($marker)
try{
    # Dotfiles are hidden on Unix; Windows requires the real Hidden attribute.
    if($IsWindows){[IO.File]::SetAttributes($marker,($markerAttributes -bor [IO.FileAttributes]::Hidden))}
    if(!((Get-Item -LiteralPath $marker -Force).Attributes -band [IO.FileAttributes]::Hidden)){throw 'Ownership regression requires a genuinely hidden marker.'}
    $hiddenPreview=Invoke-Clean @{Action='Plan';TargetDirectory=$external}
    if($hiddenPreview.status -ne 'planned' -or $hiddenPreview.ownership -ne 'registered' -or $hiddenPreview.removed_bytes -ne 0){throw 'Hidden cache ownership was not recognized safely during preview.'}
    $hiddenRun=Invoke-Clean @{TargetDirectory=$external}
    if($hiddenRun.status -ne 'passed' -or $hiddenRun.ownership -ne 'registered' -or $hiddenRun.removed_bytes -le 0 -or !(Test-Path -LiteralPath $marker) -or !(Test-Path -LiteralPath (Join-Path $external 'x86_64-pc-windows-msvc/release/app.exe'))){throw 'Hidden-marker cleanup lost ownership or exceeded the debug boundary.'}
}finally{
    if($IsWindows -and (Test-Path -LiteralPath $marker)){[IO.File]::SetAttributes($marker,$markerAttributes)}
}
@{schema_version=1;owner_id='foreign';host=[Environment]::MachineName}|ConvertTo-Json|Set-Content -LiteralPath $marker -Encoding utf8
$null=Invoke-Clean @{TargetDirectory=$external;Mode='All'} 1
if(!(Test-Path -LiteralPath (Join-Path $external 'x86_64-pc-windows-msvc/release/app.exe'))){throw 'Foreign-owned cache was deleted.'}
Write-Host 'PASS: external caches require explicit adoption; hidden owner markers support preview/cleanup; foreign ownership refused.'
$realCompilers=@(Microsoft.PowerShell.Management\Get-Process | Where-Object ProcessName -match '^(cargo|rustc|rustdoc|clippy-driver|clang|clang\+\+|gcc|g\+\+|cc1|cc1plus|link|lld|lld-link|cl)$')
if($realCompilers.Count){Write-Host 'Pipeline cleanup integration: not-run (another real compiler is active).'}else{
    $tools=Join-Path $taskRoot 'tools'
    New-Item -ItemType Directory -Path $tools -Force | Out-Null
    foreach($name in @('clean-build-artifacts.ps1','build-cache.ps1')){Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $tools}
    [IO.File]::WriteAllText((Join-Path $taskRoot '.gitignore'),"target/`ndist/`n.rust-validation/`n*.log`n")
    [IO.File]::WriteAllText((Join-Path $tools 'deliver.ps1'),@'
if(Test-Path -LiteralPath .rust-validation/fail-delivery){throw 'Expected delivery failure.'}
New-Item -ItemType Directory -Path dist -Force | Out-Null
[IO.File]::WriteAllText((Join-Path (Get-Location) 'dist/accepted.txt'),'accepted payload')
'@)
    $plan=@{schema_version=1;name='cleanup-integration';host='any';stages=@(@{id='deliver';reuse='never';script='tools/deliver.ps1';outputs=@('dist/accepted.txt')})}
    $planPath=Join-Path $taskRoot 'pipeline.json'
    $plan|ConvertTo-Json -Depth 6|Set-Content -LiteralPath $planPath -Encoding utf8
    & git -C $taskRoot -c user.name=CacheTest -c user.email=cache@example.invalid add .
    & git -C $taskRoot -c user.name=CacheTest -c user.email=cache@example.invalid commit -qm fixture
    if($LASTEXITCODE -ne 0){throw 'Could not commit pipeline fixture.'}
    $savedTarget=$env:CARGO_TARGET_DIR
    try{
        $env:CARGO_TARGET_DIR=$taskTarget
        New-Cache $taskTarget
        $runner=Join-Path $PSScriptRoot 'release-pipeline.ps1'
        $run=Join-Path $taskRoot '.rust-validation/release-pipeline/cleanup'
        $pwsh=Join-Path $PSHOME ('pwsh'+$(if($IsWindows){'.exe'}else{''}))
        & $pwsh -NoProfile -File $runner -Root $taskRoot -PlanFile $planPath -RunDirectory $run -Tag v0.0.1 -Action Run -CleanBuildArtifacts -BuildCacheLimitGiB 0 *> (Join-Path $taskRoot 'pipeline-success.log')
        if($LASTEXITCODE -ne 0){throw 'Pipeline post-delivery cleanup failed; inspect owned pipeline-success.log.'}
        if((Test-Path -LiteralPath (Join-Path $taskTarget 'debug')) -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'x86_64-pc-windows-msvc/release/app.exe'))){throw 'Pipeline cleanup removed wrong outputs.'}
        New-Cache $taskTarget
        [IO.File]::WriteAllText((Join-Path $taskRoot '.rust-validation/fail-delivery'),'fail')
        & $pwsh -NoProfile -File $runner -Root $taskRoot -PlanFile $planPath -RunDirectory $run -Tag v0.0.1 -Action Run -CleanBuildArtifacts -BuildCacheLimitGiB 0 *> (Join-Path $taskRoot 'pipeline-failure.log')
        if($LASTEXITCODE -eq 0 -or !(Test-Path -LiteralPath (Join-Path $taskTarget 'debug/deps/test.exe'))){throw 'Failed delivery triggered cleanup or was accepted.'}
        $probe=Enter-AudioLibBuildLease $taskRoot -Exclusive
        $probe.Dispose()
    }finally{$env:CARGO_TARGET_DIR=$savedTarget}
    Write-Host 'PASS: pipeline releases its build lease before cleanup; failed delivery retains caches/evidence and releases leases.'
}
Write-Host "Build artifact cleanup regressions passed. Owned evidence: $taskRoot"
