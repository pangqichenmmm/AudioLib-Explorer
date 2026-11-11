# tools/test-dependency-licenses.ps1
# Pure PowerShell fixtures: no compiler, application, server or real package build.
[CmdletBinding()]param()
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'dependency-licenses.ps1')
$parent=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([IO.Path]::DirectorySeparatorChar)
$ownedRoot=Join-Path $parent ('AudioLibLicenseFixture-'+[guid]::NewGuid().ToString('N'));$null=Assert-LicensePath $ownedRoot $parent
New-Item -ItemType Directory -Path $ownedRoot|Out-Null
function Expect-LicenseFailure([scriptblock]$Action,[string]$Pattern){$failed=$false;try{&$Action|Out-Null}catch{$failed=$_.Exception.Message -match $Pattern};if(!$failed){throw "Expected license rejection: $Pattern"}}
try{
    Set-Content -LiteralPath (Join-Path $ownedRoot 'LICENSE') -Value 'Synthetic workspace GPL notice.'
    $fixturePackages=@();foreach($spec in @(@('audiolib-desktop','0.9.0'),@('audiolib-cli','0.9.0'),@('calamine','0.36.1'),@('symphonia-codec-vorbis','0.6.1'),@('build-helper','1.0.0'),@('dev-only','1.0.0'),@('other-platform','1.0.0'))){
        $dir=Join-Path $ownedRoot ('registry/'+$spec[0]);New-Item -ItemType Directory -Path $dir -Force|Out-Null;Set-Content -LiteralPath (Join-Path $dir 'Cargo.toml') -Value 'synthetic'
        if($spec[0] -notlike 'audiolib-*'){Set-Content -LiteralPath (Join-Path $dir 'LICENSE') -Value ('Synthetic original notice for '+$spec[0])}
        $fixturePackages += [pscustomobject]@{name=$spec[0];version=$spec[1];source=$(if($spec[0] -like 'audiolib-*'){$null}else{'registry+public'});license=$(if($spec[0] -like 'audiolib-*'){'GPL-3.0-only'}else{'MIT'});license_file=$null;repository='https://example.org/public';manifest_path=(Join-Path $dir 'Cargo.toml')}
    }
    function cargo {
        if($args -notcontains '--locked' -or $args -notcontains '--offline'){throw 'Unpinned license resolver invocation.'}
        if($args[0] -ceq 'metadata'){if($args -notcontains '--filter-platform'){throw 'Metadata target filter missing.'};[pscustomobject]@{packages=$fixturePackages}|ConvertTo-Json -Depth 8}
        elseif($args[0] -ceq 'tree'){
            if($args -notcontains 'normal,build' -or $args -notcontains '--target' -or $args -contains '--workspace'){throw 'Wrong selected dependency closure.'}
            foreach($p in $fixturePackages|Where-Object name -CNotIn @('dev-only','other-platform')){"$($p.name) v$($p.version)"}
        }else{throw 'Synthetic license fixture attempted a build or unsupported Cargo command.'};$global:LASTEXITCODE=0
    }
    $graph=Get-AudioLibLicenseGraph -Root $ownedRoot -Packages @('audiolib-desktop','audiolib-cli') -Target 'x86_64-pc-windows-msvc'
    if(@($graph.packages|Where-Object name -CIn @('dev-only','other-platform')).Count -or !@($graph.packages|Where-Object name -CEQ 'build-helper').Count){throw 'Dev/platform exclusion or build-edge inclusion failed.'}
    $payload=Join-Path $ownedRoot 'payload';New-Item -ItemType Directory -Path $payload|Out-Null;Set-Content -LiteralPath (Join-Path $payload 'client.bin') -Value 'Synthetic binary identity, never executable.'
    $null=Write-AudioLibLicenseBundle -Graph $graph -Root $ownedRoot -PayloadRoot $payload -Artifacts @{'client.bin'=(Join-Path $payload 'client.bin')}
    $check={Test-AudioLibLicenseBundle -PayloadRoot $payload -ExpectedRoots @('audiolib-desktop','audiolib-cli') -Target 'x86_64-pc-windows-msvc' -ExpectedArtifacts @('client.bin')}
    $inventory=&$check
    $notice=Join-Path $payload 'licenses/calamine-0.36.1/LICENSE';$original=[IO.File]::ReadAllBytes($notice)
    Remove-Item -LiteralPath $notice;Expect-LicenseFailure $check 'license notice missing';[IO.File]::WriteAllBytes($notice,$original)
    Add-Content -LiteralPath $notice -Value 'tampered';Expect-LicenseFailure $check 'notice identity mismatch';[IO.File]::WriteAllBytes($notice,$original)
    $dependencies=Join-Path $payload 'licenses/dependencies.json';$saved=[IO.File]::ReadAllText($dependencies);@((Get-Content -Raw -LiteralPath $dependencies|ConvertFrom-Json)|Where-Object name -CNE 'symphonia-codec-vorbis')|ConvertTo-Json -Depth 8|Set-Content -LiteralPath $dependencies;Expect-LicenseFailure $check 'inventory is missing or inconsistent';[IO.File]::WriteAllText($dependencies,$saved)
    Expect-LicenseFailure {Test-AudioLibLicenseBundle -PayloadRoot $payload -ExpectedRoots @('audiolib-desktop','audiolib-cli') -Target 'aarch64-apple-darwin' -ExpectedArtifacts @('client.bin')} 'build selection differs'
    Add-Content -LiteralPath (Join-Path $payload 'client.bin') -Value 'changed';Expect-LicenseFailure $check 'executable identity mismatch';Set-Content -LiteralPath (Join-Path $payload 'client.bin') -Value 'Synthetic binary identity, never executable.'
    Set-Content -LiteralPath (Join-Path $payload 'licenses/secret.txt') -Value 'unlisted';Expect-LicenseFailure $check 'Uninventoried';Remove-Item -LiteralPath (Join-Path $payload 'licenses/secret.txt')
    # Native Mac dependencies published without standalone notices must still
    # produce a complete bundle through their exact audited catalog entries.
    $macSpecs=@(@('block2','0.5.1','MIT'),@('dispatch','0.2.0','MIT'),@('dispatch2','0.3.1','Zlib OR Apache-2.0 OR MIT'),@('objc-sys','0.3.5','MIT'),@('objc2','0.5.3','MIT'),@('objc2-app-kit','0.2.2','MIT'),@('objc2-audio-toolbox','0.3.2','Zlib OR Apache-2.0 OR MIT'),@('objc2-core-audio','0.3.2','Zlib OR Apache-2.0 OR MIT'),@('objc2-core-audio-types','0.3.2','Zlib OR Apache-2.0 OR MIT'),@('objc2-foundation','0.2.2','MIT'))
    $catalog=@(Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'license-notices.json')|ConvertFrom-Json)
    $macPackages=@();foreach($spec in $macSpecs){
        $dir=Join-Path $ownedRoot ('mac-registry/'+$spec[0]+'-'+$spec[1]);New-Item -ItemType Directory -Path $dir -Force|Out-Null;Set-Content -LiteralPath (Join-Path $dir 'Cargo.toml') -Value 'synthetic published Mac dependency without embedded license text'
        $entry=@($catalog|Where-Object {$_.name -ceq $spec[0] -and $_.version -ceq $spec[1]});if($entry.Count -ne 1){throw 'Native Mac dependency notice catalog entry missing or ambiguous.'}
        if($entry[0].PSObject.Properties['fallback_kind']){@{git=@{sha1=$entry[0].upstream_commit}}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $dir '.cargo_vcs_info.json')}
        $macPackages += [pscustomobject]@{name=$spec[0];version=$spec[1];license=$spec[2];source='registry+public';repository=$null;license_file=$null;manifest_path=(Join-Path $dir 'Cargo.toml')}
    }
    $macGraph=[pscustomobject]@{packages=@($fixturePackages|Where-Object name -CLike 'audiolib-*')+$macPackages;roots=@('audiolib-desktop','audiolib-cli');target='aarch64-apple-darwin';features=@();no_default_features=$true}
    $macPayload=Join-Path $ownedRoot 'mac-payload';New-Item -ItemType Directory -Path $macPayload|Out-Null
    $null=Write-AudioLibLicenseBundle -Graph $macGraph -Root $ownedRoot -PayloadRoot $macPayload -Artifacts @{}
    $null=Test-AudioLibLicenseBundle -PayloadRoot $macPayload -ExpectedRoots @('audiolib-desktop','audiolib-cli') -Target 'aarch64-apple-darwin' -NoDefaultFeatures -ExpectedArtifacts @()
    $unknown=$macPackages[0].PSObject.Copy();$unknown.version='0.5.999'
    Expect-LicenseFailure {Get-AudioLibNoticePlan -Graph ([pscustomobject]@{packages=@($unknown)}) -Root $ownedRoot} 'Required license text unavailable'
    $wrongLicense=$macPackages[0].PSObject.Copy();$wrongLicense.license='Apache-2.0'
    Expect-LicenseFailure {Get-AudioLibNoticePlan -Graph ([pscustomobject]@{packages=@($wrongLicense)}) -Root $ownedRoot} 'license expression changed'
    # Real pinned fallback metadata, synthetic source package and no execution.
    $fallback=@(Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'license-notices.json')|ConvertFrom-Json|Where-Object {$_.PSObject.Properties['fallback_kind']})
    foreach($pinned in $fallback){
        $dir=Join-Path $ownedRoot ('registry/'+$pinned.name);New-Item -ItemType Directory -Path $dir -Force|Out-Null;Set-Content -LiteralPath (Join-Path $dir 'Cargo.toml') -Value 'synthetic published package';@{git=@{sha1=$pinned.upstream_commit}}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $dir '.cargo_vcs_info.json')
        $one=[pscustomobject]@{name=$pinned.name;version=$pinned.version;license=$pinned.license;source='registry+public';repository=$null;license_file=$null;manifest_path=(Join-Path $dir 'Cargo.toml')}
        $fallbackGraph=[pscustomobject]@{packages=@($one);roots=@($pinned.name);target='fixture';features=@();no_default_features=$true};$dest=Join-Path $ownedRoot ('fallback-'+$pinned.name);New-Item -ItemType Directory -Path $dest|Out-Null
        $null=Write-AudioLibLicenseBundle -Graph $fallbackGraph -Root $ownedRoot -PayloadRoot $dest -Artifacts @{}
        $argsForFallback=@{PayloadRoot=$dest;ExpectedRoots=@($pinned.name);Target='fixture';NoDefaultFeatures=$true;ExpectedArtifacts=@()};$null=Test-AudioLibLicenseBundle @argsForFallback
        $vcsPath=Join-Path $dir '.cargo_vcs_info.json';$vcsBytes=[IO.File]::ReadAllBytes($vcsPath);@{git=@{sha1=('0'*40)}}|ConvertTo-Json|Set-Content -LiteralPath $vcsPath
        Expect-LicenseFailure {Get-AudioLibNoticePlan -Graph $fallbackGraph -Root $ownedRoot} 'upstream commit differs';[IO.File]::WriteAllBytes($vcsPath,$vcsBytes)
        $wrongLicense=$one.PSObject.Copy();$wrongLicense.license='Apache-2.0'
        Expect-LicenseFailure {Get-AudioLibNoticePlan -Graph ([pscustomobject]@{packages=@($wrongLicense)}) -Root $ownedRoot} 'license expression changed'
        $standardPath=Join-Path $dest ('licenses/'+$pinned.name+'-'+$pinned.version+'/LICENSE-MIT-STANDARD.txt');$standardBytes=[IO.File]::ReadAllBytes($standardPath);Add-Content -LiteralPath $standardPath -Value 'tampered'
        Expect-LicenseFailure {Test-AudioLibLicenseBundle @argsForFallback} 'notice identity mismatch';[IO.File]::WriteAllBytes($standardPath,$standardBytes)
        $path=Join-Path $dest 'licenses/inventory.json';$data=Get-Content -Raw -LiteralPath $path|ConvertFrom-Json;$data.packages[0].provenance.upstream_commit='0000000000000000000000000000000000000000';$data|ConvertTo-Json -Depth 14|Set-Content -LiteralPath $path;Expect-LicenseFailure {Test-AudioLibLicenseBundle @argsForFallback} 'fallback provenance mismatch'
    }
    if($fallback.Count -ne 3 -or @($fallback|Where-Object name -CEQ 'dispatch').Count -ne 1){throw 'All three explicit upstream-declaration fallbacks must be exercised.'}
    # A fresh index checkout must retain the pinned bytes even when a Windows
    # user's Git configuration requests CRLF. No commit, hook or compiler runs.
    $gitFixture=Join-Path $ownedRoot 'git-eol';$fresh=Join-Path $ownedRoot 'fresh-eol';$emptyTemplate=Join-Path $ownedRoot 'empty-template';New-Item -ItemType Directory -Path $gitFixture,$fresh,$emptyTemplate|Out-Null
    $attributeSource=Join-Path $PSScriptRoot 'license-notices/.gitattributes'
    $noticeRoot=Join-Path $gitFixture 'tools/license-notices';New-Item -ItemType Directory -Path $noticeRoot -Force|Out-Null;Copy-Item -LiteralPath $attributeSource -Destination $noticeRoot
    $pinnedFiles=@((Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'license-notices.json')|ConvertFrom-Json)|ForEach-Object files);$index=0
    foreach($pinnedFile in $pinnedFiles){$rawPath=Join-Path $PSScriptRoot $pinnedFile.path;Copy-Item -LiteralPath $rawPath -Destination (Join-Path $noticeRoot "LICENSE-pinned-$index.txt");$index++}
    & git -c "init.templateDir=$emptyTemplate" -C $gitFixture init --quiet;if($LASTEXITCODE){throw 'Owned Git EOL fixture initialization failed.'}
    & git -c core.autocrlf=true -c core.eol=crlf -C $gitFixture add -- tools/license-notices;if($LASTEXITCODE){throw 'Owned Git EOL fixture staging failed.'}
    & git -c core.autocrlf=true -c core.eol=crlf -C $gitFixture checkout-index --all ('--prefix='+$fresh.Replace('\','/')+'/');if($LASTEXITCODE){throw 'Owned fresh Git EOL checkout failed.'}
    $index=0;foreach($pinnedFile in $pinnedFiles){if((Get-FileHash -LiteralPath (Join-Path $fresh "tools/license-notices/LICENSE-pinned-$index.txt") -Algorithm SHA256).Hash.ToLowerInvariant() -cne $pinnedFile.sha256){throw 'Pinned license text changed with Windows Git EOL configuration.'};$index++}
    Write-Output 'License regression passed: selected normal/build closure; dev/platform exclusion; complete notice/text/EXE identity; ten native Mac dependency notices without embedded text; exact version/SPDX/VCS rejection; pinned fallback text/provenance tampering; fresh autocrlf checkout byte identity.'
}finally{
    Remove-Item Function:\cargo -ErrorAction SilentlyContinue
    $resolved=Assert-LicensePath $ownedRoot $parent;if(Test-Path -LiteralPath $resolved){Remove-Item -LiteralPath $resolved -Recurse -Force}
}
