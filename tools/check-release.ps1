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
function Read-Utf8([string]$Relative) {
    $path = Join-Path $Root $Relative
    if (!(Test-Path -LiteralPath $path)) { Fail "Missing required file: $Relative"; return $null }
    [IO.File]::ReadAllText($path, [Text.Encoding]::UTF8)
}
function Get-Schema([string]$Text, [switch]$MinOptional) {
    if ($null -eq $Text) { return $null }
    $cur = [regex]::Match($Text, '(?m)^\s*pub\s+const\s+SCHEMA_VERSION\s*:\s*i64\s*=\s*(\d+)\s*;')
    $min = [regex]::Match($Text, '(?m)^\s*pub\s+const\s+MIN_SNAPSHOT_SCHEMA_VERSION\s*:\s*i64\s*=\s*(\d+)\s*;')
    if (!$cur.Success -or (!$min.Success -and !$MinOptional)) { return $null }
    $minValue = if ($min.Success) { [int]$min.Groups[1].Value } else { [int]$cur.Groups[1].Value }
    [pscustomobject]@{ Current = [int]$cur.Groups[1].Value; Min = $minValue }
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
$schema = Get-Schema (Read-Utf8 'crates/audiolib-db/src/schema.rs')
if (!$schema) { Fail 'Cannot read SCHEMA_VERSION / MIN_SNAPSHOT_SCHEMA_VERSION from crates/audiolib-db/src/schema.rs.' }
elseif ($schema.Min -gt $schema.Current) { Fail "MIN_SNAPSHOT_SCHEMA_VERSION ($($schema.Min)) is greater than SCHEMA_VERSION ($($schema.Current))." }

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
    $candidates = @(git -C $Root tag --list 'v*' --sort=-v:refname | Where-Object { $_ -ne $Tag -and $_ -match '^v\d+\.\d+\.\d+$' })
    if ($LASTEXITCODE -ne 0) { Fail 'git tag --list failed.' }
    if ($candidates.Count -gt 0) { $BaseTag = $candidates[0] }
}
$schemaChanged = $null
if (!$BaseTag) { Write-Warning 'No previous v* tag found; schema-change check skipped.' }
else {
    $baseSchemaText = git -C $Root show "${BaseTag}:crates/audiolib-db/src/schema.rs" 2>$null
    $baseCargoText = git -C $Root show "${BaseTag}:Cargo.toml" 2>$null
    if ($LASTEXITCODE -ne 0 -and !$baseSchemaText) { Fail "Cannot read BaseTag '$BaseTag' (fetch tags first: git fetch --tags)." }
    else {
        $baseSchema = Get-Schema ($baseSchemaText -join "`n") -MinOptional
        $baseProgram = Get-CargoVersion ($baseCargoText -join "`n")
        if (!$baseSchema -or !$baseProgram) { Fail "Cannot read schema/program version from BaseTag '$BaseTag'." }
        elseif ($schema -and $program) {
            $schemaChanged = ($schema.Current -ne $baseSchema.Current)
            if ($schemaChanged) {
                $minorBumped = ($program.Major -gt $baseProgram.Major) -or ($program.Major -eq $baseProgram.Major -and $program.Minor -gt $baseProgram.Minor)
                if (!$minorBumped) { Fail "SCHEMA_VERSION changed ($($baseSchema.Current) -> $($schema.Current) since $BaseTag) but program version is $($program.Text) (base $($baseProgram.Text)); bump at least MINOR." }
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
    $wantRange = "$($schema.Min)\s*\.\.=?\s*$($schema.Current)(?!\d)"
    $ok = $false
    foreach ($line in ($notes -split "`r?`n")) {
        if ($line -match "(?<![\d.])$wantProgram(?![\d.])" -and $line -match $wantApi -and $line -imatch $wantSchema -and $line -match $wantRange) { $ok = $true; break }
    }
    if (!$ok) { Fail "doc/Rust_Release_Notes.md has no single line naming program $($program.Text), API /api/$api/, schema $($schema.Current) and snapshot range $($schema.Min)..=$($schema.Current). Example: 'Versions: program $($program.Text) | API /api/$api/ | schema $($schema.Current) | snapshot compat $($schema.Min)..=$($schema.Current)'." }
}

if ($errors.Count -gt 0) {
    foreach ($e in $errors) { Write-Host "ERROR: $e" }
    throw "Release check failed for $Tag with $($errors.Count) error(s)."
}
$changedText = if ($null -eq $schemaChanged) { 'n/a' } else { $schemaChanged }
Write-Host "Release check passed: $Tag program=$($program.Text) api=$api schema=$($schema.Current) snapshot=$($schema.Min)..=$($schema.Current) base=$BaseTag schemaChanged=$changedText"