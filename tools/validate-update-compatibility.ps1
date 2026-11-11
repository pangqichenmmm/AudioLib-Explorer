# tools/validate-update-compatibility.ps1
# Freeze the file policy accepted by the supported old updater. The current
# publication manifests cannot prove compatibility with already-installed EXEs.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Desktop', 'Server')][string]$Product,
    [Parameter(Mandatory)][string[]]$Entries,
    [ValidateSet('audiolib-server', 'audiolib-server.exe')]
    [string]$ServerBinaryName = $(if ($env:OS -eq 'Windows_NT') { 'audiolib-server.exe' } else { 'audiolib-server' })
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$baseline = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'update-compatible-files.json') -Raw | ConvertFrom-Json
if ($baseline.schema_version -ne 1 -or $baseline.baseline_tag -cnotmatch '^v\d+\.\d+\.\d+$' -or
    $baseline.baseline_commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid update compatibility baseline identity.' }

function Assert-UpdateRelative([string]$Name) {
    if (!$Name -or $Name -match '[\\:\x00-\x1f\x7f-\x9f?*<>|\"]' -or
        @($Name.ToCharArray() | Where-Object { [char]::IsControl($_) }).Count) { throw "Unsafe update compatibility path: $Name" }
    foreach ($part in $Name.Split('/')) {
        if (!$part -or $part -in @('.', '..') -or $part.EndsWith('.') -or $part.EndsWith(' ') -or
            $part.Split('.')[0] -match '^(?:CON|PRN|AUX|NUL|CONIN\$|CONOUT\$|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3])$') {
            throw "Unsafe update compatibility path: $Name"
        }
    }
}

$publicFiles = @($baseline.($Product.ToLowerInvariant()))
if (!$publicFiles.Count -or @($publicFiles | Where-Object { $_ -isnot [string] }).Count) { throw 'Invalid update compatibility baseline file list.' }
$allowed = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($name in $publicFiles) {
    Assert-UpdateRelative $name
    if (!$allowed.Add($name)) { throw 'Duplicate update compatibility baseline file.' }
}
foreach ($name in @('update-channel.txt', 'update-version.txt')) { $null = $allowed.Add($name) }
if ($Product -eq 'Desktop') {
    foreach ($name in @('AudioLibExplorer.exe', 'AudioLibCli.exe', 'Start-AudioLibExplorer.cmd')) { $null = $allowed.Add($name) }
} else {
    $null = $allowed.Add($ServerBinaryName)
}
$blockedLicenseSuffixes = @('.exe', '.dll', '.cmd', '.bat', '.ps1', '.sh', '.db', '.sqlite', '.wav', '.log',
    '.sqlite3', '.db-wal', '.db-shm', '.sqlite-wal', '.sqlite-shm', '.sqlite3-wal', '.sqlite3-shm',
    'settings.json', '.server-state.json')
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
if (!$Entries.Count -or $Entries.Count -gt 20000) { throw 'Invalid update compatibility entry count.' }
foreach ($name in $Entries) {
    Assert-UpdateRelative $name
    if (!$seen.Add($name)) { throw "Duplicate update compatibility entry: $name" }
    if ($allowed.Contains($name)) { continue }
    if ($name.StartsWith('licenses/', [StringComparison]::Ordinal)) {
        $lower = $name.ToLowerInvariant()
        $blocked = @($blockedLicenseSuffixes | Where-Object { $lower.EndsWith($_, [StringComparison]::Ordinal) }).Count
        if (!$blocked -and $lower.Split('/')[-1] -cnotin @('tokens.json', '.env')) { continue }
    }
    throw "Update compatibility with $($baseline.baseline_tag) failed: Non-public update file: $name"
}
Write-Host "Update compatibility passed: $Product payload accepted by $($baseline.baseline_tag) file policy ($($Entries.Count) files)."
