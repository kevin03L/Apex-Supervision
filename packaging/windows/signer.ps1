#Requires -Version 5.1
<#
.SYNOPSIS
  Signe ou vérifie la signature Authenticode d'un fichier binaire Windows.

.DESCRIPTION
  Prend en charge trois modes de signature selon les variables d'environnement :
  1. Certificat dans un fichier PFX :
     APEX_SIGN_PFX (chemin) et APEX_SIGN_PFX_PASSWORD (mot de passe optionnel).
  2. Certificat dans le magasin utilisateur (jeton matériel ou service cloud) :
     APEX_SIGN_THUMBPRINT (empreinte dans Cert:\CurrentUser\My).
  3. Azure Artifact Signing :
     APEX_SIGN_ARTIFACT_METADATA (fichier JSON), APEX_SIGNTOOL (chemin signtool.exe),
     APEX_SIGN_DLIB (chemin Azure.CodeSigning.Dlib.dll).

  L'horodatage RFC 3161 est obligatoire (APEX_SIGN_TIMESTAMP, http://timestamp.digicert.com par défaut).
  Le mode -Verifier contrôle la signature sans modifier le fichier.
  Le mode d'essai APEX_SIGN_TEST=1 accepte les certificats de test auto-signés avec un avertissement.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File packaging\windows\signer.ps1 target\release\apex-scada.exe
  powershell -ExecutionPolicy Bypass -File packaging\windows\signer.ps1 -Verifier dist\apex-supervision-0.4.0.1-windows-x64-setup.exe
#>
param(
    # Fichier à signer ou à vérifier
    [Parameter(Position = 0, Mandatory = $true)]
    [string]$Fichier,

    # Vérifie la signature du fichier sans le signer
    [switch]$Verifier
)

$ErrorActionPreference = 'Stop'

function Valider-Certificat([System.Security.Cryptography.X509Certificates.X509Certificate2]$cert) {
    # 1. Clé privée
    if (-not $cert.HasPrivateKey) {
        Write-Error "Erreur : le certificat '$($cert.Subject)' ne possède pas de clé privée."
        exit 1
    }

    # 2. Expiration
    $now = [DateTime]::UtcNow
    if ($now -lt $cert.NotBefore.ToUniversalTime() -or $now -gt $cert.NotAfter.ToUniversalTime()) {
        Write-Error "Erreur : le certificat '$($cert.Subject)' est expiré ou pas encore valide (valide du $($cert.NotBefore.ToString('yyyy-MM-dd')) au $($cert.NotAfter.ToString('yyyy-MM-dd')))."
        exit 1
    }

    # 3. Usage « signature de code » (EKU 1.3.6.1.5.5.7.3.3)
    $hasCodeSigning = $false
    foreach ($ext in $cert.Extensions) {
        if ($ext.Oid.Value -eq '2.5.29.37') {
            $ekuExt = [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]$ext
            foreach ($usage in $ekuExt.EnhancedKeyUsages) {
                if ($usage.Value -eq '1.3.6.1.5.5.7.3.3') {
                    $hasCodeSigning = $true
                    break
                }
            }
        }
    }
    if (-not $hasCodeSigning) {
        Write-Error "Erreur : le certificat '$($cert.Subject)' ne dispose pas de l'usage « signature de code » (EKU 1.3.6.1.5.5.7.3.3)."
        exit 1
    }

    # 4. Clé RSA obligatoire (Smart App Control ne prend pas en charge ECC)
    $algOid = $cert.PublicKey.Oid.Value
    $algFriendly = $cert.PublicKey.Oid.FriendlyName
    if ($algOid -ne '1.2.840.113549.1.1.1' -and $algFriendly -ne 'RSA') {
        Write-Error "Erreur : le certificat utilise une clé $algFriendly ($algOid). Smart App Control ne prend pas en charge ECC : une clé RSA est obligatoire."
        exit 1
    }
}

