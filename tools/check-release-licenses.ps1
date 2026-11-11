# tools/check-release-licenses.ps1
# Resolve selected dependency notices before a release build, without compiling,
# downloading, staging a package or opening an application library.
#Requires -Version 7.3
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('WindowsClient','MacosClient','Server')][string]$Profile,
    [ValidateSet('x86_64-pc-windows-msvc','x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu','aarch64-apple-darwin')][string]$Target,
    [switch]$WithoutAsio
)
$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
. (Join-Path $PSScriptRoot 'dependency-licenses.ps1')
$packages = @('audiolib-desktop','audiolib-cli')
$features = @()
$noDefaultFeatures = $false
switch ($Profile) {
    'WindowsClient' {
        if ($Target -and $Target -cne 'x86_64-pc-windows-msvc') { throw 'WindowsClient requires the Windows x64 target.' }
        $Target = 'x86_64-pc-windows-msvc'
        if (!$WithoutAsio) { $features = @('audiolib-desktop/asio','audiolib-cli/asio') }
    }
    'MacosClient' {
        if ($Target -and $Target -cne 'aarch64-apple-darwin') { throw 'MacosClient requires the macOS arm64 target.' }
        if ($WithoutAsio) { throw 'WithoutAsio applies only to WindowsClient.' }
        $Target = 'aarch64-apple-darwin'
        $noDefaultFeatures = $true
    }
    'Server' {
        if (!$Target) { throw 'Server requires an explicit release Target.' }
        if ($WithoutAsio) { throw 'WithoutAsio applies only to WindowsClient.' }
        $packages = @('audiolib-server','audiolib-db')
        $noDefaultFeatures = $true
    }
}
$graph = Get-AudioLibLicenseGraph -Root $taskRoot -Packages $packages -Target $Target -Features $features -NoDefaultFeatures:$noDefaultFeatures
$notices = @(Get-AudioLibNoticePlan -Graph $graph -Root $taskRoot)
$noticeCount = 0
foreach ($notice in $notices) {
    foreach ($file in $notice.files) {
        # Declared license_file entries can name absent files. Read the actual
        # notice now so missing/unreadable external registry files fail early.
        $null = Get-FileHash -LiteralPath $file.source -Algorithm SHA256 -ErrorAction Stop
        $noticeCount++
    }
    $noticeCount += @($notice.shared_notices).Count
}
[ordered]@{status='passed';profile=$Profile;target=$Target;features=$features;no_default_features=$noDefaultFeatures;packages=$notices.Count;notices=$noticeCount} | ConvertTo-Json -Compress
