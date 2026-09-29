# Test de bout en bout de l'installeur : freinage réseau de Windows (Lot W3,
# PLAN-FREINAGE-RESEAU-WINDOWS-2026-09, dépôt du site).
#
# Sur le runner Windows (machine jetable), avec le VRAI MSI qui vient d'être
# construit : installation, mise à jour depuis la dernière version publiée,
# réinstallation, désinstallation — et la valeur du registre vérifiée à chaque
# étape. Échec = la release reste en brouillon (le job `publish` dépend du build).
#
# Usage : msi-e2e-network-throttling.ps1 -Msi <nouveau.msi> -OldMsi <publié.msi>

param(
  [Parameter(Mandatory)] [string] $Msi,
  [Parameter(Mandatory)] [string] $OldMsi
)
$ErrorActionPreference = 'Stop'

$ProfileKey = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile'
$Memory  = 'HKLM:\SOFTWARE\Jamodio\AudioEngine'
$Off     = '-1'  # ffffffff, lu en entier signé par PowerShell

function Get-Nti {
  $v = (Get-ItemProperty -Path $ProfileKey -Name NetworkThrottlingIndex -ErrorAction SilentlyContinue).NetworkThrottlingIndex
  # Toujours un texte : 'absent', '10', '-1' (= ffffffff) — comparaisons sans conversion.
  if ($null -eq $v) { 'absent' } else { "$([int]$v)" }
}
function Set-Nti($v) {
  if ("$v" -eq 'absent') { Remove-ItemProperty -Path $ProfileKey -Name NetworkThrottlingIndex -ErrorAction SilentlyContinue }
  else { New-ItemProperty -Path $ProfileKey -Name NetworkThrottlingIndex -PropertyType DWord -Value ([int]$v) -Force | Out-Null }
}
function Get-Memory($name) {
  (Get-ItemProperty -Path $Memory -Name $name -ErrorAction SilentlyContinue).$name
}
function Assert($cond, $what) {
  if (-not $cond) { throw "ÉCHEC : $what" }
  Write-Host "  ok — $what"
}
function Invoke-Msi([string[]] $arguments, [int[]] $expected, [string] $log) {
  $p = Start-Process msiexec.exe -ArgumentList ($arguments + @('/qn', '/norestart', '/l*v', $log)) -Wait -PassThru
  if ($expected -notcontains $p.ExitCode) {
    Get-Content $log -Tail 60 | Write-Host
    throw "msiexec $($arguments -join ' ') : code $($p.ExitCode), attendu $($expected -join ' ou ')"
  }
  $p.ExitCode
}

$Reboot = 3010  # ERROR_SUCCESS_REBOOT_REQUIRED : le redémarrage est demandé

Write-Host '▸ 1. Mise à jour depuis la version publiée (le cas des musiciens déjà équipés), origine 10'
Set-Nti 10
Invoke-Msi @('/i', $OldMsi) @(0, $Reboot) 'old.log' | Out-Null
Assert ((Get-Nti) -eq '10') 'la version publiée ne touche pas au réglage'
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'upgrade.log'
Assert ((Get-Nti) -eq $Off) 'freinage désactivé après la mise à jour'
Assert ((Get-Memory NetworkThrottlingIndexOrigin) -eq '10') 'origine 10 mémorisée'
Assert ((Get-Memory NetworkThrottlingLastStep) -like 'install :*') "étape notée : $(Get-Memory NetworkThrottlingLastStep)"
Assert ($code -eq $Reboot) 'redémarrage proposé (le réglage vient de changer)'

Write-Host '▸ 2. Réinstallation (même chemin qu''une mise à jour suivante) : origine intacte, pas de redémarrage'
$code = Invoke-Msi @('/fvomus', $Msi) @(0, $Reboot) 'reinstall.log'
Assert ((Get-Memory NetworkThrottlingIndexOrigin) -eq '10') 'origine toujours 10'
Assert ($code -eq 0) 'aucun redémarrage demandé (déjà désactivé)'

Write-Host '▸ 3. Désinstallation : origine remise, rien de nous ne reste'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'uninstall.log' | Out-Null
Assert ((Get-Nti) -eq '10') 'valeur 10 remise'
Assert ($null -eq (Get-Memory NetworkThrottlingIndexOrigin)) 'origine effacée'

Write-Host '▸ 4. Valeur absente à l''origine'
Set-Nti 'absent'
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'absent-install.log'
Assert ((Get-Nti) -eq $Off) 'freinage désactivé'
Assert ($code -eq $Reboot) 'redémarrage proposé'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'absent-uninstall.log' | Out-Null
Assert ((Get-Nti) -eq 'absent') 'valeur retirée comme à l''origine'

Write-Host '▸ 5. Déjà désactivé avant nous'
Set-Nti $Off
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'off-install.log'
Assert ($code -eq 0) 'aucun redémarrage demandé'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'off-uninstall.log' | Out-Null
Assert ((Get-Nti) -eq $Off) 'toujours désactivé après désinstallation'

Write-Host '▸ 6. Modifié par quelqu''un d''autre entre-temps : jamais écrasé'
Set-Nti 10
Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'changed-install.log' | Out-Null
Set-Nti 20
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'changed-uninstall.log' | Out-Null
Assert ((Get-Nti) -eq '20') 'la valeur 20 posée depuis est gardée'

Set-Nti 10
Write-Host '✔ Installeur : freinage réseau géré comme prévu dans les 6 cas.'
