# tools/validate-portable-licenses.ps1
# Shared package/startup preflight. Match the build/package feature selection;
# do not infer ASIO absence from a missing license file.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryDirectory,
    [switch]$WithoutAsio
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'dependency-licenses.ps1')
$licenseRoot = Join-Path $BinaryDirectory 'licenses'
$notices = Get-Content -LiteralPath (Join-Path $licenseRoot 'dependencies.json') -Raw | ConvertFrom-Json
if (!($notices | Where-Object { $_.name -eq 'slint' -and $_.version -eq '1.17.1' })) { throw 'Slint dependency notice missing.' }
if (!(Test-Path -LiteralPath (Join-Path $licenseRoot 'slint-1.17.1/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md') -PathType Leaf)) { throw 'Selected Slint license text missing.' }
if (!$WithoutAsio -and !(Test-Path -LiteralPath (Join-Path $licenseRoot 'Steinberg-ASIO-SDK-2.3.4.txt') -PathType Leaf)) { throw 'ASIO SDK license missing.' }
if (!($notices | Where-Object { $_.name -eq 'soundtouch-ffi' -and $_.version -eq '0.4.1' }) -or
    !(Test-Path -LiteralPath (Join-Path $licenseRoot 'SoundTouch-2.3.2-LGPL.txt') -PathType Leaf)) { throw 'SoundTouch dependency or license notice missing.' }
$licenseFeatures=if($WithoutAsio){@()}else{@('audiolib-desktop/asio','audiolib-cli/asio')}
$inventory=Test-AudioLibLicenseBundle -PayloadRoot $BinaryDirectory -ExpectedRoots @('audiolib-desktop','audiolib-cli') -Target 'x86_64-pc-windows-msvc' -Features $licenseFeatures -ExpectedArtifacts @('AudioLibExplorer.exe','AudioLibCli.exe')
$hasAsio=@($inventory.packages|Where-Object name -CEQ 'asio-sys').Count -gt 0
if($hasAsio -eq [bool]$WithoutAsio){throw 'Portable ASIO dependency inventory differs from the selected features.'}
Write-Host 'Portable license materials passed for the selected build features.'
