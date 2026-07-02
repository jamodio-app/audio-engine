<#
  win-cargo.ps1 — Enveloppe de build Windows pour jamodio-agent.

  Charge l'environnement VS Developer (vcvars64) + les variables ASIO/LLVM, puis
  exécute `cargo` avec les arguments passés. Nécessaire car asio-sys :
    - compile le SDK ASIO C++ via le crate `cc` (a besoin de la toolchain MSVC),
    - génère des bindings via bindgen/libclang sur des headers qui incluent
      <windows.h> (a besoin de INCLUDE/LIB du Windows SDK + CRT MSVC).
  Un shell nu n'a ni INCLUDE ni LIB → bindgen échoue. vcvars64 les pose.

  Usage :  pwsh -File scripts\win-cargo.ps1 build -p jamodio-agent
           pwsh -File scripts\win-cargo.ps1 build -p jamodio-agent --features ...
#>
[CmdletBinding()]
param(
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]] $CargoArgs
)
$ErrorActionPreference = 'Stop'

# --- 1. Localiser l'installation VS + vcvars64 -------------------------------
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere introuvable : $vswhere" }
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw "Aucune install VS avec VC.Tools.x86.x64 trouvée" }
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat introuvable : $vcvars" }

# --- 2. Importer l'environnement vcvars64 dans la session --------------------
# On lance vcvars64 dans cmd puis on capture `set` et on réinjecte chaque var.
$tmp = [System.IO.Path]::GetTempFileName()
cmd /c "`"$vcvars`" >NUL 2>&1 && set > `"$tmp`""
Get-Content $tmp | ForEach-Object {
  if ($_ -match '^([^=]+)=(.*)$') {
    [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
  }
}
Remove-Item $tmp -Force

# --- 3. Variables spécifiques ASIO / bindgen --------------------------------
# CPAL_ASIO_DIR : racine du SDK Steinberg (feature `asio` de cpal).
# LIBCLANG_PATH : dossier de libclang.dll (bindgen d'asio-sys).
if (-not $env:CPAL_ASIO_DIR) { $env:CPAL_ASIO_DIR = 'C:\SDKs\asiosdk_2.3.3_2019-06-14' }
if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }

# S'assurer que cargo/rustc sont dans le PATH (vcvars n'y touche pas).
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ($env:Path -notlike "*$cargoBin*") { $env:Path = "$cargoBin;$env:Path" }

Write-Host "== win-cargo ==" -ForegroundColor Cyan
Write-Host "VS       : $vsPath"
Write-Host "CPAL_ASIO_DIR : $env:CPAL_ASIO_DIR"
Write-Host "LIBCLANG_PATH : $env:LIBCLANG_PATH"
Write-Host "cargo    : $((Get-Command cargo).Source)"
Write-Host "cmd      : cargo $($CargoArgs -join ' ')" -ForegroundColor Yellow

# --- 4. Exécuter cargo ------------------------------------------------------
& cargo @CargoArgs
exit $LASTEXITCODE
