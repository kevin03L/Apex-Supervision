#Requires -Version 5.1
<#
.SYNOPSIS
  Construit l'installateur Windows d'Apex Supervision et son empreinte SHA-256.

.DESCRIPTION
  1. Lit la version unique du produit ([workspace.package] du Cargo.toml racine) ;
  2. compile le moteur et l'utilitaire en release (sauf -SkipBuild) ;
  3. signe Authenticode les binaires release si la signature est active ;
  4. compile le script Inno Setup (avec signature de l'installateur et du désinstalleur) ;
  5. contrôle la signature de l'installateur avec signer.ps1 -Verifier ;
  6. écrit dist\<installateur>.exe.sha256.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1
  powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1 -Signer
  powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1 -SansSignature
#>
param(
    # Chemin de ISCC.exe (détecté automatiquement sinon)
    [string]$Iscc = "",
    # Réutilise les binaires déjà présents dans target\release
    [switch]$SkipBuild,
    # Exige la signature Authenticode (échec si aucune configuration n'est présente)
    [switch]$Signer,
    # Force la construction d'un installateur non signé
    [switch]$SansSignature,
    # Recalcule uniquement l'empreinte .sha256 de l'installateur (post-signature SignPath)
    [switch]$RecalculerSha256
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

if ($RecalculerSha256) {
    $match = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $match) { throw "Version introuvable dans Cargo.toml ([workspace.package])" }
    $version = $match.Matches[0].Groups[1].Value
    $setup = Join-Path $root "dist\apex-supervision-$version-windows-x64-setup.exe"
    if (-not (Test-Path $setup)) {
        $found = Get-ChildItem -Path (Join-Path $root "dist") -Filter "apex-supervision-*-windows-x64-setup.exe" | Select-Object -First 1
        if ($found) { $setup = $found.FullName } else { throw "Installateur introuvable : $setup" }
    }
    $hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
    "$hash  $(Split-Path $setup -Leaf)" | Out-File -Encoding ascii "$setup.sha256"
    Write-Host "Empreinte SHA-256 recalculée pour $(Split-Path $setup -Leaf) : $hash"
    return
}

if ($Signer -and $SansSignature) {
    throw "Les commutateurs -Signer et -SansSignature sont mutuellement exclusifs."
}

# Détermination du mode de signature
$hasSignConfig = [bool]($env:APEX_SIGN_PFX -or $env:APEX_SIGN_THUMBPRINT -or $env:APEX_SIGN_ARTIFACT_METADATA)

$activerSignature = $false
if ($SansSignature) {
    $activerSignature = $false
    Write-Warning "Installateur NON signé : bloqué par Smart App Control."
} elseif ($Signer) {
    if (-not $hasSignConfig) {
        throw "Signature exigée (-Signer) mais aucune configuration de signature n'est présente (définissez APEX_SIGN_PFX, APEX_SIGN_THUMBPRINT ou APEX_SIGN_ARTIFACT_METADATA)."
    }
    $activerSignature = $true
} else {
    if ($hasSignConfig) {
        $activerSignature = $true
    } else {
        $activerSignature = $false
        Write-Warning "Installateur NON signé : bloqué par Smart App Control."
    }
}

# Normalisation des chemins relatifs en chemins absolus pour les processus enfants (ISCC)
if ($env:APEX_SIGN_PFX -and (Test-Path $env:APEX_SIGN_PFX)) {
    $env:APEX_SIGN_PFX = (Resolve-Path $env:APEX_SIGN_PFX).Path
}
if ($env:APEX_SIGN_ARTIFACT_METADATA -and (Test-Path $env:APEX_SIGN_ARTIFACT_METADATA)) {
    $env:APEX_SIGN_ARTIFACT_METADATA = (Resolve-Path $env:APEX_SIGN_ARTIFACT_METADATA).Path
}

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

$signerScript = Join-Path $PSScriptRoot 'signer.ps1'

# Signature des binaires avant compilation de l'installateur
if ($activerSignature) {
    # Détermination de l'empreinte attendue pour éviter de resigner inutilement (-SkipBuild)
    $targetThumbprint = $null
    if ($env:APEX_SIGN_PFX -and (Test-Path $env:APEX_SIGN_PFX)) {
        try {
            $tmpCert = if ($env:APEX_SIGN_PFX_PASSWORD) {
                New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($env:APEX_SIGN_PFX, $env:APEX_SIGN_PFX_PASSWORD)
            } else {
                New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($env:APEX_SIGN_PFX)
            }
            $targetThumbprint = $tmpCert.Thumbprint
        } catch {
            # La validation détaillée sera effectuée dans signer.ps1
        }
    } elseif ($env:APEX_SIGN_THUMBPRINT) {
        $targetThumbprint = $env:APEX_SIGN_THUMBPRINT.Trim()
    }

    foreach ($bin in 'apex-scada.exe', 'apex-scada-admin.exe') {
        $p = Join-Path $root "target\release\$bin"
        $dejaSigne = $false
        $sig = Get-AuthenticodeSignature -FilePath $p
        if ($sig.Status -ne [System.Management.Automation.SignatureStatus]::NotSigned -and $null -ne $sig.SignerCertificate -and $null -ne $sig.TimeStamperCertificate) {
            if ($targetThumbprint) {
                if ($sig.SignerCertificate.Thumbprint -eq $targetThumbprint) {
                    $dejaSigne = $true
                }
            } else {
                if ($sig.Status -eq [System.Management.Automation.SignatureStatus]::Valid -or ($env:APEX_SIGN_TEST -eq '1' -and ($sig.Status -eq [System.Management.Automation.SignatureStatus]::UnknownError -or $sig.Status -eq [System.Management.Automation.SignatureStatus]::NotTrusted))) {
                    $dejaSigne = $true
                }
            }
        }

        if ($dejaSigne) {
            Write-Host "$bin est déjà signé avec le certificat $($sig.SignerCertificate.Thumbprint)."
        } else {
            Write-Host "Signature de $bin..."
            & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $signerScript $p
            if ($LASTEXITCODE -ne 0) { throw "Échec de la signature de $bin" }
        }
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

$isccArgs = @(
    "/DAppVersion=$version",
    "/DNumericVersion=$numeric",
    "/DSourceDir=$root"
)
if ($activerSignature) {
    $isccArgs += "/DSigner=1"
    $isccArgs += "/Sapexsign=powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$signerScript`" `$f"
}
$isccArgs += (Join-Path $PSScriptRoot 'apex-supervision.iss')

& $Iscc @isccArgs
if ($LASTEXITCODE -ne 0) { throw "Échec de la compilation de l'installateur (ISCC)" }

$setup = Join-Path $root "dist\apex-supervision-$version-windows-x64-setup.exe"
if (-not (Test-Path $setup)) { throw "Installateur absent : $setup" }

$signatureResume = "NON signé"
if ($activerSignature) {
    Write-Host "Vérification de la signature de l'installateur..."
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $signerScript -Verifier $setup
    if ($LASTEXITCODE -ne 0) { throw "Échec de la vérification de la signature de l'installateur ($setup)" }
    $setupSig = Get-AuthenticodeSignature -FilePath $setup
    if ($setupSig.SignerCertificate) {
        $signatureResume = "signé par $($setupSig.SignerCertificate.Subject) (empreinte $($setupSig.SignerCertificate.Thumbprint))"
    } else {
        $signatureResume = "signé"
    }
}

$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
# Format compatible avec « sha256sum -c »
"$hash  $(Split-Path $setup -Leaf)" | Out-File -Encoding ascii "$setup.sha256"
Write-Host "Installateur : $setup"
Write-Host "SHA-256      : $hash"
Write-Host "Signature    : $signatureResume"
