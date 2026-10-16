#Requires -Version 5.1
<#
.SYNOPSIS
  Construit l'installateur Windows d'Apex Supervision et son empreinte SHA-256.

.DESCRIPTION
  1. Lit la version unique du produit ([workspace.package] du Cargo.toml racine) ;
  2. compile le moteur et l'utilitaire en release (sauf -SkipBuild) ;
  3. compile le script Inno Setup ;
  4. écrit dist\<installateur>.exe.sha256.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1
#>
param(
    # Chemin de ISCC.exe (détecté automatiquement sinon)
    [string]$Iscc = "",
    # Réutilise les binaires déjà présents dans target\release
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

# Version unique du produit
$match = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $match) { throw "Version introuvable dans Cargo.toml ([workspace.package])" }
$version = $match.Matches[0].Groups[1].Value
# Version numérique Windows (4 entiers) : 0.4.0-alpha.1 -> 0.4.0.1
$core = ($version -split '-')[0]
$pre = if ($version -match '\.(\d+)$' -and $version.Contains('-')) { $Matches[1] } else { '0' }
$numeric = "$core.$pre"
Write-Host "Apex Supervision $version (version de fichier $numeric)"

if (-not $SkipBuild) {
    Push-Location $root
    try {
        cargo build --release --locked -p apex-server -p apex-admin
        if ($LASTEXITCODE -ne 0) { throw "Échec de la compilation (cargo build)" }
    } finally { Pop-Location }
}
foreach ($bin in 'apex-scada.exe', 'apex-scada-admin.exe') {
    $p = Join-Path $root "target\release\$bin"
    if (-not (Test-Path $p)) { throw "Binaire absent : $p" }
    # Ressources Windows (icône, VERSIONINFO) : absentes si le compilateur de ressources
    # manquait, ou binaire d'une autre version (-SkipBuild)
    $info = (Get-Item $p).VersionInfo
    if ($info.FileVersion -ne $numeric -or $info.ProductVersion -ne $version) {
        throw "Version de fichier de $bin : « $($info.FileVersion) » / « $($info.ProductVersion) », attendu « $numeric » / « $version » (recompilez ; windres ou Windows SDK requis)"
    }
}

if (-not $Iscc) {
    $candidates = @(
        (Get-Command iscc.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
    ) | Where-Object { $_ -and (Test-Path $_) }
    $Iscc = $candidates | Select-Object -First 1
}
if (-not $Iscc) { throw "Inno Setup 6 introuvable : winget install JRSoftware.InnoSetup, ou précisez -Iscc" }

& $Iscc "/DAppVersion=$version" "/DNumericVersion=$numeric" "/DSourceDir=$root" (Join-Path $PSScriptRoot 'apex-supervision.iss')
if ($LASTEXITCODE -ne 0) { throw "Échec de la compilation de l'installateur (ISCC)" }

$setup = Join-Path $root "dist\apex-supervision-$version-windows-x64-setup.exe"
$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
# Format compatible avec « sha256sum -c »
"$hash  $(Split-Path $setup -Leaf)" | Out-File -Encoding ascii "$setup.sha256"
Write-Host "Installateur : $setup"
Write-Host "SHA-256      : $hash"