function Verifier-Fichier([string]$cible, [string]$expectedThumbprint = "") {
    if (-not (Test-Path $cible)) {
        Write-Error "Erreur : fichier introuvable pour vérification : $cible"
        exit 1
    }
    $sig = Get-AuthenticodeSignature -FilePath $cible
    if ($sig.Status -eq [System.Management.Automation.SignatureStatus]::NotSigned -or $null -eq $sig.SignerCertificate) {
        Write-Error "Erreur : le fichier n'est pas signé : $cible"
        exit 1
    }

    if ($null -eq $sig.TimeStamperCertificate) {
        Write-Error "Erreur : aucun certificat d'horodatage RFC 3161 présent sur la signature de $cible."
        exit 1
    }

    # Vérification optionnelle de l'empreinte si renseignée ou dérivée de l'environnement
    if (-not $expectedThumbprint) {
        if ($env:APEX_SIGN_THUMBPRINT) {
            $expectedThumbprint = $env:APEX_SIGN_THUMBPRINT.Trim()
        } elseif ($env:APEX_SIGN_PFX -and (Test-Path $env:APEX_SIGN_PFX)) {
            try {
                $tmpCert = if ($env:APEX_SIGN_PFX_PASSWORD) {
                    New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($env:APEX_SIGN_PFX, $env:APEX_SIGN_PFX_PASSWORD)
                } else {
                    New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($env:APEX_SIGN_PFX)
                }
                $expectedThumbprint = $tmpCert.Thumbprint
            } catch {
                # Ne bloque pas la vérification si le mot de passe n'est pas fourni en mode vérification seule
            }
        }
    }

    if ($expectedThumbprint -and ($sig.SignerCertificate.Thumbprint -ne $expectedThumbprint)) {
        Write-Error "Erreur : l'empreinte du signataire ($($sig.SignerCertificate.Thumbprint)) ne correspond pas à l'empreinte attendue ($expectedThumbprint)."
        exit 1
    }

    if ($sig.Status -eq [System.Management.Automation.SignatureStatus]::Valid) {
        Write-Host "Signature Authenticode valide : $cible"
        Write-Host "  Signataire : $($sig.SignerCertificate.Subject) (empreinte $($sig.SignerCertificate.Thumbprint))"
        Write-Host "  Horodatage : $($sig.TimeStamperCertificate.Subject) (empreinte $($sig.TimeStamperCertificate.Thumbprint))"
    } elseif ($env:APEX_SIGN_TEST -eq '1' -and ($sig.Status -eq [System.Management.Automation.SignatureStatus]::UnknownError -or $sig.Status -eq [System.Management.Automation.SignatureStatus]::NotTrusted)) {
        Write-Warning "Avertissement (mode d'essai APEX_SIGN_TEST=1) : signature acceptée bien que la chaîne ne soit pas approuvée ($($sig.StatusMessage))."
        Write-Host "  Signataire : $($sig.SignerCertificate.Subject) (empreinte $($sig.SignerCertificate.Thumbprint))"
        Write-Host "  Horodatage : $($sig.TimeStamperCertificate.Subject) (empreinte $($sig.TimeStamperCertificate.Thumbprint))"
    } else {
        Write-Error "Erreur : signature Authenticode invalide pour $cible ($($sig.Status)) : $($sig.StatusMessage)"
        exit 1
    }
    return $sig
}

# Résolution du fichier cible
if (-not (Test-Path $Fichier)) {
    Write-Error "Erreur : fichier introuvable : $Fichier"
    exit 1
}
$targetPath = (Resolve-Path $Fichier).Path

# Mode vérification seule
if ($Verifier) {
    Verifier-Fichier $targetPath
    exit 0
}

# Mode signature
$timestamp = if ($env:APEX_SIGN_TIMESTAMP) { $env:APEX_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }

