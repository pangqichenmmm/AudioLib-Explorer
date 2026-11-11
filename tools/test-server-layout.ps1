# tools/test-server-layout.ps1
# Entry-policy fixtures only: no compiler, package build, binary or server execution.
[CmdletBinding()]param()
$ErrorActionPreference='Stop';Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'server-package-layout.ps1')
foreach($server in @('audiolib-server','audiolib-server.exe')){
    $migrator=Get-ServerMigratorName $server;$entries=@(Get-ServerPublicDocuments)+@($server,$migrator,'licenses/dependencies.json','licenses/inventory.json','update-channel.txt','update-version.txt')
    $layout=Test-ServerPackageLayout -Entries $entries -ServerBinaryName $server;Assert-ServerLayoutMetadata ([pscustomobject]$layout)
    $oldRefused=$false;try{& (Join-Path $PSScriptRoot 'validate-update-compatibility.ps1') -Product Server -ServerBinaryName $server -Entries $entries}catch{$oldRefused=$_.Exception.Message -like ('*Non-public update file: '+$migrator)};if(!$oldRefused){throw 'Old updater falsely accepted migrator layout.'}
    $invalidSets=@(@{label='missing migrator';entries=@($entries|Where-Object {$_ -cne $migrator})},@{label='duplicate migrator';entries=($entries+@($migrator))},@{label='private settings';entries=($entries+@('settings.json'))},@{label='unrelated binary';entries=($entries+@('other.exe'))},@{label='opposite platform';entries=($entries+@($(if($migrator.EndsWith('.exe')){'audiolib-migrate'}else{'audiolib-migrate.exe'})))})
    foreach($invalid in $invalidSets){$refused=$false;try{Test-ServerPackageLayout -Entries $invalid.entries -ServerBinaryName $server|Out-Null}catch{$refused=$true};if(!$refused){throw "Invalid native migration layout was accepted: $($invalid.label)"}}
    foreach($field in @('automatic_upgrade_compatible','manual_bootstrap_required','minimum_updater_version','old_updater_policy','layout_revision')){$bad=[pscustomobject]($layout|ConvertTo-Json|ConvertFrom-Json -AsHashtable);$bad.$field=switch($field){'automatic_upgrade_compatible'{$true}'manual_bootstrap_required'{$false}'minimum_updater_version'{'0.8.7'}'old_updater_policy'{'accepted'}default{1}};$refused=$false;try{Assert-ServerLayoutMetadata $bad}catch{$refused=$true};if(!$refused){throw 'False automatic-upgrade/layout metadata was accepted.'}}
}
Write-Output 'Server layout policy passed: native migration binary required, old updater refused, manual bootstrap metadata mandatory, unrelated/opposite-platform/private entries rejected.'
