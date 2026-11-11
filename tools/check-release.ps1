# tools/check-release.ps1
# Release version-policy check (see doc/Rust_Versioning.md). Read-only: never creates tags or modifies files.
#   ./tools/check-release.ps1 -Tag v0.4.0 [-BaseTag v0.3.7] [-Root <repo>] [-ReleaseTitle <title> -Prerelease <true|false>] [-CheckGitHubRelease] [-Repo owner/name]
# Naming rule: title must be exactly 'AudioLib Explorer <tag>'; prerelease iff the tag has a -rc.N/-beta.N suffix.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [string]$BaseTag,
    [string]$Root = (Split-Path -Parent $PSScriptRoot),
    [string]$ReleaseTitle,
    [Nullable[bool]]$Prerelease,
    [switch]$CheckGitHubRelease,
    [string]$Repo = 'Freedragonlancer/AudioLib-Explorer'
)
$ErrorActionPreference = 'Stop'
$Root = [IO.Path]::GetFullPath($Root)
$errors = [Collections.Generic.List[string]]::new()
function Fail([string]$Message) { $script:errors.Add($Message) }
function Read-Utf8([string]$Relative, [switch]$Optional) {
    $path = Join-Path $Root $Relative
    if (!(Test-Path -LiteralPath $path)) { if (!$Optional) { Fail "Missing required file: $Relative" }; return $null }
    [IO.File]::ReadAllText($path, [Text.Encoding]::UTF8)
}
function Get-Schema([string]$Text, [string]$Compatibility, [switch]$LegacyBase) {
    if ($null -eq $Text) { return $null }
    $cur = [regex]::Matches($Text, '(?m)^\s*pub\s+const\s+SCHEMA_VERSION\s*:\s*i64\s*=\s*(\d+)\s*;')
    $min = [regex]::Matches($Text, '(?m)^\s*pub\s+const\s+MIN_SNAPSHOT_SCHEMA_VERSION\s*:\s*i64\s*=\s*(\d+)\s*;')
    if ($cur.Count -ne 1 -or $min.Count -gt 1 -or ($min.Count -eq 0 -and !$LegacyBase)) { return $null }
    $local = [int]$cur[0].Groups[1].Value
    $facts = @{}
    foreach ($name in 'TARGET_SCHEMA','SERVER_TARGET_SCHEMA','SNAPSHOT_SCHEMA_MAX') {
        $matches = [regex]::Matches([string]$Compatibility, "(?m)^\s*pub\s+const\s+$name\s*:\s*u32\s*=\s*(\d+)\s*;")
        $declared = [regex]::IsMatch([string]$Compatibility, "(?m)^\s*pub\s+const\s+$name\b")
        if ($matches.Count -gt 1 -or ($declared -and $matches.Count -ne 1)) { return $null }
        $facts[$name] = if ($matches.Count -eq 1) { [int]$matches[0].Groups[1].Value } else { $null }
    }
    $modern = $null -ne $facts.SERVER_TARGET_SCHEMA -or
        [regex]::IsMatch([string]$Compatibility, '\bfn\s+server_schema\d+\s*\(')
    if (!$LegacyBase -and $modern -and
        ($null -eq $facts.TARGET_SCHEMA -or $null -eq $facts.SERVER_TARGET_SCHEMA -or $null -eq $facts.SNAPSHOT_SCHEMA_MAX)) { return $null }
    if ($null -ne $facts.TARGET_SCHEMA -and $facts.TARGET_SCHEMA -ne $local) { return $null }
    # Published older tags had no dedicated server target; it was their ordinary
    # local target. Recognition bounds are deliberately not migration targets.
    $server = if ($null -ne $facts.SERVER_TARGET_SCHEMA) { $facts.SERVER_TARGET_SCHEMA } else { $local }
    $maximum = if ($null -ne $facts.SNAPSHOT_SCHEMA_MAX) { $facts.SNAPSHOT_SCHEMA_MAX } else { $local }
    $minimum = if ($min.Count -eq 1) { [int]$min[0].Groups[1].Value } else { $local }
    [pscustomobject]@{ Current=$local; Server=$server; Min=$minimum; Max=$maximum; Modern=$modern }
}
function Read-BaseFile([string]$Revision, [string]$Relative, [switch]$Optional) {
    $savedPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $lines = @(& git -c "safe.directory=$Root" -C $Root show "${Revision}:$Relative" 2>$null)
        if ($LASTEXITCODE -eq 0) { return ($lines -join "`n") }
        if (!$Optional) { Fail "Cannot read '$Relative' from BaseTag '$Revision'." }
        return $null
    } finally { $ErrorActionPreference = $savedPreference }
}
function Get-CargoVersion([string]$Text) {
    if ($null -eq $Text) { return $null }
    $m = [regex]::Match($Text, '(?ms)^\[workspace\.package\]\s*(?:(?!^\[).)*?^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"')
    if ($m.Success) { [pscustomobject]@{ Major = [int]$m.Groups[1].Value; Minor = [int]$m.Groups[2].Value; Patch = [int]$m.Groups[3].Value; Text = "$($m.Groups[1].Value).$($m.Groups[2].Value).$($m.Groups[3].Value)" } }
}

