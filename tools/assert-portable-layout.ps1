# tools/assert-portable-layout.ps1
# Cheap installation preflight against the maintained package manifest. Content
# privacy and runtime acceptance still belong to the packaging/acceptance stages.
#Requires -Version 7.3
function Get-AudioLibPortableFiles([string]$BinaryDirectory) {
    $required = @('AudioLibExplorer.exe','AudioLibCli.exe','Start-AudioLibExplorer.cmd','update-channel.txt','update-version.txt') +
        @(Get-Content -LiteralPath (Join-Path $PSScriptRoot 'portable-files.json') -Raw | ConvertFrom-Json)
    $optional = @('AudioLibExplorer.exe.sha256','AudioLibCli.exe.sha256')
    $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $files = [Collections.Generic.List[object]]::new()
    foreach ($entry in Get-ChildItem -LiteralPath $BinaryDirectory -Recurse -Force) {
        if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Linked package entry.' }
        if ($entry.PSIsContainer) { continue }
        $relative = [IO.Path]::GetRelativePath($BinaryDirectory,$entry.FullName).Replace('\','/')
        if ($relative -cnotin $required -and $relative -cnotin $optional -and !$relative.StartsWith('licenses/',[StringComparison]::Ordinal)) { throw "File is outside the package allowlist: $relative" }
        if ($entry.Extension -in @('.db','.sqlite','.sqlite3','.log') -or $entry.Name -in @('settings.json','.env')) { throw 'Runtime state is not a package-owned file.' }
        $null = $names.Add($relative)
        $files.Add($entry)
    }
    foreach ($name in $required) { if (!$names.Contains($name)) { throw "Incomplete accepted package: $name" } }
    $channel = ([IO.File]::ReadAllText((Join-Path $BinaryDirectory 'update-channel.txt'))).Trim()
    $version = ([IO.File]::ReadAllText((Join-Path $BinaryDirectory 'update-version.txt'))).Trim()
    if ($channel -cnotin @('stable','beta') -or $version -cnotmatch '^\d+\.\d+\.\d+(?:-(?:rc|beta)\.(?:0|[1-9]\d*))?$' -or ($channel -ceq 'stable' -and $version.Contains('-'))) { throw 'Invalid package update identity.' }
    return $files.ToArray()
}
