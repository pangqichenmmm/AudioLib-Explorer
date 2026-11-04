# tools/test-release-pipeline.ps1
# Synthetic end-to-end runner tests: real isolated Git, typed child PowerShell,
# artifact tampering, failure/resume, environment invalidation and compact output.
#Requires -Version 7.3
param([ValidateSet('All','Profiles')][string]$Scope='All')
$ErrorActionPreference='Stop'
$taskRepository=Split-Path -Parent $PSScriptRoot
$taskRoot=Join-Path $taskRepository ('.rust-validation/release-pipeline-tests/' + [guid]::NewGuid().ToString('N') + '/repo with spaces')
New-Item -ItemType Directory -Path $taskRoot -Force | Out-Null
$taskSavedGitCount=$env:GIT_CONFIG_COUNT
$taskGitIndex=if($env:GIT_CONFIG_COUNT){[int]$env:GIT_CONFIG_COUNT}else{0}
[Environment]::SetEnvironmentVariable(('GIT_CONFIG_KEY_'+$taskGitIndex),'safe.directory')
[Environment]::SetEnvironmentVariable(('GIT_CONFIG_VALUE_'+$taskGitIndex),$taskRoot.Replace('\','/'))
$env:GIT_CONFIG_COUNT=($taskGitIndex+1).ToString()
function Invoke-FixtureGit([string[]]$Arguments) {
    & git -C $taskRoot @Arguments *> $null
    if ($LASTEXITCODE -ne 0) { throw "Fixture Git failed: $($Arguments[0])" }
}
Invoke-FixtureGit @('init','-q')
Invoke-FixtureGit @('config','user.name','Pipeline Test')
Invoke-FixtureGit @('config','user.email','pipeline@example.invalid')
[IO.File]::WriteAllText((Join-Path $taskRoot '.gitignore'), "target/`ndist/`nlocal_release/`n.rust-validation/`n")
[IO.File]::WriteAllText((Join-Path $taskRoot 'source.txt'), 'source 1')
$taskWorker=@'
param([string]$Name,[bool]$Selected,[string[]]$Packages)
$ErrorActionPreference='Stop'
if ($Selected -or ($Packages -join ',') -cne 'one,two') { throw 'Typed parameters were lost.' }
New-Item -ItemType Directory -Path target -Force | Out-Null
Add-Content -LiteralPath target/calls.txt -Value $Name
if (Test-Path -LiteralPath ('target/fail-' + $Name)) { throw 'Expected synthetic failure.' }
[IO.File]::WriteAllText((Join-Path (Get-Location) ('target/' + $Name + '.bin')), $Name)
1..1000 | ForEach-Object { "Verbose fixture log $_" }
'@
[IO.File]::WriteAllText((Join-Path $taskRoot 'worker.ps1'),$taskWorker)
$taskPlan=@{schema_version=1;name='fixture';host='any';variables=@{Selected=$false;Packages=@('one','two')};environment_variables=@('AUDIOLIB_PIPELINE_TEST');stages=@(
    @{id='checks';script='worker.ps1';parameters=@{Name='checks';Selected='${Selected}';Packages='${Packages}'};reuse='inputs';input_paths=@('source.txt','worker.ps1');outputs=@('target/checks.bin')},
    @{id='payload';script='worker.ps1';parameters=@{Name='payload';Selected='${Selected}';Packages='${Packages}'};needs=@('checks');outputs=@('target/payload.bin')}
)}
$taskPlanPath=Join-Path $taskRoot 'plan.json'
$taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
Invoke-FixtureGit @('add','.')
Invoke-FixtureGit @('commit','-qm','fixture')
$taskRun=Join-Path $taskRoot '.rust-validation/release-pipeline/test'
$taskRunner=Join-Path $PSScriptRoot 'release-pipeline.ps1'
$taskPwsh=Join-Path $PSHOME ('pwsh'+$(if($IsWindows){'.exe'}else{''}))
function Invoke-Pipeline([string]$Action='Run',[int]$ExpectedExit=0,[string[]]$ExtraArguments=@()) {
    $taskOutput=& $taskPwsh -NoProfile -File $taskRunner -Root $taskRoot -PlanFile $taskPlanPath -RunDirectory $taskRun -Tag v0.0.1 -Action $Action @ExtraArguments 2>&1
    if($LASTEXITCODE -ne $ExpectedExit){throw "Unexpected pipeline exit: $($taskOutput -join ' ')"}
    if(@($taskOutput).Count -gt (($taskPlan.stages.Count+2)*2+1)){throw ('Runner leaked stage output: ' + (($taskOutput | Select-Object -First 12) -join ' '))}
    return $taskOutput
}
function Read-State {Get-Content -LiteralPath (Join-Path $taskRun 'state.json') -Raw | ConvertFrom-Json -AsHashtable}
function Count-Calls {@(Get-Content -LiteralPath (Join-Path $taskRoot 'target/calls.txt')).Count}
$taskSaved=[Environment]::GetEnvironmentVariable('AUDIOLIB_PIPELINE_TEST')
$taskSavedGhToken=$env:GH_TOKEN
$taskSavedNativeFlags=[Environment]::GetEnvironmentVariable('CXXFLAGS_x86_64_pc_windows_msvc')
$taskSavedSlint=$env:SLINT_EMIT_DEBUG_INFO
$taskSavedTarget=$env:CARGO_TARGET_DIR
try {
    # Real profile output declarations must stay inside this owned fixture,
    # even when the caller uses an external build cache for actual releases.
    $env:CARGO_TARGET_DIR=Join-Path $taskRoot 'target'
    if($Scope -eq 'All') {
    $null=Invoke-Pipeline 'Plan'
    if(Test-Path -LiteralPath $taskRun){throw 'Plan created run state.'}
    $null=Invoke-Pipeline
    if((Count-Calls) -ne 2 -or (Read-State).status -ne 'passed'){throw 'Initial run failed.'}
    $null=Invoke-Pipeline
    if((Count-Calls) -ne 2 -or (Read-State).stages.payload.status -ne 'reused'){throw 'Unchanged run was repeated.'}
    Write-Host 'PASS: typed child commands, private verbose logs and unchanged reuse.'
    [IO.File]::WriteAllText((Join-Path $taskRoot 'README.md'),'docs only')
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','docs')
    $null=Invoke-Pipeline
    if((Count-Calls) -ne 3 -or (Read-State).stages.checks.status -ne 'reused'){throw 'Documentation invalidated source checks or failed to rebuild exact-commit payload.'}
    [IO.File]::WriteAllText((Join-Path $taskRoot 'target/payload.bin'),'tampered')
    $null=Invoke-Pipeline
    if((Count-Calls) -ne 4){throw 'Tampered artifact was accepted.'}
    Write-Host 'PASS: scoped source evidence survives docs; final payload and tampered artifacts are revalidated.'
    [IO.File]::WriteAllText((Join-Path $taskRoot 'source.txt'),'source 2')
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','source')
    [IO.File]::WriteAllText((Join-Path $taskRoot 'target/fail-checks'),'fail')
    $null=Invoke-Pipeline 'Run' 1
    if((Read-State).stages.Contains('payload')){throw 'Pipeline continued past failure.'}
    $taskFailedDirectory=(Read-State).stages.checks.log_directory
    Remove-Item -LiteralPath (Join-Path $taskRoot 'target/fail-checks')
    $null=Invoke-Pipeline
    if((Read-State).status -ne 'passed'){throw 'Failure did not resume.'}
    $taskOldFailure=Get-Content -LiteralPath (Join-Path $taskFailedDirectory 'receipt.json') -Raw | ConvertFrom-Json
    if($taskOldFailure.status -ne 'failed' -or !(Select-String -LiteralPath (Join-Path $taskFailedDirectory 'stderr.log') -Pattern 'Expected synthetic failure' -Quiet)){throw 'Retry erased original failure evidence.'}
    $taskCount=Count-Calls
    $env:AUDIOLIB_PIPELINE_TEST='environment changed'
    $null=Invoke-Pipeline
    if((Count-Calls) -ne ($taskCount+2)){throw 'Environment change reused stale evidence.'}
    Write-Host 'PASS: failure stops dependents; retry and environment invalidation work.'
    [IO.File]::WriteAllText((Join-Path $taskRoot 'source.txt'),'dirty')
    $taskCount=Count-Calls
    $null=Invoke-Pipeline 'Run' 1
    $null=Invoke-Pipeline 'Status'
    if((Count-Calls) -ne $taskCount){throw 'Dirty run or Status executed work.'}
    Write-Host 'PASS: dirty sources rejected; Status is a bounded snapshot without execution.'
    [IO.File]::WriteAllText((Join-Path $taskRoot 'source.txt'),'source 2')
    $taskCount=Count-Calls
    $taskLock=[IO.File]::Open((Join-Path $taskRun '.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
    try {$null=Invoke-Pipeline 'Run' 1} finally {$taskLock.Dispose()}
    if((Count-Calls) -ne $taskCount -or (Read-State).status -ne 'passed'){throw 'Concurrent run changed accepted state.'}
    $taskLock=[IO.File]::Open((Join-Path $taskRun '.child.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
    try {$null=Invoke-Pipeline 'Run' 1} finally {$taskLock.Dispose()}
    if((Count-Calls) -ne $taskCount -or (Read-State).status -ne 'passed'){throw 'Retry overlapped a surviving child or changed accepted state.'}
    $env:GH_TOKEN='synthetic-token-never-used-for-network'
    $taskPlan.stages=@(@{id='native';command='git';github_auth=$true;arguments=@('rev-parse','--show-toplevel');input_files=@('target/checks.bin')})
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','native plan')
    $null=Invoke-Pipeline
    $null=Invoke-Pipeline
    if((Read-State).stages.native.status -ne 'reused'){throw 'Native command receipt not reused.'}
    [IO.File]::WriteAllText((Join-Path $taskRoot 'target/checks.bin'),'changed external input')
    $null=Invoke-Pipeline
    if((Read-State).stages.native.status -ne 'passed'){throw 'External input mutation was missed.'}
    $taskPlan.stages[0].arguments=@('not-a-real-git-command')
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','native failure')
    $null=Invoke-Pipeline 'Run' 1
    if((Read-State).status -ne 'failed'){throw 'Native failure was accepted.'}
    foreach($taskPrivateFile in Get-ChildItem -LiteralPath $taskRun -Recurse -File | Where-Object Extension -eq '.json') {
        if(Select-String -LiteralPath $taskPrivateFile.FullName -SimpleMatch 'synthetic-token-never-used-for-network' -Quiet){throw 'Credential leaked into pipeline evidence.'}
    }
    Write-Host 'PASS: concurrent execution excluded; native exit codes and external artifact inputs verified.'
    $taskLogDirectory=(Read-State).stages.native.log_directory
    [IO.File]::WriteAllText((Join-Path $taskLogDirectory 'stdout.log'),('a'*20000))
    [IO.File]::WriteAllText((Join-Path $taskLogDirectory 'stderr.log'),('b'*20000))
    $taskLogOutput=Invoke-Pipeline 'Logs' 0 @('-Stage','native','-MaxCharacters','1024')
    if(($taskLogOutput -join "`n").Length -gt 1024){throw 'A long log line exceeded the character budget.'}
    $taskParameters=@{Selected='${Selected}';Packages='${Packages}'}
    $taskPlan.stages=@(
        @{id='gate';script='worker.ps1';parameters=($taskParameters+@{Name='gate'});reuse='never'},
        @{id='payload';script='worker.ps1';parameters=($taskParameters+@{Name='payload'});outputs=@('target/payload.bin')}
    )
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','transient gate fixture')
    $null=Invoke-Pipeline
    [IO.File]::WriteAllText((Join-Path $taskRoot 'target/fail-gate'),'fail')
    $null=Invoke-Pipeline 'Run' 1
    if(!(Read-State).cache.Contains('payload')){throw 'An early failure erased a valid payload receipt.'}
    Remove-Item -LiteralPath (Join-Path $taskRoot 'target/fail-gate')
    $taskCount=Count-Calls
    $null=Invoke-Pipeline
    if((Count-Calls) -ne ($taskCount+1) -or (Read-State).stages.payload.status -ne 'reused'){throw 'Retry repeated a still-valid payload.'}
    $taskCount=Count-Calls
    [Environment]::SetEnvironmentVariable('CXXFLAGS_x86_64_pc_windows_msvc','synthetic compiler configuration')
    $null=Invoke-Pipeline
    if((Count-Calls) -ne ($taskCount+2)){throw 'Target-specific compiler flags did not invalidate the build.'}
    Write-Host 'PASS: long log lines bounded; failed attempts retain unrelated receipts; native compiler changes invalidate reuse.'
    $taskMutator=@'
if(Test-Path -LiteralPath target/move-head){
    Remove-Item -LiteralPath target/move-head
    [IO.File]::WriteAllText((Join-Path (Get-Location) 'README.md'),'concurrent checkout change')
    & git add -- README.md *> $null
    & git commit -qm 'simulated concurrent source change' *> $null
    if($LASTEXITCODE -ne 0){throw 'Fixture mutation failed.'}
}
'fixture-tool 1'
'@
    $taskMutatorPath=Join-Path $taskRoot 'mutator.ps1'
    [IO.File]::WriteAllText($taskMutatorPath,$taskMutator)
    $taskPlan.environment_tools=@($taskMutatorPath)
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','source freshness fixture')
    $null=Invoke-Pipeline
    $taskCount=Count-Calls
    [IO.File]::WriteAllText((Join-Path $taskRoot 'target/move-head'),'change')
    $null=Invoke-Pipeline 'Run' 1
    if((Count-Calls) -ne $taskCount -or (Read-State).status -ne 'failed'){throw 'A changed checkout was accepted using old receipts.'}
    Write-Host 'PASS: source identity rechecked before cached execution; no false passing result after checkout changes.'
    [IO.File]::WriteAllText((Join-Path $taskRoot 'tamper.ps1'),"[IO.File]::WriteAllText((Join-Path (Get-Location) 'target/payload.bin'),'unintended package mutation')")
    $taskPlan.stages=@(
        @{id='build';script='worker.ps1';parameters=($taskParameters+@{Name='payload'});outputs=@('target/payload.bin')},
        @{id='accept';script='tamper.ps1';needs=@('build')},
        @{id='install';script='worker.ps1';parameters=($taskParameters+@{Name='install'});needs=@('accept','build')}
    )
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','upstream mutation fixture')
    $null=Invoke-Pipeline 'Run' 1
    if((Read-State).stages.accept.status -ne 'failed' -or (Read-State).stages.Contains('install') -or (Read-State).cache.Contains('accept')){throw 'Changed accepted dependency reached installation or retained acceptance.'}
    [IO.File]::WriteAllText((Join-Path $taskRoot 'exit.ps1'),'exit 17')
    $taskPlan.stages=@(@{id='exit';script='exit.ps1'})
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','script exit fixture')
    $null=Invoke-Pipeline 'Run' 1
    if((Read-State).status -ne 'failed'){throw 'Nonzero PowerShell script exit was accepted.'}
    Write-Host 'PASS: accepted dependencies cannot be mutated; failed acceptance stops installation; PowerShell exit codes propagate.'
    }
    New-Item -ItemType Directory -Path (Join-Path $taskRoot 'target') -Force | Out-Null
    $taskStub=@'
param($Tag,[switch]$WithoutAsio,[switch]$SkipBuild,$BinaryDirectory,$Source,[string[]]$Package,[switch]$Asio)
$ErrorActionPreference='Stop'
$fixtureRoot=[IO.Path]::GetFullPath((Get-Location).Path)
$fixtureTarget=Join-Path $fixtureRoot 'target'
if (!$env:CARGO_TARGET_DIR -or [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR) -cne $fixtureTarget) { throw 'Fake tools require the owned fixture target directory.' }
function Write-FixtureFile([string]$Relative,[string]$Contents) {
    $path=[IO.Path]::GetFullPath($Relative,$fixtureRoot)
    if (!$path.StartsWith($fixtureRoot+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) { throw 'Fake output escaped the fixture.' }
    New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($path)) -Force | Out-Null
    [IO.File]::WriteAllText($path,$Contents)
}
$scriptName=[IO.Path]::GetFileName($MyInvocation.MyCommand.Path)
@{script=$scriptName;without_asio=[bool]$WithoutAsio;asio=[bool]$Asio;packages=@($Package)} | ConvertTo-Json -Compress | Add-Content -LiteralPath (Join-Path $fixtureTarget 'profile-calls.jsonl')
# Model what each real tool produces, independently of the plan's declarations.
switch -Exact ($scriptName) {
    'build-release.ps1' {
        Write-FixtureFile 'target/x86_64-pc-windows-msvc/release/AudioLibExplorer.exe' "desktop without_asio=$WithoutAsio"
        Write-FixtureFile 'target/x86_64-pc-windows-msvc/release/AudioLibCli.exe' "cli without_asio=$WithoutAsio"
    }
    'package-portable.ps1' {
        if (!$SkipBuild) { throw 'Profile packaging must reuse its build.' }
        Write-FixtureFile 'dist/latest/portable/AudioLibExplorer.exe' ([IO.File]::ReadAllText((Join-Path $fixtureTarget 'x86_64-pc-windows-msvc/release/AudioLibExplorer.exe')))
        Write-FixtureFile 'dist/latest/portable/AudioLibCli.exe' ([IO.File]::ReadAllText((Join-Path $fixtureTarget 'x86_64-pc-windows-msvc/release/AudioLibCli.exe')))
        Write-FixtureFile 'dist/latest/AudioLibExplorer-latest-win-x64-portable.zip' "portable tag=$Tag without_asio=$WithoutAsio"
        $hash=(Get-FileHash -LiteralPath (Join-Path $fixtureRoot 'dist/latest/AudioLibExplorer-latest-win-x64-portable.zip') -Algorithm SHA256).Hash
        Write-FixtureFile 'dist/latest/AudioLibExplorer-latest-win-x64-portable.zip.sha256' ($hash+'  AudioLibExplorer-latest-win-x64-portable.zip')
    }
    'validate-portable.ps1' {
        if ($BinaryDirectory -cne 'dist/latest/portable') { throw 'Profile acceptance selected another payload.' }
    }
    'install-local-release.ps1' {
        if ($Source -cne 'dist/latest/portable') { throw 'Profile installation selected another payload.' }
        Write-FixtureFile 'local_release/installed.txt' ([IO.File]::ReadAllText((Join-Path $fixtureRoot 'dist/latest/portable/AudioLibExplorer.exe')))
    }
}
'@
    $taskPlan=Get-Content -LiteralPath (Join-Path $PSScriptRoot 'release-plans/windows-client.json') -Raw | ConvertFrom-Json -AsHashtable
    $taskPlan.require_tag=$false; $taskPlan.host='any'; $taskPlan.environment_tools=@()
    New-Item -ItemType Directory -Path (Join-Path $taskRoot 'tools') -Force | Out-Null
    foreach($taskStep in $taskPlan.stages){[IO.File]::WriteAllText((Join-Path $taskRoot $taskStep.script),$taskStub)}
    [IO.File]::WriteAllText((Join-Path $taskRoot 'tools/check-local.ps1'),$taskStub)
    $taskChecksPath=Join-Path $PSScriptRoot 'release-plans/checks.example.json'
    $taskPackageStep=$taskPlan.stages | Where-Object id -eq 'package'
    $taskPackageOutputs=@($taskPackageStep.outputs)
    $taskPackageStep.outputs=@('dist/latest/portable','dist/latest/AudioLibExplorer-${Tag}-win-x64-portable.zip','dist/latest/AudioLibExplorer-${Tag}-win-x64-portable.zip.sha256')
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','incorrect versioned package output fixture')
    $taskProfileRun=$taskRun
    $taskRun=Join-Path $taskRoot '.rust-validation/release-pipeline/incorrect-package-output'
    $taskFailedOutput=Invoke-Pipeline 'Run' 1 @('-ChecksFile',$taskChecksPath)
    $taskFailedState=Read-State
    $taskCalls=Get-Content -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl') | ForEach-Object{$_|ConvertFrom-Json}
    if($taskFailedState.stages.package.status -ne 'failed' -or $taskFailedState.stages.Contains('accept') -or $taskFailedState.stages.Contains('install') -or
        ($taskCalls.script -join ',') -cne 'check-release.ps1,check-local.ps1,build-release.ps1,package-portable.ps1' -or
        ($taskFailedOutput -join ' ') -notlike '*Missing stage artifact: dist/latest/AudioLibExplorer-v0.0.1-win-x64-portable.zip*') { throw 'A versioned declaration for the latest-only packager was not rejected before acceptance.' }
    Write-Host 'PASS: actual latest-only package outputs reject an incorrect versioned artifact declaration before acceptance or installation.'
    Remove-Item -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl')
    $taskRun=$taskProfileRun
    $taskPackageStep.outputs=$taskPackageOutputs
    $taskPlan | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $taskPlanPath -Encoding utf8
    Invoke-FixtureGit @('add','.'); Invoke-FixtureGit @('commit','-qm','real profile routing with declared artifacts and fake tools')
    $null=Invoke-Pipeline 'Run' 0 @('-ChecksFile',$taskChecksPath)
    $taskCalls=Get-Content -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl') | ForEach-Object{$_|ConvertFrom-Json}
    if(($taskCalls.script -join ',') -cne 'check-release.ps1,check-local.ps1,build-release.ps1,package-portable.ps1,validate-portable.ps1,install-local-release.ps1'){throw 'Actual profile stages ran out of order or were omitted.'}
    if(@($taskCalls | Where-Object script -eq 'check-local.ps1').Count -ne 1 -or !($taskCalls | Where-Object script -eq 'check-local.ps1').asio -or @($taskCalls | Where-Object { $_.script -in @('build-release.ps1','package-portable.ps1','validate-portable.ps1') -and $_.without_asio }).Count){throw 'Default Windows profile selected inconsistent features.'}
    Remove-Item -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl')
    $taskVariables=Join-Path $taskRoot 'target/without-asio.json'
    [IO.File]::WriteAllText($taskVariables,'{"WithoutAsio":true}')
    $null=Invoke-Pipeline 'Run' 0 @('-ChecksFile',$taskChecksPath,'-VariablesFile',$taskVariables)
    $taskCalls=Get-Content -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl') | ForEach-Object{$_|ConvertFrom-Json}
    if(($taskCalls.script -join ',') -cne 'check-local.ps1,build-release.ps1,package-portable.ps1,validate-portable.ps1,install-local-release.ps1'){throw 'Feature change repeated unrelated preflight or omitted affected stages.'}
    if(($taskCalls | Where-Object script -eq 'check-local.ps1').asio -or @($taskCalls | Where-Object { $_.script -in @('build-release.ps1','package-portable.ps1','validate-portable.ps1') -and $_.without_asio }).Count -ne 3){throw 'Non-ASIO override did not select one consistent check/build/package/accept configuration.'}
    Write-Host 'PASS: actual Windows plan and checks example route both ASIO selections consistently without Cargo or application execution.'
    Remove-Item -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl')
    $env:SLINT_EMIT_DEBUG_INFO=if($taskSavedSlint -eq '1'){'0'}else{'1'}
    $null=Invoke-Pipeline 'Run' 0 @('-ChecksFile',$taskChecksPath,'-VariablesFile',$taskVariables)
    $taskCalls=Get-Content -LiteralPath (Join-Path $taskRoot 'target/profile-calls.jsonl') | ForEach-Object{$_|ConvertFrom-Json}
    if(@($taskCalls).Count -ne 6 -or (Read-State).stages.build.status -ne 'passed'){throw 'Slint build configuration changed without invalidating receipts.'}
    Write-Host 'PASS: Slint build environment changes invalidate the selected configuration.'
} finally {
    [Environment]::SetEnvironmentVariable('AUDIOLIB_PIPELINE_TEST',$taskSaved)
    $env:GH_TOKEN=$taskSavedGhToken
    [Environment]::SetEnvironmentVariable('CXXFLAGS_x86_64_pc_windows_msvc',$taskSavedNativeFlags)
    $env:SLINT_EMIT_DEBUG_INFO=$taskSavedSlint
    $env:CARGO_TARGET_DIR=$taskSavedTarget
    $env:GIT_CONFIG_COUNT=$taskSavedGitCount
    [Environment]::SetEnvironmentVariable(('GIT_CONFIG_KEY_'+$taskGitIndex),$null)
    [Environment]::SetEnvironmentVariable(('GIT_CONFIG_VALUE_'+$taskGitIndex),$null)
}
Write-Host "Release pipeline synthetic regressions passed (scope=$Scope). Evidence: $taskRoot"
