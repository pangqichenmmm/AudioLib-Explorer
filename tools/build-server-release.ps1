# tools/build-server-release.ps1
# Build the headless server on its native release host (Unix or Windows MSVC). Packaging and
# dependency installation belong to the caller; this script never installs a SDK.
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'aarch64-apple-darwin')]
    [string]$Target
)
$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
. (Join-Path $PSScriptRoot 'build-cache.ps1')
$buildLease = $null
$isWindowsTarget = $Target -ceq 'x86_64-pc-windows-msvc'
if ($isWindowsTarget -ne [bool]$IsWindows) { throw 'The server release target must be built on its native OS.' }
$compilerDetails = & rustc -vV
if ($LASTEXITCODE -ne 0) { throw 'Could not determine the installed Rust host.' }
$hostLine = @($compilerDetails | Where-Object { $_ -match '^host: ' })
if ($hostLine.Count -ne 1 -or $hostLine[0].Substring(6) -cne $Target) {
    throw 'The server release target must match the installed native Rust host.'
}

$encodedFlagKey = 'CARGO_ENCODED_RUSTFLAGS'
$nativeFlagKey = 'CXXFLAGS_' + $Target
$shellFlagKey = 'CC_SHELL_ESCAPED_FLAGS'
$savedEnvironment = @{}
foreach ($key in @($encodedFlagKey, $nativeFlagKey, $shellFlagKey)) {
    $savedEnvironment[$key] = [Environment]::GetEnvironmentVariable($key)
}
$flagSeparator = [char]31
$releaseFlags = [Collections.Generic.List[string]]::new()
if ($savedEnvironment[$encodedFlagKey]) {
    foreach ($flag in $savedEnvironment[$encodedFlagKey].Split($flagSeparator)) {
        $releaseFlags.Add($flag)
    }
} elseif ($env:RUSTFLAGS) {
    # Cargo also splits unencoded RUSTFLAGS on whitespace. Callers with a flag
    # containing spaces must use CARGO_ENCODED_RUSTFLAGS instead.
    foreach ($flag in ($env:RUSTFLAGS -split '\s+' | Where-Object { $_ })) {
        $releaseFlags.Add($flag)
    }
}
if ($isWindowsTarget) {
    # CARGO_ENCODED_RUSTFLAGS replaces .cargo/config.toml target rustflags; keep the
    # workspace's static MSVC CRT, matching tools/build-release.ps1.
    $releaseFlags.Insert(0, 'target-feature=+crt-static')
    $releaseFlags.Insert(0, '-C')
} elseif (@($releaseFlags | Where-Object { $_.Contains('+crt-static') }).Count) {
    throw 'GNU Linux and macOS server releases must not enable +crt-static.'
}

$userProfile = if ($env:USERPROFILE) {
    $env:USERPROFILE
} elseif ($env:HOME) {
    $env:HOME
} else {
    [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
}
if (!$userProfile) { throw 'Could not determine the native user home for path remapping.' }
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $userProfile '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $userProfile '.rustup' }
$targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $taskRoot 'target' }
$pathMappings = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
$pathMappings[[IO.Path]::GetFullPath($taskRoot)] = '/audiolib/source'
$pathMappings[[IO.Path]::GetFullPath($userProfile, $taskRoot)] = '/audiolib/user'
$pathMappings[[IO.Path]::GetFullPath($cargoHome, $taskRoot)] = '/audiolib/cargo'
$pathMappings[[IO.Path]::GetFullPath($rustupHome, $taskRoot)] = '/audiolib/rustup'
$pathMappings[[IO.Path]::GetFullPath($targetDirectory, $taskRoot)] = '/audiolib/build'
if ($isWindowsTarget) {
    # A mapped network drive also embeds its UNC root (same as build-release.ps1).
    $workspaceDrive = Get-PSDrive -Name ([IO.Path]::GetPathRoot($taskRoot).TrimEnd(':', '\')) -ErrorAction SilentlyContinue
    if ($workspaceDrive -and $workspaceDrive.DisplayRoot) {
        $pathMappings[$workspaceDrive.DisplayRoot.TrimEnd('\') + '\' + $taskRoot.Substring(3)] = '/audiolib/source'
    }
}

# Each remap is one encoded Rust argument, including when a source directory has
# spaces. Native cc-rs flags use its shell parser and quoted individual arguments.
function ConvertTo-NativeFlag([string]$Flag) {
    $quote = [string][char]39
    $escapedQuote = $quote + [char]34 + $quote + [char]34 + $quote
    return $quote + $Flag.Replace($quote, $escapedQuote) + $quote
}
$nativeFlags = [Collections.Generic.List[string]]::new()
$nativeFlags.Add($(if ($isWindowsTarget) { '/DNDEBUG' } else { '-DNDEBUG' }))
foreach ($sourcePrefix in ($pathMappings.Keys | Sort-Object Length -Descending)) {
    $releaseFlags.Add('--remap-path-prefix=' + $sourcePrefix + '=' + $pathMappings[$sourcePrefix])
    if ($isWindowsTarget) {
        $releaseFlags.Add('--remap-path-prefix=' + $sourcePrefix.Replace('\', '/') + '=' + $pathMappings[$sourcePrefix])
        continue
    }
    $nativeFlags.Add('-ffile-prefix-map=' + $sourcePrefix + '=' + $pathMappings[$sourcePrefix])
}
$quotedNativeFlags = @($nativeFlags | ForEach-Object { ConvertTo-NativeFlag $_ }) -join ' '
$combinedNativeFlags = ($savedEnvironment[$nativeFlagKey] + ' ' + $quotedNativeFlags).Trim()
$enteredWorkspace = $false
try {
    $buildLease = Enter-AudioLibBuildLease $taskRoot
    Push-Location -LiteralPath $taskRoot
    $enteredWorkspace = $true
    [Environment]::SetEnvironmentVariable($encodedFlagKey, ($releaseFlags -join $flagSeparator))
    # Headless server features currently exclude SoundTouch. Keep native release
    # mode and __FILE__ remapping explicit if a dependency later enables it;
    # cc-rs retains caller global/target flags and appends these target flags last.
    [Environment]::SetEnvironmentVariable($nativeFlagKey, $combinedNativeFlags)
    [Environment]::SetEnvironmentVariable($shellFlagKey, '1')
    cargo build -p audiolib-server -p audiolib-db --bin audiolib-server --bin audiolib-migrate --release --locked --no-default-features --target $Target
    if ($LASTEXITCODE -ne 0) { throw 'Rust server release build failed.' }
} finally {
    if ($buildLease) { $buildLease.Dispose() }
    foreach ($key in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($key, $savedEnvironment[$key])
    }
    if ($enteredWorkspace) { Pop-Location }
}