if ($env:APEX_SIGN_ARTIFACT_METADATA) {
    # Azure Artifact Signing via signtool.exe
    $metadata = $env:APEX_SIGN_ARTIFACT_METADATA
    if (-not (Test-Path $metadata)) {
        Write-Error "Erreur : fichier de métadonnées d'artefact introuvable : $metadata"
        exit 1
    }
    $signtool = $env:APEX_SIGNTOOL
    if (-not $signtool) {
        $found = Get-Command signtool.exe -ErrorAction SilentlyContinue
        if ($found) { $signtool = $found.Source }
    }
    if (-not $signtool -or -not (Test-Path $signtool)) {
        Write-Error "Erreur : signtool.exe introuvable. Définissez APEX_SIGNTOOL ou installez le Windows SDK."
        exit 1
    }
    $dlib = $env:APEX_SIGN_DLIB
    if (-not $dlib -or -not (Test-Path $dlib)) {
        Write-Error "Erreur : Azure.CodeSigning.Dlib.dll introuvable. Définissez APEX_SIGN_DLIB."
        exit 1
    }

    Write-Host "Signature Authenticode de $targetPath via Azure Artifact Signing..."
    & $signtool sign /v /fd SHA256 /tr $timestamp /td SHA256 /dlib $dlib /dmdf $metadata $targetPath
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Erreur : échec de la signature avec signtool (code de sortie $LASTEXITCODE)."
        exit 1
    }
    Verifier-Fichier $targetPath
    exit 0
}

$cert = $null
if ($env:APEX_SIGN_PFX) {
    $pfxPath = $env:APEX_SIGN_PFX
    if (-not (Test-Path $pfxPath)) {
        $altPath = Join-Path $PSScriptRoot "..\..\$pfxPath"
        if (Test-Path $altPath) {
            $pfxPath = (Resolve-Path $altPath).Path
        } else {
            Write-Error "Erreur : fichier PFX introuvable : $pfxPath"
            exit 1
        }
    }
    try {
        $pfxPassword = $env:APEX_SIGN_PFX_PASSWORD
        if ([string]::IsNullOrEmpty($pfxPassword)) {
            $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($pfxPath)
        } else {
            $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($pfxPath, $pfxPassword)
        }
    } catch {
        # Ne jamais afficher le mot de passe
        Write-Error "Erreur : impossible de charger le certificat PFX '$pfxPath'. Vérifiez le mot de passe et l'intégrité du fichier."
        exit 1
    }
} elseif ($env:APEX_SIGN_THUMBPRINT) {
    $thumb = $env:APEX_SIGN_THUMBPRINT.Trim()
    $cert = Get-Item "Cert:\CurrentUser\My\$thumb" -ErrorAction SilentlyContinue
    if (-not $cert) {
        $cert = Get-ChildItem "Cert:\CurrentUser\My" -ErrorAction SilentlyContinue | Where-Object { $_.Thumbprint -eq $thumb } | Select-Object -First 1
    }
    if (-not $cert) {
        Write-Error "Erreur : certificat avec l'empreinte '$thumb' introuvable dans Cert:\CurrentUser\My."
        exit 1
    }
} else {
    Write-Error "Erreur : aucune configuration de signature définie (APEX_SIGN_PFX, APEX_SIGN_THUMBPRINT ou APEX_SIGN_ARTIFACT_METADATA requis)."
    exit 1
}

# Contrôle des exigences de sécurité Authenticode
Valider-Certificat $cert

# Application de la signature Authenticode
Write-Host "Signature Authenticode de $targetPath avec le certificat $($cert.Subject) (empreinte $($cert.Thumbprint))..."
try {
    $sigResult = Set-AuthenticodeSignature -FilePath $targetPath -Certificate $cert -HashAlgorithm SHA256 -TimestampServer $timestamp
} catch {
    Write-Error "Erreur lors de l'application de la signature Authenticode : $_"
    exit 1
}

# Contrôle après signature
Verifier-Fichier $targetPath -expectedThumbprint $cert.Thumbprint
exit 0