# 1. Tag format: vX.Y.Z, vX.Y.Z-rc.N or vX.Y.Z-beta.N
$tagMatch = [regex]::Match($Tag, '^v(\d+)\.(\d+)\.(\d+)(?<suffix>-(?:rc|beta)\.\d+)?$')
if (!$tagMatch.Success) { Fail "Tag '$Tag' must look like vX.Y.Z, vX.Y.Z-rc.N or vX.Y.Z-beta.N." }
$tagCore = if ($tagMatch.Success) { "$($tagMatch.Groups[1].Value).$($tagMatch.Groups[2].Value).$($tagMatch.Groups[3].Value)" }

# 2. Program version (workspace Cargo.toml) must equal the tag
$program = Get-CargoVersion (Read-Utf8 'Cargo.toml')
if (!$program) { Fail 'Cannot read MAJOR.MINOR.PATCH from [workspace.package] version in Cargo.toml.' }
elseif ($tagCore -and $program.Text -ne $tagCore) { Fail "Tag $Tag does not match Cargo.toml workspace version $($program.Text) (expected tag v$($program.Text), v$($program.Text)-rc.N or v$($program.Text)-beta.N)." }

# 2b. Release naming: title == 'AudioLib Explorer <tag>', tag core == Cargo version, prerelease iff suffixed tag
$expectedTitle = "AudioLib Explorer $Tag"
$expectPrerelease = $tagMatch.Success -and $tagMatch.Groups['suffix'].Success
$checkTitle = $PSBoundParameters.ContainsKey('ReleaseTitle')
if ($CheckGitHubRelease) {
    $rel = $null
    $apiError = $null
    $ghError = $null
    $ghErrFile = [IO.Path]::GetTempFileName()
    try {
        $json = gh release view $Tag -R $Repo --json name,isPrerelease 2>$ghErrFile
        if ($LASTEXITCODE -eq 0 -and $json) { $r = $json | ConvertFrom-Json; $rel = @{ Name = [string]$r.name; Pre = [bool]$r.isPrerelease } }
        else { $ghError = "exit $LASTEXITCODE" }
    } catch { $rel = $null; $ghError = $_.Exception.Message }
    finally {
        # gh never prints the token to stderr; still redact anything token-shaped before reporting.
        $errText = if (Test-Path -LiteralPath $ghErrFile) { (Get-Content -LiteralPath $ghErrFile -Raw) } else { $null }
        Remove-Item -LiteralPath $ghErrFile -Force -ErrorAction SilentlyContinue
        if ($errText) { $ghError = (($errText.Trim() -replace '(gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+)', '<redacted>') -replace '\s+', ' ') }
    }
    if (!$rel) {
        try {
            $uri = "https://api.github.com/repos/$Repo/releases/tags/$([uri]::EscapeDataString($Tag))"
            $r = Invoke-RestMethod -Uri $uri -Headers @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'check-release.ps1' } -ErrorAction Stop
            $rel = @{ Name = [string]$r.name; Pre = [bool]$r.prerelease }
        } catch {
            $rel = $null
            $resp = $_.Exception.Response
            if ($resp -and $resp.StatusCode) { $apiError = "HTTP $([int]$resp.StatusCode)" } else { $apiError = $_.Exception.Message }
        }
    }
    if ($rel) { $ReleaseTitle = $rel.Name; $Prerelease = $rel.Pre; $checkTitle = $true }
    else {
        $detail = if ($apiError) { "anonymous GitHub API: $apiError" } else { 'not found' }
        Fail "GitHub release $Tag could not be read from $Repo (gh release view failed: $ghError; $detail); title/prerelease checks skipped."
        $checkTitle = $false; $Prerelease = $null
    }
}
if ($checkTitle) {
    if ($ReleaseTitle -cne $expectedTitle) { Fail "Release title '$ReleaseTitle' must be exactly '$expectedTitle'." }
    $titleCore = [regex]::Match([string]$ReleaseTitle, '^AudioLib Explorer v(\d+\.\d+\.\d+)')
    if ($program -and $titleCore.Success -and $titleCore.Groups[1].Value -ne $program.Text) { Fail "Release title version $($titleCore.Groups[1].Value) does not match Cargo.toml workspace version $($program.Text)." }
}
if ($null -ne $Prerelease -and [bool]$Prerelease -ne $expectPrerelease) { Fail "Release prerelease flag is $Prerelease but tag $Tag requires prerelease=$expectPrerelease (suffixed tags are prereleases, plain vX.Y.Z is not)." }

