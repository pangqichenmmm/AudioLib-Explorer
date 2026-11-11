# tools/build-macos-client.ps1
# Native Apple Silicon release build. The private proof is not a release asset.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$ExpectedCommit,
    [string]$ReportPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion -lt [version]'7.3' -or !$IsMacOS -or
    [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -ne [Runtime.InteropServices.Architecture]::Arm64) {
    throw 'Run with native arm64 PowerShell 7.3+ on an Apple Silicon Mac.'
}
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$taskTarget = 'aarch64-apple-darwin'
function Read-Tool([string]$Tool, [string[]]$Arguments) {
    $result = (& $Tool @Arguments 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "Native command failed: $Tool" }
    return $result
}
function Assert-Unlinked([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked build paths are refused.' }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if (!$parent -or $parent -eq $cursor) { break }
        $cursor = $parent
    }
}
function Assert-Source {
    if ((Read-Tool git @('-C', $taskRoot, 'rev-parse', 'HEAD')) -cne $ExpectedCommit -or
        (Read-Tool git @('-C', $taskRoot, 'status', '--porcelain', '--untracked-files=all'))) {
        throw 'Release requires a clean checkout at the exact expected commit.'
    }
}
function Quote-NativeFlag([string]$Flag) {
    $quote = [string][char]39
    return $quote + $Flag.Replace($quote, ($quote + [char]34 + $quote + [char]34 + $quote)) + $quote
}
Assert-Unlinked $taskRoot
Assert-Source
$taskCompiler = Read-Tool rustc @('-vV')
if ($taskCompiler -notmatch '(?m)^host: aarch64-apple-darwin$' -or $taskCompiler -notmatch '(?m)^release: 1\.97\.1$') {
    throw 'Use the pinned Rust 1.97.1 native aarch64-apple-darwin toolchain.'
}
$taskSdk = Read-Tool xcrun @('--sdk', 'macosx', '--show-sdk-version')
$taskVersionMatch = [regex]::Match([IO.File]::ReadAllText((Join-Path $taskRoot 'Cargo.toml')), '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"')
if (!$taskVersionMatch.Success) { throw 'Workspace version is missing.' }
$taskVersion = $taskVersionMatch.Groups[1].Value
$taskUserRoot = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
if (!$taskUserRoot) { throw 'Cannot identify the native home for privacy remapping.' }
$taskBuildRoot = [IO.Path]::GetFullPath($(if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $taskRoot 'target' }), $taskRoot)
Assert-Unlinked $taskBuildRoot
New-Item -ItemType Directory -Path $taskBuildRoot -Force | Out-Null
if (!$ReportPath) {
    $ReportPath = Join-Path $taskRoot ('.rust-validation/macos-client-build/' + $ExpectedCommit.Substring(0, 12) + '-' + [guid]::NewGuid().ToString('N') + '/build.json')
}
$ReportPath = [IO.Path]::GetFullPath($ReportPath, $taskRoot)
Assert-Unlinked $ReportPath
if (Test-Path -LiteralPath $ReportPath) { throw 'Build proof already exists; use a fresh report path.' }
$taskProofRoot = Split-Path -Parent $ReportPath
New-Item -ItemType Directory -Path $taskProofRoot -Force | Out-Null
$taskSaved = @{}
foreach ($key in @('CARGO_ENCODED_RUSTFLAGS', 'CXXFLAGS_aarch64-apple-darwin', 'CC_SHELL_ESCAPED_FLAGS', 'MACOSX_DEPLOYMENT_TARGET')) {
    $taskSaved[$key] = [Environment]::GetEnvironmentVariable($key)
}
$taskFlags = [Collections.Generic.List[string]]::new()
if ($taskSaved.CARGO_ENCODED_RUSTFLAGS) {
    foreach ($flag in $taskSaved.CARGO_ENCODED_RUSTFLAGS.Split([char]31)) { $taskFlags.Add($flag) }
} elseif ($env:RUSTFLAGS) {
    foreach ($flag in ($env:RUSTFLAGS -split '\s+' | Where-Object { $_ })) { $taskFlags.Add($flag) }
}
if (@($taskFlags | Where-Object { $_.Contains('+crt-static') }).Count) { throw 'macOS releases cannot use +crt-static.' }
$taskMappings = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
$taskMappings[$taskRoot] = '/audiolib/source'
$taskMappings[$taskBuildRoot] = '/audiolib/build'
$taskMappings[$taskUserRoot] = '/audiolib/user'
$taskMappings[[IO.Path]::GetFullPath($(if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $taskUserRoot '.cargo' }), $taskRoot)] = '/audiolib/cargo'
$taskMappings[[IO.Path]::GetFullPath($(if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $taskUserRoot '.rustup' }), $taskRoot)] = '/audiolib/rustup'
$taskNativeFlags = [Collections.Generic.List[string]]::new()
$taskNativeFlags.Add('-DNDEBUG')
foreach ($prefix in ($taskMappings.Keys | Sort-Object Length -Descending)) {
    $taskFlags.Add('--remap-path-prefix=' + $prefix + '=' + $taskMappings[$prefix])
    $taskNativeFlags.Add('-ffile-prefix-map=' + $prefix + '=' + $taskMappings[$prefix])
}
$taskArguments = @('build', '-p', 'audiolib-desktop', '-p', 'audiolib-cli', '--release', '--locked', '--no-default-features', '--target', $taskTarget)
$taskProof = [ordered]@{
    success=$false; commit=$ExpectedCommit; version=$taskVersion; target=$taskTarget; minimum_macos='11.0'; asio=$false
    rustc=$taskCompiler; sdk=$taskSdk; arguments=$taskArguments; source=$taskRoot; target_directory=$taskBuildRoot
    private_prefixes=@($taskMappings.Keys); metadata=(Join-Path $taskProofRoot 'cargo-metadata.json'); binaries=[ordered]@{}
}
$taskEntered = $false
$taskLock = $null
. (Join-Path $PSScriptRoot 'build-cache.ps1')
$buildLease = $null
try {
    $buildLease = Enter-AudioLibBuildLease $taskRoot
    $taskLock = [IO.File]::Open((Join-Path $taskBuildRoot '.audiolib-macos-client-release.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    Push-Location -LiteralPath $taskRoot
    $taskEntered = $true
    $env:CARGO_ENCODED_RUSTFLAGS = $taskFlags -join [char]31
    [Environment]::SetEnvironmentVariable('CXXFLAGS_aarch64-apple-darwin', (($taskSaved['CXXFLAGS_aarch64-apple-darwin'] + ' ' + (($taskNativeFlags | ForEach-Object { Quote-NativeFlag $_ }) -join ' ')).Trim()))
    $env:CC_SHELL_ESCAPED_FLAGS = '1'
    $env:MACOSX_DEPLOYMENT_TARGET = '11.0'
    & cargo @taskArguments 2>&1 | Tee-Object -FilePath (Join-Path $taskProofRoot 'build.log') | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'macOS desktop/CLI release build failed.' }
    $metadata = Read-Tool cargo @('metadata', '--format-version', '1', '--locked', '--no-default-features', '--filter-platform', $taskTarget)
    [IO.File]::WriteAllText($taskProof.metadata, $metadata, [Text.UTF8Encoding]::new($false))
    foreach ($name in @('AudioLibExplorer', 'AudioLibCli')) {
        $path = Join-Path $taskBuildRoot ($taskTarget + '/release/' + $name)
        Assert-Unlinked $path
        if ((Read-Tool /usr/bin/lipo @('-archs', $path)) -cne 'arm64' -or
            (Read-Tool $path @('--version')) -cne ($name + ' ' + $taskVersion)) { throw "Invalid native release identity: $name" }
        $taskProof.binaries[$name] = [ordered]@{ path=$path; sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
    }
    Assert-Source
    $taskProof.success = $true
} finally {
    try {
        foreach ($key in $taskSaved.Keys) { [Environment]::SetEnvironmentVariable($key, $taskSaved[$key]) }
        if ($taskEntered) { Pop-Location }
        if ($taskLock) { $taskLock.Dispose() }
        [IO.File]::WriteAllText($ReportPath, (($taskProof | ConvertTo-Json -Depth 8) + [char]10), [Text.UTF8Encoding]::new($false))
    } finally { if ($buildLease) { $buildLease.Dispose() } }
}
Write-Host "Native macOS client build passed. Private proof: $ReportPath"
