# tools/server-package-layout.ps1
# Layout 2 adds the production migration tool. Old updaters genuinely refuse it.
function Get-ServerMigratorName([string]$ServerBinaryName){
    switch -CaseSensitive ($ServerBinaryName){'audiolib-server.exe'{return 'audiolib-migrate.exe'}'audiolib-server'{return 'audiolib-migrate'}default{throw 'Unsupported server binary layout.'}}
}
function Get-ServerPublicDocuments {
    @(Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'server-files.json')|ConvertFrom-Json|Where-Object {$_ -cnotin @('audiolib-migrate','audiolib-migrate.exe')})
}
function Test-ServerPackageLayout {
    param([string[]]$Entries,[string]$ServerBinaryName)
    $migrator=Get-ServerMigratorName $ServerBinaryName
    if(@($Entries|Where-Object {$_ -ceq $migrator}).Count -ne 1 -or @($Entries|Where-Object {$_ -ceq $ServerBinaryName}).Count -ne 1 -or @($Entries|Where-Object {$_ -cin @('audiolib-migrate','audiolib-migrate.exe') -and $_ -cne $migrator}).Count){throw 'Server layout requires exactly the native server and migration tool.'}
    # Everything except the explicitly new binary must still satisfy the frozen
    # old-updater boundary; never widen that historical policy to manufacture PASS.
    $legacyEntries=@($Entries|Where-Object {$_ -cne $migrator})
    & (Join-Path $PSScriptRoot 'validate-update-compatibility.ps1') -Product Server -ServerBinaryName $ServerBinaryName -Entries $legacyEntries|Out-Null
    $refused=$false
    try{& (Join-Path $PSScriptRoot 'validate-update-compatibility.ps1') -Product Server -ServerBinaryName $ServerBinaryName -Entries $Entries|Out-Null}catch{if(!$_.Exception.Message.EndsWith(('Non-public update file: '+$migrator),[StringComparison]::Ordinal)){throw};$refused=$true}
    if(!$refused){throw 'Frozen old-updater policy unexpectedly accepted the new server layout.'}
    return [ordered]@{layout_revision=2;automatic_upgrade_compatible=$false;manual_bootstrap_required=$true;minimum_updater_version='0.9.0';old_updater_policy='refused'}
}
function Assert-ServerLayoutMetadata($Value){
    if($Value.layout_revision -ne 2 -or $Value.automatic_upgrade_compatible -isnot [bool] -or $Value.automatic_upgrade_compatible -or $Value.manual_bootstrap_required -isnot [bool] -or !$Value.manual_bootstrap_required -or $Value.minimum_updater_version -cne '0.9.0' -or $Value.old_updater_policy -cne 'refused'){throw 'Server migration layout/bootstrap metadata is invalid.'}
}