# 3. Schema version
$schema = Get-Schema (Read-Utf8 'crates/audiolib-db/src/schema.rs') (Read-Utf8 'crates/audiolib-core/src/compatibility.rs' -Optional)
if (!$schema) { Fail 'Cannot read unambiguous local/server write targets and snapshot min/max from schema.rs / compatibility.rs.' }
elseif ($schema.Min -lt 1 -or $schema.Min -gt $schema.Max -or $schema.Current -lt $schema.Min -or $schema.Current -gt $schema.Max -or $schema.Server -lt $schema.Min -or $schema.Server -gt $schema.Max) {
    Fail "Invalid snapshot range $($schema.Min)..=$($schema.Max) for local schema $($schema.Current) / server schema $($schema.Server)."
}

# 4. API version: every /api/vN used by remote client/server code must be the same single version
$apiVersions = @()
foreach ($dir in 'server', 'crates/audiolib-remote') {
    $full = Join-Path $Root $dir
    if (Test-Path -LiteralPath $full) {
        foreach ($f in Get-ChildItem -LiteralPath $full -Recurse -Filter *.rs -File) {
            $apiVersions += [regex]::Matches([IO.File]::ReadAllText($f.FullName), '/api/v(\d+)\b') | ForEach-Object { $_.Groups[1].Value }
        }
    }
}
$apiVersions = @($apiVersions | Sort-Object -Unique)
if ($apiVersions.Count -ne 1) { Fail "Expected exactly one API version in server/remote sources, found: $($apiVersions -join ', ')." }
$api = if ($apiVersions.Count -eq 1) { "v$($apiVersions[0])" }

