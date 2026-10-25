# tools/validate-spectral-ui.ps1
[CmdletBinding()]
param([string]$BinaryDirectory = 'dist/latest/portable', [string]$ReferenceWav, [string]$ArtifactDirectory = '.rust-validation/spectral-v4', [switch]$ThemeViews)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'console-utf8.ps1')
try {
$taskRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$binaryRoot = [IO.Path]::GetFullPath((Join-Path $taskRoot $BinaryDirectory))
$artifactRoot = Join-Path $taskRoot $ArtifactDirectory
$testRoot = Join-Path $artifactRoot ('run-' + [guid]::NewGuid().ToString('N'))
$previousDataRoot = $env:AUDIOLIB_DATA_DIR
New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
function Invoke-View([string]$Name, [string]$Language, [string]$View) {
    $snapshot = Join-Path $artifactRoot ($Name + '.png')
    $stdout = Join-Path $testRoot ($Name + '.stdout')
    $stderr = Join-Path $testRoot ($Name + '.stderr')
    $process = Start-Process -FilePath (Join-Path $binaryRoot 'AudioLibExplorer.exe') -WorkingDirectory $binaryRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr -ArgumentList @('--smoke-ui','--smoke-language',$Language,'--smoke-view',$View,'--smoke-delay-ms','7000','--screenshot',('"' + $snapshot + '"'))
    try {
        if (!$process.WaitForExit(25000)) { throw "Spectral UI smoke timed out: $Name" }
        if ($process.ExitCode -ne 0 -or !(Test-Path -LiteralPath $snapshot)) { throw "Spectral UI smoke failed: $(Get-Content -LiteralPath $stderr -Raw)" }
        if ((Get-Content -LiteralPath $stderr -Raw) -match 'binding loop|error:') { throw "Spectral UI emitted an error: $stderr" }
        [pscustomobject]@{ view=$Name; success=$true; screenshot=$snapshot }
    } finally {
        if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
}
try {
    $env:AUDIOLIB_DATA_DIR = Join-Path $testRoot 'real-library-copy'
    New-Item -ItemType Directory -Force -Path $env:AUDIOLIB_DATA_DIR | Out-Null
    # Explicit isolated preferences prevent first-start legacy database discovery.
    $initialSettings = @{active_database=(Join-Path $env:AUDIOLIB_DATA_DIR 'library.db');language='en';playback=@{volume=0.0};allowed_scan_roots=@()} | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText((Join-Path $env:AUDIOLIB_DATA_DIR 'settings.json'), $initialSettings, [Text.UTF8Encoding]::new($false))
    $views = @()
    $views += Invoke-View 'main-cn' 'cn' 'main'
    $views += Invoke-View 'compact-en' 'en' 'compact'
    $views += Invoke-View 'peaks-cn' 'cn' 'peaks'
    if ($ThemeViews) {
        $views += Invoke-View 'output-en' 'en' 'output'
        $views += Invoke-View 'batch-cn' 'cn' 'batch'
        $views += Invoke-View 'normal-en' 'en' 'normal'
    }
    # Synthetic tones are written only in this validation's owned directory.
    $fixtureRoot = Join-Path $testRoot 'fixtures'
    $paletteRoot = Join-Path $testRoot 'palette-userdata'
    New-Item -ItemType Directory -Path $fixtureRoot,$paletteRoot | Out-Null
    $wavePath = Join-Path $fixtureRoot 'Spectral reference - 7 tones, white noise, 628Hz mixed -20 -10 0 +10 dB.wav'
    $writer = [IO.BinaryWriter]::new([IO.File]::Create($wavePath))
    try {
        $rate = 48000; $dataSize = 12 * $rate * 4
        $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $writer.Write([uint32]($dataSize+36))
        $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $writer.Write([uint32]16)
        $writer.Write([uint16]1); $writer.Write([uint16]2); $writer.Write([uint32]$rate)
        $writer.Write([uint32]($rate*4)); $writer.Write([uint16]4); $writer.Write([uint16]16)
        $writer.Write([Text.Encoding]::ASCII.GetBytes('data')); $writer.Write([uint32]$dataSize)
        $random = [Random]::new(73)
        foreach ($frequency in @(14,36,93,241,628,1632,4241,0,-20,-10,-1,10)) {
            for ($frame=0; $frame -lt $rate; $frame++) {
                $time = $frame / $rate
                $envelope = 0.5 - 0.5 * [Math]::Cos(2*[Math]::PI*$time)
                $sample = if ($frequency -gt 10) {
                    [Math]::Sin(2*[Math]::PI*$frequency*$time)
                } elseif ($frequency -eq 0) {
                    $random.NextDouble()*2-1
                } else {
                    $snr = if ($frequency -eq -1) { 0 } else { $frequency }
                    $toneGain = [Math]::Pow(10,$snr/20)
                    $tone = [Math]::Sin(2*[Math]::PI*628*$time)*[Math]::Sqrt(2)
                    $noise = ($random.NextDouble()*2-1)*[Math]::Sqrt(3)
                    ($tone*$toneGain+$noise)/(2*(1+$toneGain))
                }
                $value = [int16]($sample*$envelope*24000)
                $writer.Write($value); $writer.Write($value)
            }
        }
    } finally { $writer.Dispose() }
    $paletteDb = Join-Path $paletteRoot 'library.db'
    @{active_database=$paletteDb;language='en';playback=@{volume=0.0};allowed_scan_roots=@()} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $paletteRoot 'settings.json') -Encoding UTF8
    $env:AUDIOLIB_DATA_DIR = $paletteRoot
    $created = (& (Join-Path $binaryRoot 'AudioLibCli.exe') db create $paletteDb | Out-String) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or !$created.success) { throw 'Synthetic spectral reference library creation failed.' }
    $scan = (& (Join-Path $binaryRoot 'AudioLibCli.exe') --database $paletteDb scan $fixtureRoot | Out-String) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or !$scan.success) { throw 'Synthetic spectral reference scan failed.' }
    $views += Invoke-View 'palette-en' 'en' 'main'
    if ($ReferenceWav) {
        $sourcePath = (Resolve-Path -LiteralPath $ReferenceWav).Path
        $sourceHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
        $referenceRoot = Join-Path $testRoot 'reference-fixture'
        $referenceData = Join-Path $testRoot 'reference-userdata'
        New-Item -ItemType Directory -Path $referenceRoot,$referenceData | Out-Null
        Copy-Item -LiteralPath $sourcePath -Destination $referenceRoot
        $referenceDb = Join-Path $referenceData 'library.db'
        @{active_database=$referenceDb;language='en';playback=@{volume=0.0};allowed_scan_roots=@()} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $referenceData 'settings.json') -Encoding UTF8
        $env:AUDIOLIB_DATA_DIR = $referenceData
        $created = (& (Join-Path $binaryRoot 'AudioLibCli.exe') db create $referenceDb | Out-String) | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0 -or !$created.success) { throw 'Reference library creation failed.' }
        $scan = (& (Join-Path $binaryRoot 'AudioLibCli.exe') --database $referenceDb scan $referenceRoot | Out-String) | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0 -or !$scan.success) { throw 'Reference copy scan failed.' }
        $views += Invoke-View 'reference-en' 'en' 'main'
        if ((Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash -ne $sourceHash) { throw 'Reference source changed.' }
        @{ source=$sourcePath; sha256=$sourceHash; unchanged=$true; isolated_copy=$true } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $artifactRoot 'reference-source.json') -Encoding UTF8
    }
    $views | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $artifactRoot 'views.json') -Encoding UTF8
    $views | ConvertTo-Json -Depth 5
} finally { $env:AUDIOLIB_DATA_DIR = $previousDataRoot }
} finally {
    Restore-AudioLibConsoleEncoding
}
