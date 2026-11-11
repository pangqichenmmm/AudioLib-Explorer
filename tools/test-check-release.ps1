# tools/test-check-release.ps1
# Exercise release policy through real checker invocations and isolated synthetic Git tags.
# No Cargo, AudioLib binary, publication or tag in the application repository is used.
#Requires -Version 7.3
[CmdletBinding()]param([string]$EvidenceDirectory)
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
$projectRoot=Split-Path -Parent $PSScriptRoot
$checker=Join-Path $PSScriptRoot 'check-release.ps1'
$ownedName='AudioLibReleasePolicy-'+[guid]::NewGuid().ToString('N')
$ownedRoot=Join-Path ([IO.Path]::GetTempPath()) $ownedName
$repo=Join-Path $ownedRoot 'repo'
if(Test-Path -LiteralPath $ownedRoot){throw 'Synthetic policy fixture must be new.'}
$null=New-Item -ItemType Directory -Path $repo
if(!$EvidenceDirectory){$EvidenceDirectory=Join-Path $projectRoot ('.rust-validation/release-policy/'+$ownedName)}
$evidenceRoot=[IO.Path]::GetFullPath($EvidenceDirectory)
if(Test-Path -LiteralPath $evidenceRoot){throw 'Policy evidence directory must be new.'}
$null=New-Item -ItemType Directory -Path $evidenceRoot
$results=[Collections.Generic.List[object]]::new()
$utf8=[Text.UTF8Encoding]::new($false)
function Write-Fixture([string]$Relative,[string]$Text){
    $path=[IO.Path]::GetFullPath((Join-Path $repo $Relative))
    if(!$path.StartsWith([IO.Path]::GetFullPath($repo)+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Synthetic file escaped its owned repository.'}
    $null=[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
    [IO.File]::WriteAllText($path,$Text,$utf8)
}
function Git-Fixture([string[]]$Arguments){
    $output=& git -c "safe.directory=$repo" -c user.name=ReleasePolicyTest -c user.email=release-policy@example.invalid -C $repo @Arguments 2>&1
    if($LASTEXITCODE -ne 0){throw ('Synthetic Git operation failed: '+($output|Out-String))}
}
function Schema-Text([int]$Local){"pub const SCHEMA_VERSION: i64 = $Local;`npub const MIN_SNAPSHOT_SCHEMA_VERSION: i64 = 3;`n"}
function Compatibility-Text([int]$Local,[int]$Server,[int]$Maximum,[string]$Variant='normal'){
    $text="pub const TARGET_SCHEMA: u32 = $Local;`npub const SNAPSHOT_SCHEMA_MAX: u32 = $Maximum;`n"
    if($Variant -cne 'legacy' -and $Variant -cne 'missing-server'){$text+="pub const SERVER_TARGET_SCHEMA: u32 = $Server;`n"}
    if($Variant -ceq 'duplicate-server'){$text+="pub const SERVER_TARGET_SCHEMA: u32 = $Server;`n"}
    if($Variant -ceq 'malformed-max'){$text=$text.Replace("SNAPSHOT_SCHEMA_MAX: u32 = $Maximum;","SNAPSHOT_SCHEMA_MAX: u32 = $Maximum + 0;")}
    if($Variant -ceq 'missing-server'){$text+="pub fn server_schema9() -> Self { format.schema_version = 9; }`n"}
    $text
}
function Case([string]$Name,[string]$Version,[int]$Local,[int]$Server,[int]$Maximum,[string]$Notes,[bool]$Pass,[string]$Reason='',[string]$Variant='normal',[string]$Tag=''){
    Write-Fixture 'Cargo.toml' "[workspace.package]`nversion = `"$Version`"`n"
    Write-Fixture 'crates/audiolib-db/src/schema.rs' (Schema-Text $Local)
    Write-Fixture 'crates/audiolib-core/src/compatibility.rs' (Compatibility-Text $Local $Server $Maximum $Variant)
    Write-Fixture 'doc/Rust_Release_Notes.md' $Notes
    $stdout=Join-Path $evidenceRoot ($Name+'.stdout.log');$stderr=Join-Path $evidenceRoot ($Name+'.stderr.log')
    $start=[Diagnostics.ProcessStartInfo]::new();$start.FileName=Join-Path $PSHOME 'pwsh.exe';$start.UseShellExecute=$false;$start.CreateNoWindow=$true;$start.RedirectStandardOutput=$true;$start.RedirectStandardError=$true
    foreach($argument in @('-NoProfile','-File',$checker,'-Root',$repo,'-Tag',$(if($Tag){$Tag}else{'v'+$Version}),'-BaseTag','v0.8.7')){$start.ArgumentList.Add($argument)}
    $process=[Diagnostics.Process]::Start($start)
    try{$out=$process.StandardOutput.ReadToEndAsync();$err=$process.StandardError.ReadToEndAsync();$process.WaitForExit();$code=$process.ExitCode;$text=$out.GetAwaiter().GetResult();$errorText=$err.GetAwaiter().GetResult()}finally{$process.Dispose()}
    [IO.File]::WriteAllText($stdout,$text,$utf8);[IO.File]::WriteAllText($stderr,$errorText,$utf8)
    $actual=$code -eq 0
    $reasonMatches=!$Reason -or ($text+$errorText).Contains($Reason,[StringComparison]::OrdinalIgnoreCase)
    $results.Add([ordered]@{case=$Name;expected_pass=$Pass;exit_code=$code;reason_matches=$reasonMatches;passed=($actual -eq $Pass -and $reasonMatches)})
    if($actual -ne $Pass -or !$reasonMatches){throw "Release policy case failed: $Name; retained $ownedRoot and $evidenceRoot"}
}
$success=$false
try{
    Git-Fixture @('init','--quiet')
    Write-Fixture 'Cargo.toml' "[workspace.package]`nversion = `"0.8.7`"`n"
    Write-Fixture 'crates/audiolib-db/src/schema.rs' (Schema-Text 8)
    # Genuine older released shape: no SERVER_TARGET_SCHEMA, ordinary server target8.
    Write-Fixture 'crates/audiolib-core/src/compatibility.rs' (Compatibility-Text 8 8 8 'legacy')
    Write-Fixture 'server/protocol.rs' '// /api/v1/example'
    Write-Fixture 'doc/Rust_Release_Notes.md' 'Program 0.8.7 | API /api/v1/ | schema8 | snapshot compat3..=8'
    Git-Fixture @('add','--','.')
    Git-Fixture @('commit','--quiet','-m','synthetic released baseline')
    Git-Fixture @('tag','v0.8.7')
    Case 'legacy-patch' '0.8.8' 8 8 8 'Program0.8.8 | API /api/v1/ | schema8 | snapshot compat3..=8' $true '' 'legacy'
    Case 'server-only-change-rejects-patch' '0.8.8' 8 9 9 'Program0.8.8 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=9' $false 'bump at least MINOR'
    Case 'server-change-minor' '0.9.0' 8 9 9 'Program0.9.0 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=9' $true
    Case 'local-change-rejects-patch' '0.8.8' 9 9 9 'Program0.8.8 | API /api/v1/ | local schema9 | server schema9 | snapshot compat3..=9' $false 'bump at least MINOR'
    Case 'stale-snapshot-notes' '0.9.0' 8 9 9 "Program0.8.7 | API /api/v1/ | schema8 | snapshot compat3..=8`nProgram0.9.0 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=8" $false 'has no single line'
    Case 'missing-modern-server-target' '0.9.0' 8 9 9 'Program0.9.0 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=9' $false 'Cannot read unambiguous' 'missing-server'
    Case 'duplicate-server-target' '0.9.0' 8 9 9 'Program0.9.0 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=9' $false 'Cannot read unambiguous' 'duplicate-server'
    Case 'malformed-snapshot-maximum' '0.9.0' 8 9 9 'Program0.9.0 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=9' $false 'Cannot read unambiguous' 'malformed-max'
    Case 'read-maximum-only-is-not-write-change' '0.8.8' 8 8 9 'Program0.8.8 | API /api/v1/ | local schema8 | server schema8 | snapshot compat3..=9' $true 'schemaChanged=False'
    Case 'server-target-exceeds-reader' '0.9.0' 8 9 8 'Program0.9.0 | API /api/v1/ | local schema8 | server schema9 | snapshot compat3..=8' $false 'Invalid snapshot range'
    Case 'chinese-modern-notes-beta' '0.9.0' 8 9 9 '开发候选程序0.9.0 | API /api/v1/ | 本地写schema8 | 服务端迁移schema9 | 快照兼容3..=9' $true '' 'normal' 'v0.9.0-beta.1'
    Case 'base-version-regression' '0.8.6' 8 8 8 'Program0.8.6 | API /api/v1/ | schema8 | snapshot compat3..=8' $false 'lower than base tag' 'legacy'
    $success=$true
}finally{
    [ordered]@{success=$success;cases=@($results.ToArray());checker_sha256=(Get-FileHash -LiteralPath $checker -Algorithm SHA256).Hash.ToLowerInvariant();synthetic_repository=$repo;publication_or_application_repository_tags=$false;cargo_or_application_started=$false}|ConvertTo-Json -Depth 6|Set-Content -LiteralPath (Join-Path $evidenceRoot 'result.json') -Encoding utf8
}
[pscustomobject]@{success=$success;passed=$results.Count;evidence=(Join-Path $evidenceRoot 'result.json');cargo_or_application_started=$false}|ConvertTo-Json -Compress
