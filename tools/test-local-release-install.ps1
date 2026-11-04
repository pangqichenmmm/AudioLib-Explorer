# tools/test-local-release-install.ps1
# Test the installer in an owned fixture checkout, optionally reusing an already
# accepted runtime payload. Never install into this checkout's real local_release.
#Requires -Version 7.3
param([string]$RuntimePayload)
$ErrorActionPreference='Stop'
if (!$IsWindows) { throw 'Windows installation regression requires Windows.' }
$taskRoot=Join-Path ([IO.Path]::GetTempPath()) ('AudioLibInstallRegression-'+[guid]::NewGuid().ToString('N'))
$taskTools=Join-Path $taskRoot 'tools'
$taskSource=Join-Path $taskRoot 'payload'
$taskInstalled=Join-Path $taskRoot 'local_release'
New-Item -ItemType Directory -Path $taskTools,$taskSource,$taskInstalled | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install-local-release.ps1') -Destination $taskTools
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'native-desktop-window.ps1') -Destination $taskTools
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'assert-portable-layout.ps1') -Destination $taskTools
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'portable-files.json') -Destination $taskTools
Copy-Item -LiteralPath (Join-Path (Split-Path -Parent $PSScriptRoot) 'Cargo.toml') -Destination $taskRoot
$taskFixtureVersion=[regex]::Match([IO.File]::ReadAllText((Join-Path $taskRoot 'Cargo.toml')),'(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"').Groups[1].Value
$taskSentinel=Join-Path $taskInstalled 'user-file.keep'
[IO.File]::WriteAllText($taskSentinel,'unowned file must survive')
$taskPwsh=Join-Path $PSHOME 'pwsh.exe'
$taskInstaller=Join-Path $taskTools 'install-local-release.ps1'
& $taskPwsh -NoProfile -File $taskInstaller -Source $taskSource *> (Join-Path $taskRoot 'incomplete.log')
if ($LASTEXITCODE -eq 0 -or [IO.File]::ReadAllText($taskSentinel) -cne 'unowned file must survive') { throw 'Incomplete source was accepted or installation data was touched.' }
Write-Host 'PASS: incomplete package rejected before installation; unrelated file preserved.'
$taskRequired=@('AudioLibExplorer.exe','AudioLibCli.exe','Start-AudioLibExplorer.cmd','update-channel.txt','update-version.txt') + @(Get-Content -LiteralPath (Join-Path $taskTools 'portable-files.json') -Raw | ConvertFrom-Json)
foreach($taskName in $taskRequired){
    $taskPath=Join-Path $taskSource $taskName
    New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($taskPath)) -Force | Out-Null
    [IO.File]::WriteAllText($taskPath,'fixture')
}
[IO.File]::WriteAllText((Join-Path $taskSource 'update-channel.txt'),'stable')
[IO.File]::WriteAllText((Join-Path $taskSource 'update-version.txt'),$taskFixtureVersion)
$taskOldExe=Join-Path $taskInstalled 'AudioLibExplorer.exe'
[IO.File]::WriteAllText($taskOldExe,'original installed executable')
function Assert-Rejected([string]$Name,[string]$Candidate=$taskSource){
    & $taskPwsh -NoProfile -File $taskInstaller -Source $Candidate *> (Join-Path $taskRoot ($Name+'.log'))
    if($LASTEXITCODE -eq 0 -or [IO.File]::ReadAllText($taskOldExe) -cne 'original installed executable'){throw "Preflight $Name accepted or copied files before rejection."}
}
[IO.File]::WriteAllText((Join-Path $taskSource 'settings.json'),'must not replace user state')
Assert-Rejected 'unexpected-state'
Remove-Item -LiteralPath (Join-Path $taskSource 'settings.json')
$taskLink=Join-Path $taskInstalled 'config'
$taskOutside=Join-Path $taskRoot 'outside-install'
New-Item -ItemType Directory -Path $taskOutside | Out-Null
[IO.File]::WriteAllText((Join-Path $taskOutside 'keep.txt'),'outside untouched')
New-Item -ItemType Junction -Path $taskLink -Target $taskOutside | Out-Null
Assert-Rejected 'late-linked-directory'
if([IO.File]::ReadAllText((Join-Path $taskOutside 'keep.txt')) -cne 'outside untouched'){throw 'Linked destination was modified.'}
if(![IO.Path]::GetFullPath($taskLink).StartsWith($taskRoot+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Fixture link cleanup escaped owned root.'}
[IO.Directory]::Delete($taskLink)
New-Item -ItemType Directory -Path (Join-Path $taskRoot '.rust-validation') -Force | Out-Null
$taskInstallLock=[IO.File]::Open((Join-Path $taskRoot '.rust-validation/local-release-install.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
try{Assert-Rejected 'concurrent-install'}finally{$taskInstallLock.Dispose()}
Assert-Rejected 'overlap' (Join-Path $taskInstalled 'payload')
[IO.File]::WriteAllText((Join-Path $taskSource 'update-version.txt'),$(if($taskFixtureVersion -eq '0.0.0'){'0.0.1'}else{'0.0.0'}))
Assert-Rejected 'wrong-version'
[IO.File]::WriteAllText((Join-Path $taskSource 'update-version.txt'),$taskFixtureVersion)
Write-Host 'PASS: allowlist, version, every destination, overlap and installation lock checked before copying.'
if (!$RuntimePayload) { Write-Host 'Runtime startup: not-run (pass -RuntimePayload with an already accepted portable payload).'; exit 0 }
$RuntimePayload=[IO.Path]::GetFullPath($RuntimePayload)
$taskOriginalData=$env:AUDIOLIB_DATA_DIR
$taskPrivateData=Join-Path $taskRoot 'normal-user-data'
New-Item -ItemType Directory -Path $taskPrivateData | Out-Null
[IO.File]::WriteAllText((Join-Path $taskPrivateData 'settings.json'),'normal user data must survive')
$env:AUDIOLIB_DATA_DIR=$taskPrivateData
try {
    # Reuse the accepted payload in the owned fixture; never touch the real install.
    foreach ($taskFile in Get-ChildItem -LiteralPath $RuntimePayload -Recurse -File) {
        $taskRelative=[IO.Path]::GetRelativePath($RuntimePayload,$taskFile.FullName)
        $taskTarget=Join-Path $taskSource $taskRelative
        New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($taskTarget)) -Force | Out-Null
        Copy-Item -LiteralPath $taskFile.FullName -Destination $taskTarget
    }
    foreach ($taskAttempt in 1..2) {
        $taskLog=Join-Path $taskRoot ('attempt-'+$taskAttempt+'.log')
        & $taskPwsh -NoProfile -File $taskInstaller -Source $taskSource *> $taskLog
        if ($LASTEXITCODE -ne 0) { throw "Owned installation fixture failed; inspect $taskLog" }
        $taskResult=Get-Content -LiteralPath $taskLog -Raw | ConvertFrom-Json
        if (!$taskResult.success -or $taskResult.startup -cne 'passed' -or ($taskAttempt -eq 2 -and $taskResult.copied_files -ne 0)) { throw 'Installation identity/startup/idempotence failed.' }
    }
    if ([IO.File]::ReadAllText($taskSentinel) -cne 'unowned file must survive' -or [IO.File]::ReadAllText((Join-Path $taskPrivateData 'settings.json')) -cne 'normal user data must survive') { throw 'User data was modified.' }
    Write-Host 'PASS: owned runtime fixture identity and startup; identical files not recopied; user files/preferences preserved.'
} finally { $env:AUDIOLIB_DATA_DIR=$taskOriginalData }
Write-Host "Local installer regression passed. Owned evidence: $taskRoot"
