# tools/dependency-licenses.ps1
# Selected-root normal/build package inventory (not a compiler unit graph).
# https://doc.rust-lang.org/cargo/commands/cargo-tree.html#feature-unification
function Assert-LicenseRelative([string]$Path) {
    if (!$Path -or [IO.Path]::IsPathRooted($Path) -or $Path -match '[\\:\x00-\x1f]' -or @($Path.Split('/')|Where-Object {$_ -in @('','.','..')}).Count) { throw 'Unsafe license relative path.' }
}
function Assert-LicensePath([string]$Path,[string]$Parent) {
    $full=[IO.Path]::GetFullPath($Path);$comparison=if($IsWindows){[StringComparison]::OrdinalIgnoreCase}else{[StringComparison]::Ordinal}
    if($Parent){$base=[IO.Path]::GetFullPath($Parent).TrimEnd([IO.Path]::DirectorySeparatorChar);if(!$full.StartsWith($base+[IO.Path]::DirectorySeparatorChar,$comparison)){throw 'License path escaped its owned boundary.'}}
    for($cursor=$full;$cursor;){$item=Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue;if($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)){throw 'Linked license path refused.'};$next=[IO.Path]::GetDirectoryName($cursor);if($next -eq $cursor){break};$cursor=$next}
    return $full
}
function Get-LicenseTextHash([string]$Text) { [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($Text))).ToLowerInvariant() }
function Get-LicenseIdentityHash($Packages) {
    [string[]]$records=@($Packages|ForEach-Object {ConvertTo-Json -InputObject @($_.name,$_.version,$_.license) -Compress})
    [Array]::Sort($records,[StringComparer]::Ordinal)
    Get-LicenseTextHash (($records -join "`n")+"`n")
}
function Get-PinnedLicenseProvenance($Entry) {
    if(!$Entry -or !$Entry.PSObject.Properties['fallback_kind']){return $null}
    if($Entry.fallback_kind -cne 'upstream-declaration-with-standard-text' -or $Entry.missing_standalone_file -ne $true -or $Entry.upstream_commit -cnotmatch '^[0-9a-f]{40}$' -or $Entry.standard_text_scope -cne 'permission-conditions-disclaimer'){throw 'Invalid pinned license declaration fallback.'}
    $files=@($Entry.files|Sort-Object {[IO.Path]::GetFileName($_.path)}|ForEach-Object {[ordered]@{path=[IO.Path]::GetFileName($_.path);sha256=$_.sha256;source=$_.source}})
    $sources=@($Entry.provenance|Sort-Object {$_.role},{$_.url}|ForEach-Object {if($_.url -cnotmatch '^https://[^/@\s?#]+(?:/[^\s?#]*)?$' -or $_.sha256 -cnotmatch '^[0-9a-f]{64}$'){throw 'Invalid pinned license provenance source.'};[ordered]@{url=$_.url;sha256=$_.sha256;role=$_.role}})
    return [ordered]@{fallback_kind=$Entry.fallback_kind;missing_standalone_file=$true;upstream_commit=$Entry.upstream_commit;standard_text_scope=$Entry.standard_text_scope;files=$files;provenance=$sources}
}
function Get-PublicLicenseProvenanceHash($Value) {
    $entry=[pscustomobject]@{fallback_kind=$Value.fallback_kind;missing_standalone_file=$Value.missing_standalone_file;upstream_commit=$Value.upstream_commit;standard_text_scope=$Value.standard_text_scope;files=$Value.files;provenance=$Value.provenance}
    Get-LicenseTextHash (ConvertTo-Json -InputObject (Get-PinnedLicenseProvenance $entry) -Depth 12 -Compress)
}
function Assert-SharedAsioNotice($Package,$Packages,[string]$Path){
    $cpal=@($Packages|Where-Object name -CEQ 'cpal');$repository='https://github.com/RustAudio/cpal'
    if($Package.name -cne 'asio-sys' -or $Package.version -cne '0.4.0' -or $Package.license -cne 'Apache-2.0' -or $Package.repository -cne $repository -or $cpal.Count -ne 1 -or $cpal[0].version -cne '0.18.2' -or $cpal[0].license -cne 'Apache-2.0' -or $cpal[0].repository -cne $repository -or !(Test-Path -LiteralPath $Path -PathType Leaf)){throw 'Packaged asio-sys shared CPAL license identity is invalid or missing.'}
    if((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -cne 'C71D239DF91726FC519C6EB72D318EC65820627232B2F796219E87DCF35D0AB4'){throw 'Packaged asio-sys shared CPAL license differs from the audited upstream notice.'}
}
function Get-AudioLibLicenseGraph {
    param([Parameter(Mandatory)][string]$Root,[Parameter(Mandatory)][string[]]$Packages,[Parameter(Mandatory)][string]$Target,[string[]]$Features=@(),[switch]$NoDefaultFeatures,$Metadata)
    $Features=@($Features|Where-Object {$_})
    $treeArgs=@('tree','--locked','--offline','--target',$Target,'--edges','normal,build','--prefix','none','--format','{p}','--color','never')
    foreach($name in $Packages){$treeArgs+=@('-p',$name)}
    $metaArgs=@('metadata','--format-version','1','--locked','--offline','--filter-platform',$Target)
    if($Features.Count){$treeArgs+=@('--features',($Features -join ','));$metaArgs+=@('--features',($Features -join ','))}
    if($NoDefaultFeatures){$treeArgs+='--no-default-features';$metaArgs+='--no-default-features'}
    Push-Location -LiteralPath $Root
    try {
        if(!$Metadata){$raw=(& cargo @metaArgs 2>$null | Out-String);if($LASTEXITCODE){throw 'License metadata resolution failed (locked/offline).'};$Metadata=$raw|ConvertFrom-Json}
        $tree=@(& cargo @treeArgs 2>$null);if($LASTEXITCODE){throw 'Selected license dependency tree resolution failed (locked/offline).'}
    }finally{Pop-Location}
    $selected=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($line in $tree){if([string]::IsNullOrWhiteSpace($line)){continue};if($line -cnotmatch '^([A-Za-z0-9_-]+) v([^\s]+)(?:\s.*)?$'){throw 'Unrecognized Cargo dependency identity.'};$null=$selected.Add($Matches[1]+'@'+$Matches[2])}
    $resolved=@();foreach($key in ($selected|Sort-Object)){$matchingPackages=@($Metadata.packages|Where-Object {($_.name+'@'+$_.version)-ceq$key});if($matchingPackages.Count -ne 1){throw "Missing or ambiguous selected license identity: $key"};$resolved+=$matchingPackages[0]}
    foreach($name in $Packages){if(@($resolved|Where-Object name -CEQ $name).Count -ne 1){throw 'Selected license root missing.'}}
    if(!$resolved.Count){throw 'Empty license dependency closure.'}
    return [pscustomobject]@{packages=@($resolved|Sort-Object name,version);roots=@($Packages|Sort-Object);target=$Target;features=@($Features|Sort-Object);no_default_features=[bool]$NoDefaultFeatures}
}
function Get-AudioLibNoticePlan {
    param($Graph,[string]$Root)
    $overrideFile=Join-Path $PSScriptRoot 'license-notices.json';$overrides=if(Test-Path -LiteralPath $overrideFile){@(Get-Content -Raw -LiteralPath $overrideFile|ConvertFrom-Json)}else{@()}
    $plans=@();foreach($package in $Graph.packages){
        if($package.name -cnotmatch '^[A-Za-z0-9_-]+$' -or $package.version -cnotmatch '^[A-Za-z0-9.+_-]+$'){throw 'Unsafe dependency notice identity.'}
        $sourceRoot=Split-Path -Parent $package.manifest_path;$null=Assert-LicensePath $sourceRoot
        $files=[Collections.Generic.List[object]]::new();$seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach($entry in (Get-ChildItem -LiteralPath $sourceRoot -Force|Where-Object Name -Match '^(LICENSE|COPYING|NOTICE)')){
            $children=if($entry.PSIsContainer){if($entry.Name -notmatch '^(LICENSES?|COPYING|NOTICE)$'){continue};@(Get-ChildItem -LiteralPath $entry.FullName -Recurse -File -Force)}else{@($entry)}
            foreach($file in $children){$null=Assert-LicensePath $file.FullName $sourceRoot;$relative=[IO.Path]::GetRelativePath($sourceRoot,$file.FullName).Replace('\','/');Assert-LicenseRelative $relative;if($seen.Add($relative)){$files.Add(@{source=$file.FullName;relative=$relative})}}
        }
        if($package.license_file){$license=[IO.Path]::GetFullPath($package.license_file,$sourceRoot);$null=Assert-LicensePath $license $sourceRoot;$relative=[IO.Path]::GetFileName($license);if($relative -notmatch '^(LICENSE|COPYING|NOTICE)'){$relative='LICENSE-declared-'+$relative};if($seen.Add($relative)){$files.Add(@{source=$license;relative=$relative})}}
        if($package.name -ceq 'soundtouch-ffi'){$license=Join-Path $sourceRoot 'soundtouch-2_3_2/COPYING.TXT';$null=Assert-LicensePath $license $sourceRoot;$files.Add(@{source=$license;relative='SoundTouch-LGPL.txt'})}
        if(!$files.Count -and !$package.source){$license=Join-Path $Root 'LICENSE';$null=Assert-LicensePath $package.manifest_path $Root;if($package.license -cne 'GPL-3.0-only'){throw 'Unknown workspace license fallback.'};$files.Add(@{source=$license;relative='LICENSE'})}
        $shared=@();if(!$files.Count -and $package.name -ceq 'asio-sys'){$cpal=@($Graph.packages|Where-Object name -CEQ 'cpal');$path=if($cpal.Count -eq 1){Join-Path (Split-Path -Parent $cpal[0].manifest_path) 'LICENSE'}else{Join-Path $Root 'missing-cpal-notice'};$null=Assert-LicensePath $path;Assert-SharedAsioNotice $package $Graph.packages $path;$shared=@(@{path='cpal-0.18.2/LICENSE';sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()})}
        $override=@($overrides|Where-Object {$_.name -ceq $package.name -and $_.version -ceq $package.version})
        if($override.Count -gt 1){throw 'Duplicate dependency notice override.'}
        if($override.Count -and !$shared.Count){if($override[0].license -cne $package.license){throw 'Pinned notice license expression changed.'};foreach($file in $override[0].files){Assert-LicenseRelative $file.path;$path=Join-Path $PSScriptRoot $file.path;$null=Assert-LicensePath $path (Join-Path $PSScriptRoot 'license-notices');if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256){throw 'Pinned upstream license notice identity mismatch.'};$relative=[IO.Path]::GetFileName($path);if($seen.Add($relative)){$files.Add(@{source=$path;relative=$relative})}}}
        if(!$files.Count -and !$shared.Count){throw "Required license text unavailable: $($package.name)@$($package.version)"}
        $repository=if($package.repository -and $package.repository -cmatch '^https://[^/@\s?#]+(?:/[^\s?#]*)?$'){$package.repository}else{$null}
        $provenance=if($override.Count){Get-PinnedLicenseProvenance $override[0]}else{$null}
        if($provenance){$vcs=Get-Content -Raw -LiteralPath (Join-Path $sourceRoot '.cargo_vcs_info.json')|ConvertFrom-Json;if($vcs.git.sha1 -cne $provenance.upstream_commit){throw 'Fallback license upstream commit differs from the selected published crate.'}}
        $plans+=@{name=$package.name;version=$package.version;license=$package.license;repository=$repository;files=$files.ToArray();provenance=$provenance;shared_notices=$shared}
    };return $plans
}
function Write-AudioLibLicenseBundle {
    param($Graph,[string]$Root,[string]$PayloadRoot,[string]$Prefix='licenses',[hashtable]$Artifacts,[hashtable]$ExtraFiles=@{},[switch]$MacManifest)
    Assert-LicenseRelative $Prefix;$licenseRoot=Join-Path $PayloadRoot $Prefix;$null=Assert-LicensePath $licenseRoot $PayloadRoot;New-Item -ItemType Directory -Path $licenseRoot -Force|Out-Null
    $written=[Collections.Generic.List[string]]::new();$public=@();$inventory=@()
    foreach($plan in (Get-AudioLibNoticePlan $Graph $Root)){
        $notices=@();foreach($file in $plan.files){$relative=$plan.name+'-'+$plan.version+'/'+$file.relative;Assert-LicenseRelative $relative;$destination=Join-Path $licenseRoot $relative;$null=Assert-LicensePath $destination $licenseRoot;New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force|Out-Null;Copy-Item -LiteralPath $file.source -Destination $destination -Force;$written.Add($Prefix+'/'+$relative);$notices+=@{path=$relative;sha256=(Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()}}
        $row=[ordered]@{name=$plan.name;version=$plan.version;license=$plan.license;repository=$plan.repository};if($MacManifest){$row.files=@($plan.files|ForEach-Object relative|Sort-Object)};$public+=$row
        $inventory+=@{name=$plan.name;version=$plan.version;license=$plan.license;notices=$notices;provenance=$plan.provenance;shared_notices=$plan.shared_notices}
    }
    $extras=@();foreach($relative in ($ExtraFiles.Keys|Sort-Object)){Assert-LicenseRelative $relative;$destination=Join-Path $licenseRoot $relative;$null=Assert-LicensePath $destination $licenseRoot;$null=Assert-LicensePath $ExtraFiles[$relative];Copy-Item -LiteralPath $ExtraFiles[$relative] -Destination $destination -Force;$written.Add($Prefix+'/'+$relative);$extras+=@{path=$relative;sha256=(Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()}}
    $binaries=@();foreach($relative in ($Artifacts.Keys|Sort-Object)){Assert-LicenseRelative $relative;$null=Assert-LicensePath $Artifacts[$relative] $PayloadRoot;$binaries+=@{path=$relative;sha256=(Get-FileHash -LiteralPath $Artifacts[$relative] -Algorithm SHA256).Hash.ToLowerInvariant()}}
    $inventory=[ordered]@{schema_version=1;scope='selected Cargo package normal/build closure; dev edges excluded; not a compiler unit graph';roots=$Graph.roots;target=$Graph.target;features=$Graph.features;no_default_features=$Graph.no_default_features;closure_sha256=Get-LicenseIdentityHash $public;packages=$inventory;artifact_binding=$(if($MacManifest){'signed-bundle-report'}else{'payload-sha256'});artifacts=$binaries;extra_notices=$extras}
    foreach($entry in @(@{name='dependencies.json';value=@($public)},@{name='inventory.json';value=$inventory})){$destination=Join-Path $licenseRoot $entry.name;$null=Assert-LicensePath $destination $licenseRoot;[IO.File]::WriteAllText($destination,(ConvertTo-Json -InputObject $entry.value -Depth 12)+"`n",[Text.UTF8Encoding]::new($false));$written.Add($Prefix+'/'+$entry.name)}
    return $written.ToArray()
}
function Test-AudioLibLicenseBundle {
    param([string]$PayloadRoot,[string]$Prefix='licenses',[string[]]$ExpectedRoots,[string]$Target,[string[]]$Features=@(),[switch]$NoDefaultFeatures,[string[]]$ExpectedArtifacts,[switch]$SignedBundle)
    $Features=@($Features|Where-Object {$_})
    $root=Join-Path $PayloadRoot $Prefix;$null=Assert-LicensePath $root $PayloadRoot
    $inventory=Get-Content -Raw -LiteralPath (Join-Path $root 'inventory.json')|ConvertFrom-Json
    $packages=@(Get-Content -Raw -LiteralPath (Join-Path $root 'dependencies.json')|ConvertFrom-Json)
    if($inventory.schema_version -ne 1 -or $inventory.target -cne $Target -or [bool]$inventory.no_default_features -ne [bool]$NoDefaultFeatures -or (@($inventory.roots|Sort-Object)-join ',') -cne (@($ExpectedRoots|Sort-Object)-join ',') -or (@($inventory.features|Sort-Object)-join ',') -cne (@($Features|Sort-Object)-join ',')){throw 'License inventory build selection differs from the selected payload.'}
    if(!$packages.Count -or $packages.Count -ne @($inventory.packages).Count -or (Get-LicenseIdentityHash $packages) -cne $inventory.closure_sha256 -or (Get-LicenseIdentityHash $inventory.packages) -cne $inventory.closure_sha256){throw 'License dependency inventory is missing or inconsistent.'}
    $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase);$null=$seen.Add('dependencies.json');$null=$seen.Add('inventory.json');$ids=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $overrides=@(Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'license-notices.json')|ConvertFrom-Json)
    foreach($package in $inventory.packages){if($package.name -cnotmatch '^[A-Za-z0-9_-]+$' -or $package.version -cnotmatch '^[A-Za-z0-9.+_-]+$' -or !$ids.Add($package.name+'@'+$package.version) -or (!@($package.notices).Count -and !@($package.shared_notices).Count)){throw 'Invalid dependency license notice inventory.'};foreach($notice in $package.notices){if(!$notice.path.StartsWith($package.name+'-'+$package.version+'/',[StringComparison]::Ordinal)){throw 'License notice belongs to a different dependency.'}};foreach($notice in $package.shared_notices){if($notice.path -cne 'cpal-0.18.2/LICENSE' -or $notice.sha256 -cne 'c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4'){throw 'Invalid shared dependency notice reference.'};$identity=@($packages|Where-Object {$_.name -ceq $package.name -and $_.version -ceq $package.version});if($identity.Count -ne 1){throw 'Shared notice dependency identity missing.'};Assert-SharedAsioNotice $identity[0] $packages (Join-Path $root $notice.path)};$pinned=@($overrides|Where-Object {$_.name -ceq $package.name -and $_.version -ceq $package.version});$expected=if($pinned.Count){Get-PinnedLicenseProvenance $pinned[0]}else{$null};if($expected){if(!$package.provenance -or (Get-PublicLicenseProvenanceHash $package.provenance) -cne (Get-PublicLicenseProvenanceHash $expected)){throw 'Pinned license fallback provenance mismatch.'}}elseif($package.provenance){throw 'Unrecognized license fallback provenance.'}}
    foreach($name in $ExpectedRoots){if(!@($inventory.packages|Where-Object name -CEQ $name).Count){throw 'License dependency root identity missing.'}}
    foreach($notice in @(@($inventory.packages|ForEach-Object notices)+@($inventory.extra_notices))){Assert-LicenseRelative $notice.path;if(!$seen.Add($notice.path) -or $notice.sha256 -cnotmatch '^[0-9a-f]{64}$'){throw 'Duplicate or invalid license notice identity.'};$path=Join-Path $root $notice.path;$null=Assert-LicensePath $path $root;if(!(Test-Path -LiteralPath $path -PathType Leaf)){throw "Required license notice missing: $($notice.path)"};if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $notice.sha256){throw "License notice identity mismatch: $($notice.path)"}}
    if($inventory.artifact_binding -cne $(if($SignedBundle){'signed-bundle-report'}else{'payload-sha256'}) -or ($SignedBundle -and @($inventory.artifacts).Count)){throw 'License artifact identity binding mismatch.'}
    if((@($inventory.artifacts|ForEach-Object path|Sort-Object)-join ',') -cne (@($ExpectedArtifacts|Sort-Object)-join ',')){throw 'License inventory artifact selection mismatch.'}
    foreach($artifact in $inventory.artifacts){Assert-LicenseRelative $artifact.path;$path=Join-Path $PayloadRoot $artifact.path;$null=Assert-LicensePath $path $PayloadRoot;if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $artifact.sha256){throw 'License inventory executable identity mismatch.'}}
    foreach($file in (Get-ChildItem -LiteralPath $root -Recurse -File -Force)){$null=Assert-LicensePath $file.FullName $root;$relative=[IO.Path]::GetRelativePath($root,$file.FullName).Replace('\','/');if(!$seen.Contains($relative)){throw 'Uninventoried license payload file.'}}
    return $inventory
}
