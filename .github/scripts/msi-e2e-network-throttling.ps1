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
  [Parameter(Mandatory)] [string] $OldMsi,
  # Journaux msiexec (/l*v) : conservés par le workflow en cas d'échec.
  [string] $LogDir = 'msi-logs',
  [int] $TimeoutMs = 120000
)
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$ErrorActionPreference = 'Stop'

$ProfileKey = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile'
$Memory  = 'HKLM:\SOFTWARE\Jamodio\AudioEngine'
$Off     = '4294967295'  # ffffffff, tel que PowerShell relit un DWORD (non signé)

function Get-Nti {
  $v = (Get-ItemProperty -Path $ProfileKey -Name NetworkThrottlingIndex -ErrorAction SilentlyContinue).NetworkThrottlingIndex
  # Toujours un texte : 'absent', '10', '4294967295' — comparaisons sans conversion.
  if ($null -eq $v) { 'absent' } else { "$v" }
}
function Set-Nti([string] $v) {
  $key = 'HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile'
  if ($v -eq 'absent') {
    Remove-ItemProperty -Path $ProfileKey -Name NetworkThrottlingIndex -ErrorAction SilentlyContinue
  } else {
    # reg.exe accepte 0xffffffff comme tout DWORD, sans question de signe.
    reg.exe add $key /v NetworkThrottlingIndex /t REG_DWORD /d $v /f | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "reg add $v : échec" }
  }
}
function Get-Memory($name) {
  (Get-ItemProperty -Path $Memory -Name $name -ErrorAction SilentlyContinue).$name
}
# Pose un état « laissé par une version précédente » (cas 7 et 8).
function Set-Memory([string] $name, $value, [string] $type = 'String') {
  New-Item -Path $Memory -Force | Out-Null
  New-ItemProperty -Path $Memory -Name $name -PropertyType $type -Value $value -Force | Out-Null
}
function Clear-Memory {
  Remove-Item -Path $Memory -Recurse -Force -ErrorAction SilentlyContinue
}
function Assert($cond, $what) {
  if (-not $cond) { throw "ÉCHEC : $what" }
  Write-Host "  ok — $what"
}
function Invoke-Msi([string[]] $arguments, [int[]] $expected, [string] $log) {
  $log = Join-Path $LogDir $log
  Write-Host "  [$(Get-Date -Format HH:mm:ss)] msiexec $($arguments -join ' ')"
  # Chaque argument entre guillemets s'il contient un espace : Start-Process
  # joint la liste SANS les ajouter, et msiexec, recevant un chemin coupé
  # (« …\Jamodio Audio Engine_x.msi »), ouvre sa fenêtre d'aide et attend un clic
  # — blocage vécu deux fois (0.6.6-11, 0.6.6-12). Attente bornée, jamais `-Wait`.
  $all = ($arguments + @('/qn', '/norestart', '/l*v', $log)) |
    ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
  $p = Start-Process msiexec.exe -ArgumentList ($all -join ' ') -PassThru
  if (-not $p.WaitForExit($TimeoutMs)) {
    Write-Host "  BLOQUÉ après $($TimeoutMs / 1000) s — programmes en cours :"
    Get-CimInstance Win32_Process | Where-Object { $_.Name -match 'msiexec|jamodio|Jamodio|WebView|EdgeUpdate|setup' } |
      ForEach-Object { Write-Host "    $($_.ProcessId) $($_.Name) :: $($_.CommandLine)" }
    if (Test-Path $log) { Get-Content $log -Tail 80 | Write-Host }
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    throw "msiexec $($arguments -join ' ') : bloqué"
  }
  if ($expected -notcontains $p.ExitCode) {
    Get-Content $log -Tail 60 | Write-Host
    throw "msiexec $($arguments -join ' ') : code $($p.ExitCode), attendu $($expected -join ' ou ')"
  }
  Write-Host "  [$(Get-Date -Format HH:mm:ss)] code $($p.ExitCode)"
  $p.ExitCode
}

$Reboot = 3010  # ERROR_SUCCESS_REBOOT_REQUIRED : le redémarrage est demandé

