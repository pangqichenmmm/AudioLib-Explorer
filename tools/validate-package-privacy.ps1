# tools/validate-package-privacy.ps1
# Release boundary check: explicit payload files, public report schema and local
# build-path removal. This is not a scan of historical Git objects or releases.
[CmdletBinding()]
param(
    [string]$BinaryDirectory = 'dist/latest/portable',
    [string]$ReleaseReport,
    # Only immediately after package-portable.ps1 has fully scanned this payload.
    # Recheck names, links, update markers and EXE identity without rereading all
    # payload contents; the new report still receives its full validation.
    [switch]$ReportOnly
)
$ErrorActionPreference = 'Stop'
if ($ReportOnly -and !$ReleaseReport) { throw '-ReportOnly requires -ReleaseReport and a payload just validated by package-portable.ps1.' }
. (Join-Path $PSScriptRoot 'console-utf8.ps1')
try {
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$payloadRoot = [IO.Path]::GetFullPath($(if ([IO.Path]::IsPathRooted($BinaryDirectory)) { $BinaryDirectory } else { Join-Path $taskRoot $BinaryDirectory }))
$allowedFiles = @('AudioLibExplorer.exe', 'AudioLibCli.exe', 'Start-AudioLibExplorer.cmd', 'update-channel.txt', 'update-version.txt') +
    # Windows PowerShell 5.1 emits a JSON array as one Object[]; unroll it explicitly.
    @((Get-Content -LiteralPath (Join-Path $PSScriptRoot 'portable-files.json') -Raw | ConvertFrom-Json) | ForEach-Object { $_ })
# Checksum sidecars written by package-portable.ps1 next to the executables: allowed, not
# required (a published archive does not carry them).
$optionalFiles = @('AudioLibExplorer.exe.sha256', 'AudioLibCli.exe.sha256')
$privatePrefixes = [Collections.Generic.List[string]]::new()
foreach ($prefix in @($taskRoot, $env:USERPROFILE, $env:CARGO_HOME, $env:RUSTUP_HOME, $env:CARGO_TARGET_DIR)) {
    if ($prefix) {
        $privatePrefixes.Add([IO.Path]::GetFullPath($prefix).TrimEnd('\', '/'))
        $privatePrefixes.Add([IO.Path]::GetFullPath($prefix).Replace('\', '/').TrimEnd('/'))
    }
}
$workspaceDrive = Get-PSDrive -Name ([IO.Path]::GetPathRoot($taskRoot).TrimEnd(':', '\')) -ErrorAction SilentlyContinue
if ($workspaceDrive.DisplayRoot) {
    $privatePrefixes.Add($workspaceDrive.DisplayRoot.TrimEnd('\') + '\' + $taskRoot.Substring(3))
}
function Assert-NoPrivatePath([string]$Text, [string]$Name) {
    foreach ($prefix in $privatePrefixes) {
        if ($Text.IndexOf($prefix, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
            $Text.IndexOf($prefix.Replace('\', '\\'), [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "Local build/user path found in public payload: $Name" }
    }
}
# Example values that look like real deployment details must not ship in the payload.
# Public licenses may legitimately name people, so the name check skips licenses/.
$placeholderPatterns = @(
    @{ Pattern = '(?<![0-9])18086(?![0-9])'; Label = 'private example port 18086'; Licenses = $true },
    @{ Pattern = '(?<![0-9.])192\.168\.[0-9]{1,3}\.[0-9]{1,3}(?![0-9]|/[0-9])'; Label = 'private LAN address 192.168.x.x (CIDR ranges allowed)'; Licenses = $true },
    @{ Pattern = '\bPaul\b'; Label = 'personal-name placeholder'; Licenses = $false }
)
function Assert-NoPrivatePlaceholder([string]$Text, [string]$Name, [bool]$IsLicense) {
    foreach ($check in $placeholderPatterns) {
        if ($IsLicense -and !$check.Licenses) { continue }
        if ([regex]::IsMatch($Text, $check.Pattern, [Text.RegularExpressions.RegexOptions]::CultureInvariant)) { throw "Private-looking example value ($($check.Label)) found in public payload: $Name" }
    }
}
if (!(Test-Path -LiteralPath $payloadRoot -PathType Container)) { throw 'Portable payload is missing.' }
foreach ($name in @('update-channel.txt', 'update-version.txt')) {
    if (!(Test-Path -LiteralPath (Join-Path $payloadRoot $name) -PathType Leaf)) { throw 'Portable update metadata is missing.' }
}
$updateChannel = (Get-Content -LiteralPath (Join-Path $payloadRoot 'update-channel.txt') -Raw).Trim()
$updateVersion = (Get-Content -LiteralPath (Join-Path $payloadRoot 'update-version.txt') -Raw).Trim()
if ($updateChannel -cnotin @('stable', 'beta') -or $updateVersion -cnotmatch '^\d+\.\d+\.\d+(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$' -or
    ($updateChannel -ceq 'stable' -and $updateVersion.Contains('-'))) { throw 'Portable update metadata is invalid.' }
if ((Get-Item -LiteralPath $payloadRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint -or
    @(Get-ChildItem -LiteralPath $payloadRoot -Recurse -Force -Attributes ReparsePoint).Count -gt 0) { throw 'Linked payload is not supported.' }
foreach ($file in (Get-ChildItem -LiteralPath $payloadRoot -Recurse -Force -File)) {
    # Windows PowerShell 5.1 (.NET Framework) lacks [IO.Path]::GetRelativePath; every
    # enumerated file sits below $payloadRoot, so strip that prefix instead.
    $relative = $file.FullName.Substring($payloadRoot.TrimEnd('\', '/').Length).TrimStart('\', '/').Replace('\', '/')
    if ($relative -notin $allowedFiles -and $relative -notin $optionalFiles -and !$relative.StartsWith('licenses/', [StringComparison]::Ordinal)) { throw "File is outside the public payload allowlist: $relative" }
    if ($file.Extension -in @('.db', '.sqlite', '.sqlite3', '.log') -or $file.Name -in @('settings.json', '.env')) { throw "Runtime state found in payload: $relative" }
    if ($ReportOnly) { continue }
    $bytes = [IO.File]::ReadAllBytes($file.FullName)
    Assert-NoPrivatePath ([Text.Encoding]::UTF8.GetString($bytes)) $relative
    $isLicense = $relative.StartsWith('licenses/', [StringComparison]::Ordinal)
    Assert-NoPrivatePlaceholder ([Text.Encoding]::UTF8.GetString($bytes)) $relative $isLicense
    if ($file.Extension -eq '.exe') {
        Assert-NoPrivatePath ([Text.Encoding]::Unicode.GetString($bytes)) $relative
        Assert-NoPrivatePlaceholder ([Text.Encoding]::Unicode.GetString($bytes)) $relative $false
        if ($bytes.Length -gt 1) { Assert-NoPrivatePath ([Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1)) $relative }
        if ($bytes.Length -gt 1) { Assert-NoPrivatePlaceholder ([Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1)) $relative $false }
    }
}
foreach ($file in $allowedFiles) {
    if (!(Test-Path -LiteralPath (Join-Path $payloadRoot $file) -PathType Leaf)) { throw "Required public payload file missing: $file" }
}
if ($ReleaseReport) {
    $reportText = Get-Content -LiteralPath $ReleaseReport -Raw -Encoding UTF8
    Assert-NoPrivatePath $reportText 'release report'
    $report = $reportText | ConvertFrom-Json
    $fields = @('success','tag','commit','version','checked_at','checks','binaries','portable')
    if (@($report.PSObject.Properties.Name | Where-Object { $_ -notin $fields }).Count -or
        @($fields | Where-Object { $_ -notin $report.PSObject.Properties.Name }).Count) { throw 'Public release report schema mismatch.' }
    $portableFields = @('success','mode','records','source_unchanged','python_runtime_required')
    if (@($report.portable.PSObject.Properties.Name | Where-Object { $_ -notin $portableFields }).Count -or
        @($portableFields | Where-Object { $_ -notin $report.portable.PSObject.Properties.Name }).Count) { throw 'Public portable report schema mismatch.' }
    if ($report.portable.mode -isnot [string]) { throw 'Public portable report has invalid validation mode.' }
    if ($report.portable.mode -ceq 'clean-checkout-synthetic') {
        if (!$report.portable.success -or !$report.portable.source_unchanged -or
            $report.portable.records -ne 1 -or $report.portable.python_runtime_required) { throw 'Public portable report has invalid validation results.' }
    } elseif ($report.portable.mode -ceq 'not-run') {
        foreach ($field in @('success','records','source_unchanged','python_runtime_required')) {
            if ($null -ne $report.portable.$field) { throw 'Unrun portable validation must contain only null results.' }
        }
    } else { throw 'Public portable report has invalid validation mode.' }
    if (!$report.success -or $report.commit -notmatch '^[0-9a-f]{40}$' -or
        $report.version -notmatch '^\d+\.\d+\.\d+$' -or $report.tag -notmatch ('^v' + [regex]::Escape($report.version) + '(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$') -or
        $updateVersion -cne $report.tag.Substring(1)) { throw 'Public release report has invalid build/validation identity.' }
    if (@($report.binaries.PSObject.Properties.Name | Sort-Object) -join ',' -ne 'AudioLibCli.exe,AudioLibExplorer.exe') { throw 'Public binary report schema mismatch.' }
    foreach ($binary in @('AudioLibExplorer.exe', 'AudioLibCli.exe')) {
        if ($report.binaries.$binary -cne (Get-FileHash -LiteralPath (Join-Path $payloadRoot $binary) -Algorithm SHA256).Hash) { throw "Release report binary identity mismatch: $binary" }
    }
}
if ($ReportOnly) {
    Write-Host 'Portable report passed: schema, privacy, update identity and EXE hashes; payload content scan reused from the preceding package step.'
} else {
    Write-Host 'Portable publication boundary passed: allowlisted files, no local build/user paths, no private example values, public report schema (when provided).'
}
} finally {
    Restore-AudioLibConsoleEncoding
}
