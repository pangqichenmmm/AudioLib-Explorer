# tools/clean-build-artifacts.ps1
<#
.SYNOPSIS
Preview or prune owned Cargo build caches; never clean source or application data.
.DESCRIPTION
Default budget: 40 logical GiB, debug profiles only. MaxGiB=0 clears every
eligible candidate of the chosen mode. Cargo home, SDKs, packages, installed
applications and validation evidence are not candidates. No process is stopped.
External target directories need Register; nonempty adoption is explicit.
#>
#Requires -Version 7.3
[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateSet('Plan','Run','Register')][string]$Action='Plan',
    [ValidateSet('Incremental','Debug','All')][string]$Mode='Debug',
    [ValidateRange(0,100000)][double]$MaxGiB=40,
    [ValidateRange(0,3650)][int]$OlderThanDays=0,
    [string]$TargetDirectory,
    [switch]$AdoptExisting,
    [string]$Root=(Split-Path -Parent $PSScriptRoot)
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'build-cache.ps1')
$lease=$null
$report=$null
try {
    $Root=Get-AudioLibCanonicalPath $Root
    $manifest=Join-Path $Root 'Cargo.toml'
    if(!(Test-Path -LiteralPath $manifest -PathType Leaf) -or !([IO.File]::ReadAllText($manifest).Contains('crates/audiolib-core'))){throw 'Root must identify the AudioLib Cargo workspace, not an arbitrary directory.'}
    $explicitTarget=$PSBoundParameters.ContainsKey('TargetDirectory')
    if($Action -eq 'Register' -and !$explicitTarget){throw 'Register requires an explicitly named -TargetDirectory.'}
    if (!$TargetDirectory) { $TargetDirectory=if($env:CARGO_TARGET_DIR){$env:CARGO_TARGET_DIR}else{Join-Path $Root 'target'} }
    if (![IO.Path]::IsPathRooted($TargetDirectory)) { $TargetDirectory=Join-Path $Root $TargetDirectory }
    $TargetDirectory=Get-AudioLibCanonicalPath $TargetDirectory
    $comparison=if($IsWindows){[StringComparison]::OrdinalIgnoreCase}else{[StringComparison]::Ordinal}
    function Is-Within([string]$Path,[string]$Parent) { $Path.Equals($Parent,$comparison) -or $Path.StartsWith($Parent+[IO.Path]::DirectorySeparatorChar,$comparison) }
    if ((Is-Within $Root $TargetDirectory) -or $TargetDirectory -eq [IO.Path]::GetPathRoot($TargetDirectory)) { throw 'A workspace/volume root or its ancestor is not a build target.' }
    $protected=@('local_release','beta_release','dist','app','crates','server','tools','doc','docs','data','config','resources','test_audiolib','.git') | ForEach-Object {Get-AudioLibCanonicalPath (Join-Path $Root $_)}
    $protected+=@(Get-AudioLibBuildCommonDirectory $Root)
    foreach($path in @($env:AUDIOLIB_DATA_DIR,$env:AUDIOLIB_BETA_DATA_DIR,$env:CARGO_HOME,$env:RUSTUP_HOME,$env:CPAL_ASIO_DIR,$env:LIBCLANG_PATH)) { if($path){$protected+=Get-AudioLibCanonicalPath $path} }
    foreach($path in @((Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo'),(Join-Path ([Environment]::GetFolderPath('UserProfile')) '.rustup'))) { $protected+=Get-AudioLibCanonicalPath $path }
    if($env:LOCALAPPDATA){$protected+=Get-AudioLibCanonicalPath (Join-Path $env:LOCALAPPDATA 'AudioLibExplorer')}
    foreach($path in $protected){if((Is-Within $TargetDirectory $path) -or (Is-Within $path $TargetDirectory)){throw 'Build target overlaps a protected source, SDK, toolchain, installation or data directory.'}}
    Assert-AudioLibUnlinkedPath $TargetDirectory
    $defaultTarget=Get-AudioLibCanonicalPath (Join-Path $Root 'target')
    $common=Get-AudioLibBuildCommonDirectory $Root
    $identity=if($IsWindows){$common.ToLowerInvariant()}else{$common}
    $ownerId=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($identity)))
    $ownerPath=Join-Path $TargetDirectory '.audiolib-build-owner.json'
    $report=[ordered]@{schema_version=1;action=$Action;status='planned';mode=$Mode;target=$TargetDirectory;max_gib=$MaxGiB;before_bytes=0L;removed_bytes=0L;remaining_bytes=0L;budget_met=$null;candidates=@();preserved='release outputs unless All; target root/markers; SDKs; Cargo/Rustup home; dist; installations; source/audio/data; validation evidence'}
    if (!(Test-Path -LiteralPath $TargetDirectory)) {
        if($Action -ne 'Register'){$report.status='absent';$report|ConvertTo-Json -Compress -Depth 5;exit 0}
        $null=[IO.Directory]::CreateDirectory($TargetDirectory)
    }
    $entries=@(Get-ChildItem -LiteralPath $TargetDirectory -Force)
    foreach($entry in $entries){
        if($entry.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Linked build target entry.'}
        $known=if($entry.PSIsContainer){$entry.Name -in @('debug','release','doc','tmp') -or $entry.Name -match '^[A-Za-z0-9_]+-[A-Za-z0-9_-]+$'}else{$entry.Name -in @('CACHEDIR.TAG','.rustc_info.json','.audiolib-build-owner.json','.cargo-lock','.DS_Store') -or $entry.Name -match '^\.audiolib-.*\.lock$'}
        if(!$known){throw "Unrecognized build target entry: $($entry.Name)"}
    }
    if((Test-Path -LiteralPath $ownerPath) -and (Get-Item -LiteralPath $ownerPath -Force).Length -gt 16384){throw 'Invalid oversized cache ownership marker.'}
    $owner=if(Test-Path -LiteralPath $ownerPath){Get-Content -LiteralPath $ownerPath -Raw|ConvertFrom-Json}else{$null}
    if($owner -and ($owner.schema_version -ne 1 -or $owner.owner_id -cne $ownerId -or $owner.host -cne [Environment]::MachineName)){throw 'Build cache belongs to a different workspace or host.'}
    $cargoTag=Join-Path $TargetDirectory 'CACHEDIR.TAG'
    $hasCargoTag=(Test-Path -LiteralPath $cargoTag -PathType Leaf) -and (Get-Item -LiteralPath $cargoTag).Length -le 4096 -and ([IO.File]::ReadAllText($cargoTag).StartsWith('Signature: 8a477f597d28d172789f06886806bc55',[StringComparison]::Ordinal))
    if($entries.Count -and !($entries.Count -eq 1 -and $owner) -and !$hasCargoTag){throw 'Nonempty build cache lacks the real Cargo cache marker.'}
    if($Action -eq 'Register'){
        if(!$explicitTarget){throw 'Register requires an explicitly named -TargetDirectory.'}
        if(!$owner -and $entries.Count -and !$AdoptExisting){throw 'Existing external cache needs explicit -AdoptExisting after checking exclusive ownership.'}
    }elseif(!$TargetDirectory.Equals($defaultTarget,$comparison) -and !$owner -and $Action -eq 'Run'){throw 'External cache is not registered to this workspace; register it explicitly before cleanup.'}
    $report.ownership=if($owner){'registered'}elseif($TargetDirectory.Equals($defaultTarget,$comparison)){'project-target'}else{'unregistered-preview-only'}
    try{$lease=Enter-AudioLibBuildLease $Root -Exclusive:($Action -ne 'Plan')}catch{$report.status='blocked';$report.reason='A managed build/test/release or cleanup is active.';$report|ConvertTo-Json -Compress -Depth 5;exit 2}
    function Get-BuildUsers { @(Get-Process | Where-Object {
        if($_.ProcessName -match '^(cargo|rustc|rustdoc|clippy-driver|clang|clang\+\+|gcc|g\+\+|cc1|cc1plus|link|lld|lld-link|cl)$'){return $true}
        try{$processPath=$_.Path;if($processPath){return Is-Within (Get-AudioLibCanonicalPath $processPath) $TargetDirectory}}catch{}
        return $false
    } | Select-Object ProcessName,Id) }
    $active=@(Get-BuildUsers)
    if($active.Count){$report.status='blocked';$report.reason='Compiler or target executable is active; no process was stopped.';$report.active_count=$active.Count;$report.active=@($active|Select-Object -First 5);$report|ConvertTo-Json -Compress -Depth 5;exit 2}
    if($Action -eq 'Register'){
        if($PSCmdlet.ShouldProcess($TargetDirectory,'Register owned Cargo cache')){
            @{schema_version=1;owner_id=$ownerId;host=[Environment]::MachineName}|ConvertTo-Json|Set-Content -LiteralPath $ownerPath -Encoding utf8
            $report.status='registered'
        }
        $report|ConvertTo-Json -Compress -Depth 5;exit 0
    }
    $profiles=@()
    foreach($entry in $entries|Where-Object PSIsContainer){
        if($entry.Name -in @('debug','release','doc','tmp')){$profiles+=,$entry}
        elseif($entry.Name -notin @('doc','tmp')){
            foreach($name in @('debug','release','doc','tmp')){ $path=Join-Path $entry.FullName $name;if(Test-Path -LiteralPath $path -PathType Container){Assert-AudioLibUnlinkedPath $path;$profiles+=, (Get-Item -LiteralPath $path)} }
        }
    }
    foreach($profile in $profiles){
        $size=0L;$incrementalSize=0L;$latest=$profile.LastWriteTimeUtc
        $incremental=Join-Path $profile.FullName 'incremental'
        foreach($entry in Get-ChildItem -LiteralPath $profile.FullName -Recurse -Force){
            if($entry.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Linked entry in Cargo profile; cleanup refused.'}
            if($entry.LastWriteTimeUtc -gt $latest){$latest=$entry.LastWriteTimeUtc}
            if(!$entry.PSIsContainer){$size+=$entry.Length;if(Is-Within $entry.FullName $incremental){$incrementalSize+=$entry.Length}}
        }
        $report.before_bytes+=$size
        $path=if($Mode -eq 'Incremental'){$incremental}else{$profile.FullName}
        $selected=($Mode -eq 'All' -or $Mode -eq 'Incremental' -or $profile.Name -eq 'debug') -and (Test-Path -LiteralPath $path -PathType Container) -and $latest -le [DateTime]::UtcNow.AddDays(-$OlderThanDays)
        if($selected){$report.candidates+=,[ordered]@{path=$path;bytes=$(if($Mode -eq 'Incremental'){$incrementalSize}else{$size});last_write_utc=$latest.ToString('o');status='eligible'}}
    }
    $report.remaining_bytes=$report.before_bytes
    $limit=[long]($MaxGiB*1GB)
    foreach($candidate in @($report.candidates|Sort-Object last_write_utc)){
        if($MaxGiB -gt 0 -and $report.remaining_bytes -le $limit){$candidate.status='retained-budget';continue}
        if($Action -eq 'Plan'){$candidate.status='would-remove';$report.remaining_bytes-=$candidate.bytes;continue}
        $absolute=Get-AudioLibCanonicalPath $candidate.path
        if(!(Is-Within $absolute $TargetDirectory) -or $absolute.Equals($TargetDirectory,$comparison)){throw 'Cleanup destination escaped its verified build target.'}
        Assert-AudioLibUnlinkedPath $absolute
        $active=@(Get-BuildUsers)
        if($active.Count){$report.status='blocked';$report.reason='A compiler/target executable started during inventory; remaining candidates retained.';$report.active_count=$active.Count;$report|ConvertTo-Json -Compress -Depth 5;exit 2}
        if($PSCmdlet.ShouldProcess($absolute,'Remove owned Cargo profile/cache')){
            Remove-Item -LiteralPath $absolute -Recurse -Force
            $candidate.status='removed';$report.removed_bytes+=$candidate.bytes;$report.remaining_bytes-=$candidate.bytes
        }else{$candidate.status='retained-whatif'}
    }
    if($MaxGiB -gt 0){$report.budget_met=$report.remaining_bytes -le $limit}
    $report.status=if($Action -eq 'Plan'){'planned'}elseif($WhatIfPreference){'whatif'}else{'passed'}
    $report|ConvertTo-Json -Compress -Depth 5
    exit 0
}catch{
    if($report){$report.status='failed';$report.error=$_.Exception.Message;$report|ConvertTo-Json -Compress -Depth 5}else{@{status='failed';error=$_.Exception.Message}|ConvertTo-Json -Compress}
    exit 1
}finally{if($lease){$lease.Dispose()}}
