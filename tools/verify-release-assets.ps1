# tools/verify-release-assets.ps1
# Read-only release identity and asset/checksum verification. Metadata is the JSON
# from `gh release view <tag> --json tagName,name,isPrerelease,assets`.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][string]$ReleaseJsonPath,
    [string]$AssetDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$tagMatch = [regex]::Match($Tag, '^v(?<major>0|[1-9]\d*)\.(?<minor>0|[1-9]\d*)\.(?<patch>0|[1-9]\d*)(?<suffix>-(?:rc|beta)\.(?:0|[1-9]\d*))?$')
if (!$tagMatch.Success) { throw "Tag '$Tag' must be canonical vX.Y.Z, vX.Y.Z-rc.N or vX.Y.Z-beta.N." }
$expectsPrerelease = $tagMatch.Groups['suffix'].Success
$major = [bigint]::Parse($tagMatch.Groups['major'].Value, [Globalization.CultureInfo]::InvariantCulture)
$minor = [bigint]::Parse($tagMatch.Groups['minor'].Value, [Globalization.CultureInfo]::InvariantCulture)
$patch = [bigint]::Parse($tagMatch.Groups['patch'].Value, [Globalization.CultureInfo]::InvariantCulture)
$includesMacClient = $major -gt 0 -or $minor -gt 8 -or ($minor -eq 8 -and $patch -ge 3)
$includesLinuxDocker = $major -gt 0 -or $minor -gt 8 -or ($minor -eq 8 -and $patch -ge 4)
# First release (including its -beta.N/-rc.N tags) that ships the native Windows x64
# server ZIP. Releases before it keep their historical asset sets (0.8.4: twelve).
$WindowsServerFirstVersion = [version]'0.8.5'
$includesWindowsServer = $major -gt $WindowsServerFirstVersion.Major -or ($major -eq $WindowsServerFirstVersion.Major -and ($minor -gt $WindowsServerFirstVersion.Minor -or ($minor -eq $WindowsServerFirstVersion.Minor -and $patch -ge $WindowsServerFirstVersion.Build)))
$packages = @("AudioLibExplorer-$Tag-win-x64-portable.zip")
if ($includesMacClient) { $packages += "AudioLibExplorer-$Tag-macos-arm64.zip" }
$packages += "AudioLibServer-$Tag-macos-arm64.tar.gz"
if ($includesLinuxDocker) {
    $packages += "AudioLibServer-$Tag-linux-x64.tar.gz"
    $packages += "AudioLibServer-$Tag-docker-linux-amd64.tar.gz"
    $packages += "AudioLibServer-$Tag-docker-linux-arm64.tar.gz"
}
if ($includesWindowsServer) { $packages += "AudioLibServer-$Tag-win-x64.zip" }
$expectedAssets = @($packages | ForEach-Object { $_; "$_.sha256" })

$releaseText = [IO.File]::ReadAllText([IO.Path]::GetFullPath($ReleaseJsonPath))
if (!$releaseText.TrimStart().StartsWith('{', [StringComparison]::Ordinal)) { throw 'Release metadata must be one JSON object.' }
$release = $releaseText | ConvertFrom-Json
if ($null -eq $release -or @($release).Count -ne 1) { throw 'Release metadata must be one JSON object.' }
foreach ($property in @('tagName', 'name', 'isPrerelease', 'assets')) {
    if ($release.PSObject.Properties.Name -cnotcontains $property) { throw "Release metadata is missing $property." }
}
if ($release.tagName -isnot [string] -or $release.tagName -cne $Tag) { throw 'Release tag does not match the requested tag.' }
if ($release.name -isnot [string] -or $release.name -cne "AudioLib Explorer $Tag") { throw 'Release title does not match the tag.' }
if ($release.isPrerelease -isnot [bool] -or $release.isPrerelease -ne $expectsPrerelease) { throw 'Release prerelease flag does not match the tag suffix.' }

$assetNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$caseInsensitiveNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($asset in @($release.assets)) {
    if ($null -eq $asset -or $asset.PSObject.Properties.Name -cnotcontains 'name' -or $asset.name -isnot [string] -or !$asset.name) { throw 'Release contains invalid asset metadata.' }
    if (!$caseInsensitiveNames.Add($asset.name) -or !$assetNames.Add($asset.name)) { throw "Duplicate or case-colliding release asset: $($asset.name)" }
}
$missing = @($expectedAssets | Where-Object { !$assetNames.Contains($_) })
$unexpected = @($assetNames | Where-Object { $expectedAssets -cnotcontains $_ })
if ($missing.Count -or $unexpected.Count) {
    $problems = @()
    if ($missing.Count) { $problems += "Missing release assets: $($missing -join ', ')" }
    if ($unexpected.Count) { $problems += "Unexpected release assets: $($unexpected -join ', ')" }
    throw ($problems -join '; ')
}

if ($AssetDirectory) {
    $assetRoot = [IO.Path]::GetFullPath($AssetDirectory)
    if (!(Test-Path -LiteralPath $assetRoot -PathType Container)) { throw 'Downloaded asset directory is missing.' }
    if ((Get-Item -LiteralPath $assetRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Linked asset directory is not supported.' }
    foreach ($package in $packages) {
        foreach ($filename in @($package, "$package.sha256")) {
            $filePath = Join-Path $assetRoot $filename
            if (!(Test-Path -LiteralPath $filePath -PathType Leaf)) { throw "Downloaded asset is missing: $filename" }
            $file = Get-Item -LiteralPath $filePath -Force
            if ($file.Name -cne $filename -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Downloaded asset has an invalid identity: $filename" }
        }
        $checksumText = [IO.File]::ReadAllText((Join-Path $assetRoot "$package.sha256")).Trim()
        $checksumMatch = [regex]::Match($checksumText, '^(?<hash>[0-9A-Fa-f]{64})[ \t]+\*?(?<name>[^\r\n]+)$')
        if (!$checksumMatch.Success -or $checksumMatch.Groups['name'].Value -cne $package) { throw "Malformed checksum file for $package" }
        $actualHash = (Get-FileHash -LiteralPath (Join-Path $assetRoot $package) -Algorithm SHA256).Hash
        if ($actualHash -ine $checksumMatch.Groups['hash'].Value) { throw "Checksum mismatch for $package" }
    }
    Write-Host "Published release checksums verified: $($packages.Count) packages."
}
Write-Host "Published release identity verified: $Tag, $($expectedAssets.Count) exact assets."
# Success-stream output is deliberately only package basenames, so callers can
# download exactly this list before invoking the checksum pass.
$packages