Write-Host '▸ 1. Mise à jour depuis la version publiée (le cas des musiciens déjà équipés), origine 10'
Set-Nti 10
Invoke-Msi @('/i', $OldMsi) @(0, $Reboot) 'old.log' | Out-Null
Assert ((Get-Nti) -eq '10') 'la version publiée ne touche pas au réglage'
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'upgrade.log'
Assert ((Get-Nti) -eq $Off) 'freinage désactivé après la mise à jour'
Assert ((Get-Memory NetworkThrottlingIndexOrigin) -eq 'msi:#10') "origine 10 mémorisée par l'installeur : $(Get-Memory NetworkThrottlingIndexOrigin)"
Assert ((Get-Memory NetworkThrottlingLastStep) -like 'install (installeur) : freinage désactivé, effectif au prochain redémarrage*') "étape notée : $(Get-Memory NetworkThrottlingLastStep)"
Assert ($code -eq $Reboot) 'redémarrage proposé (le réglage vient de changer)'

Write-Host '▸ 2. Réinstallation / réparation (réécrit tout le registre) : origine intacte, pas de redémarrage'
$code = Invoke-Msi @('/fvomus', $Msi) @(0, $Reboot) 'reinstall.log'
Assert ((Get-Memory NetworkThrottlingIndexOrigin) -eq 'msi:#10') "origine toujours 10 : $(Get-Memory NetworkThrottlingIndexOrigin)"
Assert ((Get-Memory NetworkThrottlingLastStep) -like '*déjà désactivé avant cette installation*') "trace exacte (rien ne change) : $(Get-Memory NetworkThrottlingLastStep)"
Assert ($code -eq 0) 'aucun redémarrage demandé (déjà désactivé)'

Write-Host '▸ 3. Désinstallation : origine remise, rien de nous ne reste'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'uninstall.log' | Out-Null
Assert ((Get-Nti) -eq '10') 'valeur 10 remise'
Assert ($null -eq (Get-Memory NetworkThrottlingIndexOrigin)) 'origine effacée'
Assert ($null -eq (Get-Memory NetworkThrottlingManaged)) 'marqueur retiré'
Assert ($null -eq (Get-Memory NetworkThrottlingLastStep)) 'trace retirée'

Write-Host '▸ 4. Valeur absente à l''origine'
Set-Nti 'absent'
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'absent-install.log'
Assert ((Get-Nti) -eq $Off) 'freinage désactivé'
Assert ($code -eq $Reboot) 'redémarrage proposé'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'absent-uninstall.log' | Out-Null
Assert ((Get-Nti) -eq 'absent') 'valeur retirée comme à l''origine'

Write-Host '▸ 5. Déjà désactivé avant nous'
Set-Nti '0xffffffff'
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

Write-Host '▸ 7. Le cas du 30/09 : installé par une version précédente, réglage jamais posé (marqueur sans origine)'
Clear-Memory
Set-Nti 10
Set-Memory NetworkThrottlingManaged 1 'DWord'
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'marker-install.log'
Assert ((Get-Nti) -eq $Off) 'freinage désactivé'
Assert ((Get-Memory NetworkThrottlingIndexOrigin) -eq 'msi:#10') 'origine 10 mémorisée'
Assert ($code -eq $Reboot) 'redémarrage proposé'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'marker-uninstall.log' | Out-Null
Assert ((Get-Nti) -eq '10') 'valeur 10 remise'

Write-Host '▸ 8. Origine déjà notée par une version précédente (0.6.6-12 à -14, forme « 10 ») : jamais remplacée'
Clear-Memory
Set-Nti '0xffffffff'
Set-Memory NetworkThrottlingManaged 1 'DWord'
Set-Memory NetworkThrottlingIndexOrigin '10'
$code = Invoke-Msi @('/i', $Msi) @(0, $Reboot) 'legacy-install.log'
Assert ((Get-Memory NetworkThrottlingIndexOrigin) -eq '10') "origine « 10 » conservée, pas remplacée par ffffffff : $(Get-Memory NetworkThrottlingIndexOrigin)"
Assert ($code -eq 0) 'aucun redémarrage demandé (déjà désactivé)'
Invoke-Msi @('/x', $Msi) @(0, $Reboot) 'legacy-uninstall.log' | Out-Null
Assert ((Get-Nti) -eq '10') 'valeur 10 remise'

Clear-Memory
Set-Nti 10
Write-Host '✔ Installeur : freinage réseau géré comme prévu dans les 8 cas.'