# 5. Schema change vs base tag => at least MINOR bump
# The withdrawn v0.8.0 Release keeps its tag. Only the 0.8.1
# series uses the last actually published release; explicit -BaseTag always wins.
if (!$BaseTag -and $tagCore -ceq '0.8.1') {
    $BaseTag = 'v0.7.8'
    Write-Host '0.8.1 release baseline: v0.7.8 (withdrawn v0.8.0 tag remains unchanged).'
}
if (!$BaseTag) {
    $candidates = @(git -c "safe.directory=$Root" -C $Root tag --list 'v*' --sort=-v:refname | Where-Object { $_ -ne $Tag -and $_ -match '^v\d+\.\d+\.\d+$' })
    if ($LASTEXITCODE -ne 0) { Fail 'git tag --list failed.' }
    if ($candidates.Count -gt 0) { $BaseTag = $candidates[0] }
}
$schemaChanged = $null
if (!$BaseTag) { Write-Warning 'No previous v* tag found; schema-change check skipped.' }
else {
    $baseSchemaText = Read-BaseFile $BaseTag 'crates/audiolib-db/src/schema.rs'
    $baseCargoText = Read-BaseFile $BaseTag 'Cargo.toml'
    $baseCompatibilityText = Read-BaseFile $BaseTag 'crates/audiolib-core/src/compatibility.rs' -Optional
    if (!$baseSchemaText -or !$baseCargoText) { Fail "Cannot read BaseTag '$BaseTag' (fetch tags first: git fetch --tags)." }
    else {
        $baseSchema = Get-Schema $baseSchemaText $baseCompatibilityText -LegacyBase
        $baseProgram = Get-CargoVersion $baseCargoText
        if (!$baseSchema -or !$baseProgram) { Fail "Cannot read schema/program version from BaseTag '$BaseTag'." }
        elseif ($schema -and $program) {
            $schemaChanged = ($schema.Current -ne $baseSchema.Current -or $schema.Server -ne $baseSchema.Server)
            if ($schemaChanged) {
                $minorBumped = ($program.Major -gt $baseProgram.Major) -or ($program.Major -eq $baseProgram.Major -and $program.Minor -gt $baseProgram.Minor)
                if (!$minorBumped) { Fail "Writable schema target changed (local $($baseSchema.Current) -> $($schema.Current), server $($baseSchema.Server) -> $($schema.Server) since $BaseTag) but program version is $($program.Text) (base $($baseProgram.Text)); bump at least MINOR." }
            }
            if (($program.Major -lt $baseProgram.Major) -or ($program.Major -eq $baseProgram.Major -and ($program.Minor -lt $baseProgram.Minor -or ($program.Minor -eq $baseProgram.Minor -and $program.Patch -lt $baseProgram.Patch)))) {
                Fail "Program version $($program.Text) is lower than base tag $BaseTag ($($baseProgram.Text))."
            }
        }
    }
}

# 6. Release notes need one line with program, API, schema and snapshot range
$notes = Read-Utf8 'doc/Rust_Release_Notes.md'
if ($notes -and $program -and $schema -and $api) {
    $wantProgram = [regex]::Escape($program.Text)
    $wantApi = [regex]::Escape("/api/$api/")
    $wantSchema = "schema\s*v?$($schema.Current)(?!\d)"
    $wantRange = "(?:快照兼容|snapshot\s+compat(?:ibility)?)\s*$($schema.Min)\s*\.\.=?\s*$($schema.Max)(?!\d)"
    $wantLocal = "(?:本地(?:可写|写)|local(?:\s+(?:write|writable))?)\s*schema\s*v?$($schema.Current)(?!\d)"
    $wantServer = "(?:服务端(?:迁移|可写|写)|server(?:\s+(?:write|migration))?)\s*schema\s*v?$($schema.Server)(?!\d)"
    $modernNotes = $schema.Modern -or $schema.Server -ne $schema.Current -or $schema.Max -ne $schema.Current
    $ok = $false
    foreach ($line in ($notes -split "`r?`n")) {
        $targetsMatch = if ($modernNotes) { $line -imatch $wantLocal -and $line -imatch $wantServer } else { $line -imatch $wantSchema }
        if ($line -match "(?<![\d.])$wantProgram(?![\d.])" -and $line -match $wantApi -and $targetsMatch -and $line -imatch $wantRange) { $ok = $true; break }
    }
    if (!$ok) { Fail "doc/Rust_Release_Notes.md has no single line naming program $($program.Text), API /api/$api/, local schema $($schema.Current), server schema $($schema.Server) and snapshot range $($schema.Min)..=$($schema.Max). Example: 'Versions: program $($program.Text) | API /api/$api/ | local schema $($schema.Current) | server schema $($schema.Server) | snapshot compat $($schema.Min)..=$($schema.Max)'." }
}

if ($errors.Count -gt 0) {
    foreach ($e in $errors) { Write-Host "ERROR: $e" }
    throw "Release check failed for $Tag with $($errors.Count) error(s)."
}
$changedText = if ($null -eq $schemaChanged) { 'n/a' } else { $schemaChanged }
Write-Host "Release check passed: $Tag program=$($program.Text) api=$api schema=$($schema.Current) serverSchema=$($schema.Server) snapshot=$($schema.Min)..=$($schema.Max) base=$BaseTag schemaChanged=$changedText"
