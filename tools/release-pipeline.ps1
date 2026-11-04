# tools/release-pipeline.ps1
<#
.SYNOPSIS
Run a trusted local release plan with bounded output and resumable receipts.
.DESCRIPTION
Plan/Status never build or validate. Run requires a clean checkout, stops on the
first failure, and rechecks input/environment/artifact identity before reuse.
Logs and receipts stay private under .rust-validation/release-pipeline/.
Plans are trusted executable configuration. Keep secrets in the environment.
#>
#Requires -Version 7.3
[CmdletBinding()]
param(
    [ValidateSet('Plan','Run','Status','Logs')][string]$Action = 'Plan',
    [string]$PlanFile = 'tools/release-plans/windows-client.json',
    [ValidatePattern('^v\d+\.\d+\.\d+(?:-(?:beta|rc)\.(?:0|[1-9]\d*))?$')][string]$Tag,
    [string]$VariablesFile,
    [string]$ChecksFile,
    [string]$RunDirectory,
    [string]$Stage,
    [ValidateRange(1,80)][int]$Tail = 15,
    [ValidateRange(256,16000)][int]$MaxCharacters = 6000,
    [switch]$CleanBuildArtifacts,
    [ValidateRange(0,100000)][double]$BuildCacheLimitGiB = 40,
    [string]$Root = (Split-Path -Parent $PSScriptRoot)
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
Set-StrictMode -Version Latest
$Root = [IO.Path]::GetFullPath($Root)
function Resolve-PathValue([string]$Path) { [IO.Path]::GetFullPath($Path, $Root) }
function Hash-Text([string]$Value) { [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($Value))).ToLowerInvariant() }
function Emit($Value) { $Value | ConvertTo-Json -Compress -Depth 12 | Write-Output }
function Assert-Unlinked([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction SilentlyContinue
        if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Linked pipeline paths are not supported.' }
        $cursor = [IO.Path]::GetDirectoryName($cursor)
    }
}
function Save-State {
    Assert-Unlinked $statePath
    Assert-Unlinked ($statePath + '.tmp')
    [IO.File]::WriteAllText(($statePath + '.tmp'), ($state | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
    [IO.File]::Move(($statePath + '.tmp'), $statePath, $true)
}
function Summary {
    Emit ([ordered]@{status=$state.status;commit=$state.commit;source_checks=$state.source_checks;stages=@($state.stages.Values | ForEach-Object { @{id=$_.id;status=$_.status} });state=$statePath})
}
function Expand-Value($Value) {
    if ($Value -is [string]) {
        if ($Value -match '^\$\{([A-Za-z][A-Za-z0-9_]*)\}$') {
            if (!$variables.Contains($Matches[1])) { throw "Unknown plan variable: $($Matches[1])" }
            $resolved = $variables[$Matches[1]]
            if ($resolved -is [array]) { return ,$resolved }
            return $resolved
        }
        return [regex]::Replace($Value, '\$\{([A-Za-z][A-Za-z0-9_]*)\}', {
            param($match)
            if (!$variables.Contains($match.Groups[1].Value)) { throw 'Unknown plan variable.' }
            [string]$variables[$match.Groups[1].Value]
        })
    }
    if ($Value -is [Collections.IDictionary]) {
        $expanded = [ordered]@{}
        foreach ($key in $Value.Keys) { $expanded[$key] = Expand-Value $Value[$key] }
        return $expanded
    }
    if ($Value -is [array]) { return ,@($Value | ForEach-Object { Expand-Value $_ }) }
    return $Value
}
function Artifact-Identity($Paths) {
    $records = [Collections.Generic.List[object]]::new()
    foreach ($path in $Paths) {
        $full = Resolve-PathValue $path
        Assert-Unlinked $full
        if (!(Test-Path -LiteralPath $full)) { throw "Missing stage artifact: $path" }
        $files = if (Test-Path -LiteralPath $full -PathType Container) { @(Get-ChildItem -LiteralPath $full -Recurse -Force) } else { @(Get-Item -LiteralPath $full) }
        foreach ($file in ($files | Sort-Object FullName)) {
            if ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Linked artifact is not supported.' }
            if (!$file.PSIsContainer) { $records.Add([ordered]@{path=$file.FullName;sha256=(Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash;mode=$(if ($IsWindows) {''} else {[int][IO.File]::GetUnixFileMode($file.FullName)})}) }
        }
        $records.Add([ordered]@{path=$full;kind=$(if (Test-Path -LiteralPath $full -PathType Container) {'directory'} else {'file'})})
    }
    Hash-Text ($records | ConvertTo-Json -Compress -Depth 4)
}
function Get-ArtifactKey($Paths) {
    $key = Hash-Text (ConvertTo-Json -InputObject @($Paths) -Compress)
    if (!$artifactCache.ContainsKey($key)) { $artifactCache[$key] = Artifact-Identity $Paths }
    $artifactCache[$key]
}
function Assert-Dependencies($Item) {
    if ($Item.Contains('needs')) {
        foreach ($dependency in $Item.needs) {
            if ((Get-ArtifactKey $stageOutputs[$dependency]) -cne $state.stages[$dependency].artifacts) { throw "Stage artifact changed after acceptance: $dependency" }
        }
    }
}
function Read-Git([string[]]$Arguments) {
    $result = & git -C $Root @Arguments 2>$null
    if ($LASTEXITCODE -ne 0) { throw "Git input check failed: $($Arguments[0])" }
    ($result | Out-String).Trim()
}
function Assert-Source {
    if ((Read-Git @('rev-parse','HEAD')) -cne $commit -or (Read-Git @('status','--porcelain','--untracked-files=normal'))) { throw 'Source changed during the pipeline; receipt was not accepted.' }
    if ($plan.Contains('require_tag') -and $plan.require_tag -and (Read-Git @('rev-parse',"$Tag`^{commit}")) -cne $commit) { throw 'Release tag changed during the pipeline.' }
}
function Read-LogTail([string]$Path) {
    Assert-Unlinked $Path
    $stream = [IO.File]::Open($Path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::ReadWrite)
    try {
        $length = [int][Math]::Min($stream.Length, $MaxCharacters * 4)
        $null = $stream.Seek(-$length,[IO.SeekOrigin]::End)
        $bytes = [byte[]]::new($length)
        $read = $stream.Read($bytes,0,$length)
        $text = [Text.Encoding]::UTF8.GetString($bytes,0,$read).TrimEnd("`r","`n")
        $text = (@($text -split '\r?\n' | Select-Object -Last $Tail) -join "`n")
        $text -replace '(gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+)', '<redacted>'
    } finally { $stream.Dispose() }
}
$lock = $null
$process = $null
$buildLease = $null
. (Join-Path $PSScriptRoot 'build-cache.ps1')
try {
    $plan = Get-Content -LiteralPath (Resolve-PathValue $PlanFile) -Raw | ConvertFrom-Json -AsHashtable
    if ($plan.schema_version -ne 1 -or $plan.name -notmatch '^[a-z0-9-]+$') { throw 'Unsupported release plan.' }
    if (!$RunDirectory) {
        if (!$Tag) { throw 'Provide -Tag or an existing -RunDirectory.' }
        $RunDirectory = Join-Path $Root ('.rust-validation/release-pipeline/' + $Tag + '/' + $plan.name)
    }
    $RunDirectory = Resolve-PathValue $RunDirectory
    $ownedRoot = Join-Path $Root '.rust-validation/release-pipeline'
    $pathComparison = if ($IsWindows) { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
    if (!$RunDirectory.StartsWith($ownedRoot + [IO.Path]::DirectorySeparatorChar, $pathComparison)) { throw 'RunDirectory must be under .rust-validation/release-pipeline/.' }
    Assert-Unlinked $RunDirectory
    $statePath = Join-Path $RunDirectory 'state.json'
    Assert-Unlinked $statePath
    if ($Action -in @('Status','Logs')) {
        $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json -AsHashtable
        if ($Action -eq 'Status') { Summary; exit 0 }
        if (!$Stage -or !$state.stages.Contains($Stage)) { throw 'Choose an existing -Stage for Logs.' }
        $remaining = $MaxCharacters
        foreach ($suffix in @('stdout','stderr')) {
            $log = if ($state.stages[$Stage].Contains('log_directory')) { Join-Path $state.stages[$Stage].log_directory ($suffix + '.log') } else { Join-Path $RunDirectory ($Stage + '.' + $suffix + '.log') }
            $log = [IO.Path]::GetFullPath($log)
            if (!$log.StartsWith($RunDirectory + [IO.Path]::DirectorySeparatorChar,$pathComparison)) { throw 'Log path escaped the owned run directory.' }
            if ($remaining -gt 1 -and (Test-Path -LiteralPath $log)) {
                $text = Read-LogTail $log
                $available = [Math]::Max(0,$remaining - [Environment]::NewLine.Length)
                if ($text.Length -gt $available) { $text = $text.Substring($text.Length - $available) }
                if ($text) { Write-Output $text; $remaining -= $text.Length + [Environment]::NewLine.Length }
            }
        }
        exit 0
    }
    $commit = Read-Git @('rev-parse','HEAD')
    $variables = [ordered]@{Tag=$Tag;Commit=$commit;Root=$Root;RunDirectory=$RunDirectory;Asio=$false;TargetDirectory=$(if ($env:CARGO_TARGET_DIR) { Resolve-PathValue $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' })}
    if (!$plan.Contains('variables')) { $plan.variables = @{} }
    if (!$plan.Contains('environment_variables')) { $plan.environment_variables = @() }
    if (!$plan.Contains('environment_tools')) { $plan.environment_tools = @() }
    foreach ($key in $plan.variables.Keys) {
        if ($variables.Contains($key)) { throw 'Plan cannot replace a reserved variable.' }
        $variables[$key] = Expand-Value $plan.variables[$key]
    }
    if ($VariablesFile) {
        $overrides = Get-Content -LiteralPath (Resolve-PathValue $VariablesFile) -Raw | ConvertFrom-Json -AsHashtable
        foreach ($key in $overrides.Keys) {
            if (!$plan.variables.Contains($key)) { throw "Variable cannot be overridden: $key" }
            $variables[$key] = $overrides[$key]
        }
    }
    if ($variables.Contains('WithoutAsio')) {
        if ($variables.WithoutAsio -isnot [bool]) { throw 'WithoutAsio must be a JSON boolean.' }
        $variables.Asio = !$variables.WithoutAsio
    }
    $steps = @($plan.stages)
    if (!$steps.Count) { throw 'A release plan must contain stages.' }
    if ($ChecksFile) {
        $checks = @(Get-Content -LiteralPath (Resolve-PathValue $ChecksFile) -Raw | ConvertFrom-Json -AsHashtable)
        # Reject an invalid release identity before spending time on Rust checks.
        if ($steps[0].id -eq 'preflight') { $steps = @($steps[0]) + $checks + @($steps | Select-Object -Skip 1) }
        else { $steps = $checks + $steps }
    }
    if ($CleanBuildArtifacts) {
        $steps += @{id='build-cache-cleanup';script='tools/clean-build-artifacts.ps1';reuse='never';parameters=@{Action='Run';Mode='Debug';MaxGiB=$BuildCacheLimitGiB;Root=$Root};needs=@($steps[-1].id)}
    }
    $ids = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $expandedSteps = @()
    $stageOutputs = @{}
    foreach ($item in $steps) {
        $item = Expand-Value $item
        if ($item.id -notmatch '^[a-z0-9-]+$' -or !$ids.Add($item.id)) { throw 'Stage IDs must be unique safe names.' }
        if ($item.Contains('needs')) { foreach ($dependency in $item.needs) { if ($dependency -eq $item.id -or !$ids.Contains($dependency)) { throw 'Dependencies must name earlier stages.' } } }
        if ($item.Contains('script') -eq $item.Contains('command')) { throw 'A stage must select one script or native command.' }
        if ($item.Contains('reuse') -and $item.reuse -notin @('commit','inputs','never')) { throw 'Unsupported reuse mode.' }
        if ($item.Contains('reuse') -and $item.reuse -eq 'inputs' -and (!$item.Contains('input_paths') -or !$item.input_paths.Count)) { throw 'Input reuse requires explicit input_paths.' }
        $expandedSteps += ,$item
        $stageOutputs[$item.id] = if ($item.Contains('outputs')) { @($item.outputs) } else { @() }
    }
    if ($Action -eq 'Plan') {
        Emit @{status='planned';name=$plan.name;stages=@($expandedSteps | ForEach-Object { @{id=$_.id;command=$(if ($_.Contains('script')) {$_.script} else {$_.command})} });source_checks=$(if ($ChecksFile) {'selected'} else {'not-requested'});state=$statePath}
        exit 0
    }
    if (!$Tag) { throw 'Run requires -Tag.' }
    if (Read-Git @('status','--porcelain','--untracked-files=normal')) { throw 'Run requires a clean checkout; use Plan while editing.' }
    if ($plan.Contains('require_tag') -and $plan.require_tag -and (Read-Git @('rev-parse',"$Tag`^{commit}")) -cne $commit) { throw 'Release tag must identify HEAD.' }
    $hostName = if ($IsWindows) {'windows'} elseif ($IsMacOS) {'macos'} else {'linux'}
    if ($plan.Contains('host') -and $plan.host -ne 'any' -and $plan.host -ne $hostName) { throw 'Run this plan on its matching native host.' }
    New-Item -ItemType Directory -Path $RunDirectory -Force | Out-Null
    Assert-Unlinked (Join-Path $RunDirectory '.lock')
    $lock = [IO.File]::Open((Join-Path $RunDirectory '.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $childLockPath = Join-Path $RunDirectory '.child.lock'
    Assert-Unlinked $childLockPath
    $childProbe = [IO.File]::Open($childLockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $childProbe.Dispose()
    $buildLease = Enter-AudioLibBuildLease $Root
    $previousState = if (Test-Path -LiteralPath $statePath) { Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json -AsHashtable } else { @{schema_version=2;cache=@{}} }
    if ($previousState.schema_version -notin @(1,2)) { throw 'Unsupported pipeline state schema.' }
    $cache = if ($previousState.Contains('cache')) { $previousState.cache } else { $previousState.stages }
    $previous = @{stages=$cache}
    # Current-attempt status and reusable successful evidence are separate. A
    # transient early failure must not erase later, still-valid payload receipts.
    $state = [ordered]@{schema_version=2;status='running';commit=$commit;source_checks=$(if ($ChecksFile) {'selected'} else {'not-requested'});stages=[ordered]@{};cache=$cache}
    $environment = [ordered]@{host=$hostName;architecture=[Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString();powershell=$PSVersionTable.PSVersion.ToString()}
    $nativeEnvironment = @([Environment]::GetEnvironmentVariables().Keys | Where-Object { $_ -match '^(CARGO_(BUILD_|TARGET_|PROFILE_|ENCODED_|HOME$)|RUST(UP_|FLAGS$|C($|_)|DOC($|FLAGS$|_))|SLINT_|CXX($|FLAGS|_)|CC($|_)|(HOST|TARGET)_(CC|CXX|CFLAGS|CXXFLAGS|AR)|AR($|_)|CFLAGS($|_)|CPPFLAGS($|_)|LDFLAGS($|_)|BINDGEN_|PKG_CONFIG|LIBCLANG|CPAL_ASIO|SDKROOT$|MACOSX_DEPLOYMENT_TARGET$|LIB(PATH)?$|LINK$|CL$|_CL_$|INCLUDE$|VCINSTALLDIR$|VCToolsInstallDir$|WindowsSdkDir$)' })
    foreach ($name in (@('PATH','RUSTUP_TOOLCHAIN','CARGO_HOME','RUSTUP_HOME','CARGO_TARGET_DIR','CARGO_BUILD_TARGET','CARGO_ENCODED_RUSTFLAGS','RUSTFLAGS','CXXFLAGS','LIBCLANG_PATH','CPAL_ASIO_DIR') + $nativeEnvironment + @($plan.environment_variables) | Sort-Object -Unique)) {
        $environment[$name] = [Environment]::GetEnvironmentVariable($name)
    }
    Push-Location -LiteralPath $Root
    try {
        foreach ($tool in @($plan.environment_tools)) {
            $version = & $tool --version 2>$null
            if ($LASTEXITCODE -ne 0) { throw "Cannot identify required tool: $tool" }
            $environment['tool:' + $tool] = ($version | Out-String).Trim()
        }
    } finally { Pop-Location }
    $environmentKey = Hash-Text ($environment | ConvertTo-Json -Compress)
    $runnerKey = (Get-FileHash -LiteralPath $PSCommandPath).Hash + (Get-FileHash -LiteralPath (Join-Path $PSScriptRoot 'release-stage.ps1')).Hash
    $artifactCache = @{}
    Save-State
    foreach ($item in $expandedSteps) {
        Assert-Source
        $id = $item.id
        Assert-Dependencies $item
        if ($CleanBuildArtifacts -and $id -eq 'build-cache-cleanup' -and $buildLease) { $buildLease.Dispose();$buildLease=$null }
        $command = [ordered]@{root=$Root;lock_path=$childLockPath}
        if ($item.Contains('github_auth')) { $command.github_auth = [bool]$item.github_auth }
        if ($item.Contains('script')) { $command.script = Resolve-PathValue $item.script; $command.parameters = if ($item.Contains('parameters')) {$item.parameters} else {@{}} }
        else { $command.command=$item.command; $command.arguments=if ($item.Contains('arguments')) {@($item.arguments)} else {@()} }
        $sourceKey = if ($item.Contains('reuse') -and $item.reuse -eq 'inputs') { Read-Git (@('ls-tree','-r','--full-tree','HEAD','--') + @($item.input_paths)) } else { $commit }
        $dependencyKeys = @()
        if ($item.Contains('needs')) { foreach ($dependency in $item.needs) { $dependencyKeys += $state.stages[$dependency].receipt } }
        $scriptKey = if ($command.Contains('script')) { (Get-FileHash -LiteralPath $command.script).Hash } else { '' }
        $externalKey = if ($item.Contains('input_files')) { Artifact-Identity @($item.input_files) } else { '' }
        $inputKey = Hash-Text (($item | ConvertTo-Json -Compress -Depth 12) + ($command | ConvertTo-Json -Compress -Depth 12) + $scriptKey + $sourceKey + $externalKey + $environmentKey + $runnerKey + ($dependencyKeys -join ','))
        $outputs = if ($item.Contains('outputs')) {@($item.outputs)} else {@()}
        $artifactKey = $null
        if ($previous.stages.Contains($id) -and $previous.stages[$id].status -in @('passed','reused') -and $previous.stages[$id].input -ceq $inputKey -and !($item.Contains('reuse') -and $item.reuse -eq 'never')) {
            try {
                $artifactKey = Get-ArtifactKey $outputs
            } catch { $artifactKey = $null }
        }
        if ($artifactKey -and $artifactKey -ceq $previous.stages[$id].artifacts) {
            $state.stages[$id] = $previous.stages[$id]
            $state.stages[$id].status = 'reused'
            Save-State
            Emit @{stage=$id;status='reused'}
            continue
        }
        # A rerun may mutate any artifacts. Invalidate the in-process hash cache.
        $artifactCache.Clear()
        $attemptDirectory = Join-Path $RunDirectory ('attempts/' + $id + '-' + [guid]::NewGuid().ToString('N'))
        Assert-Unlinked $attemptDirectory
        New-Item -ItemType Directory -Path $attemptDirectory -Force | Out-Null
        $state.stages[$id] = @{id=$id;status='running';input=$inputKey;source_commit=$commit;log_directory=$attemptDirectory;started_at=[DateTime]::UtcNow.ToString('o')}
        $state.stages[$id] | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $attemptDirectory 'receipt.json') -Encoding utf8
        Save-State
        Emit @{stage=$id;status='running'}
        $commandFile = Join-Path $attemptDirectory 'command.json'
        Assert-Unlinked $commandFile
        [IO.File]::WriteAllText($commandFile, ($command | ConvertTo-Json -Depth 12), [Text.UTF8Encoding]::new($false))
        $start = [Diagnostics.ProcessStartInfo]::new()
        $start.FileName = Join-Path $PSHOME ('pwsh' + $(if ($IsWindows) {'.exe'} else {''}))
        $start.UseShellExecute=$false; $start.CreateNoWindow=$true; $start.RedirectStandardOutput=$true; $start.RedirectStandardError=$true
        foreach ($argument in @('-NoProfile','-File',(Join-Path $PSScriptRoot 'release-stage.ps1'),'-CommandFile',$commandFile)) { $start.ArgumentList.Add($argument) }
        $streams = @()
        try {
            foreach ($suffix in @('stdout','stderr')) {
                $log = Join-Path $attemptDirectory ($suffix + '.log')
                Assert-Unlinked $log
                $streams += [IO.File]::Create($log)
            }
            $process = [Diagnostics.Process]::Start($start)
            $copies = @($process.StandardOutput.BaseStream.CopyToAsync($streams[0]), $process.StandardError.BaseStream.CopyToAsync($streams[1]))
            $timer = [Diagnostics.Stopwatch]::StartNew()
            $nextUpdate = 60
            while (!$process.WaitForExit(1000)) {
                if ($timer.Elapsed.TotalSeconds -ge $nextUpdate) { Emit @{stage=$id;status='running';seconds=[int]$timer.Elapsed.TotalSeconds}; $nextUpdate += 60 }
            }
            foreach ($copy in $copies) { $null = $copy.GetAwaiter().GetResult() }
            if ($process.ExitCode -ne 0) { throw "Stage $id failed (exit $($process.ExitCode)); use Logs -Stage $id." }
        } finally {
            foreach ($stream in $streams) { $stream.Dispose() }
            if ($process -and $process.HasExited) { $process.Dispose(); $process=$null }
            foreach ($suffix in @('stdout','stderr')) {
                $latestLog = Join-Path $RunDirectory ($id + '.' + $suffix + '.log')
                Assert-Unlinked $latestLog
                $attemptLog = Join-Path $attemptDirectory ($suffix + '.log')
                if (Test-Path -LiteralPath $attemptLog) { [IO.File]::Copy($attemptLog,$latestLog,$true) }
            }
        }
        Assert-Source
        Assert-Dependencies $item
        $artifactKey = Get-ArtifactKey $outputs
        $receipt = Hash-Text ($inputKey + $artifactKey)
        $state.stages[$id] = @{id=$id;status='passed';input=$inputKey;artifacts=$artifactKey;receipt=$receipt;source_commit=$commit;log_directory=$attemptDirectory;seconds=[Math]::Round($timer.Elapsed.TotalSeconds,1)}
        $state.cache[$id] = $state.stages[$id]
        $state.stages[$id] | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $attemptDirectory 'receipt.json') -Encoding utf8
        Save-State
        Emit @{stage=$id;status='passed';seconds=$state.stages[$id].seconds}
    }
    Assert-Source
    $state.status='passed'; Save-State; Summary
} catch {
    if ($lock -and (Get-Variable state -ErrorAction SilentlyContinue)) {
        $state.status='failed'
        if ((Get-Variable id -ErrorAction SilentlyContinue) -and $state.stages.Contains($id) -and $state.stages[$id].status -eq 'running') {
            $state.stages[$id].status='failed'
            if ($state.cache.Contains($id)) { $null = $state.cache.Remove($id) }
            $state.stages[$id] | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $state.stages[$id].log_directory 'receipt.json') -Encoding utf8
        }
        Save-State
    }
    Emit @{status='failed';error=$_.Exception.Message}
    exit 1
} finally { if ($buildLease) { $buildLease.Dispose() }; if ($lock) { $lock.Dispose() } }
