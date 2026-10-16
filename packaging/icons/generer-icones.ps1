#Requires -Version 5.1
<#
.SYNOPSIS
  Génère l'icône d'Apex Supervision : SVG, PNG 256 px et ICO multi-tailles.

.DESCRIPTION
  L'icône est entièrement décrite ici (aucune ressource externe, aucune marque) : un carré
  aux angles arrondis bleu nuit, un sommet « Λ » blanc (l'apex) et un voyant vert (état de
  marche). La même géométrie, exprimée dans un repère de 256 × 256, produit :
    - apex-supervision.svg      version vectorielle (paquet Linux, documentation) ;
    - apex-supervision-256.png  fenêtre de l'utilitaire, paquet Linux (hicolor 256x256) ;
    - apex-supervision.ico      16, 24, 32, 48, 64 (BMP 32 bits) et 256 px (PNG) :
                                ressources des exécutables Windows et installateur.
  Chaque taille est dessinée directement (pas de réduction d'une grande image) avec
  l'anticrénelage de System.Drawing. Les fichiers produits sont versionnés : relancer ce
  script après toute modification de la géométrie, puis recompiler.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File packaging\icons\generer-icones.ps1
#>
param(
    # Dossier de sortie (par défaut : celui du script)
    [string]$Sortie = $PSScriptRoot
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# --- Géométrie (repère 256 × 256), source unique du dessin ---------------------------------
$Fond      = @{ X = 8; Y = 8; Cote = 240; Rayon = 52; Couleur = '#17375E' }
$Sommet    = @{ Points = @(@(56, 188), @(128, 64), @(200, 188)); Epaisseur = 26; Couleur = '#FFFFFF' }
$Voyant    = @{ X = 128; Y = 166; Rayon = 20; Couleur = '#3DDC84' }
$TaillesIco = 16, 24, 32, 48, 64, 256

function ConvertTo-Couleur([string]$hex) {
    [System.Drawing.ColorTranslator]::FromHtml($hex)
}

# Dessine l'icône à la taille demandée (pixels), sur fond transparent.
function New-Icone([int]$taille) {
    $bmp = New-Object System.Drawing.Bitmap($taille, $taille, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $g.Clear([System.Drawing.Color]::Transparent)
        $g.ScaleTransform($taille / 256.0, $taille / 256.0)

        # Fond : carré aux angles arrondis
        $d = 2 * $Fond.Rayon
        $x0 = $Fond.X; $y0 = $Fond.Y; $x1 = $Fond.X + $Fond.Cote - $d; $y1 = $Fond.Y + $Fond.Cote - $d
        $chemin = New-Object System.Drawing.Drawing2D.GraphicsPath
        $chemin.AddArc($x0, $y0, $d, $d, 180, 90)
        $chemin.AddArc($x1, $y0, $d, $d, 270, 90)
        $chemin.AddArc($x1, $y1, $d, $d, 0, 90)
        $chemin.AddArc($x0, $y1, $d, $d, 90, 90)
        $chemin.CloseFigure()
        $pinceau = New-Object System.Drawing.SolidBrush (ConvertTo-Couleur $Fond.Couleur)
        $g.FillPath($pinceau, $chemin)
        $pinceau.Dispose(); $chemin.Dispose()

        # Sommet « Λ »
        $stylo = New-Object System.Drawing.Pen((ConvertTo-Couleur $Sommet.Couleur), [single]$Sommet.Epaisseur)
        $stylo.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $stylo.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $stylo.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
        [System.Drawing.PointF[]]$points = $Sommet.Points | ForEach-Object { New-Object System.Drawing.PointF($_[0], $_[1]) }
        $g.DrawLines($stylo, $points)
        $stylo.Dispose()

        # Voyant d'état
        $r = $Voyant.Rayon
        $pinceau = New-Object System.Drawing.SolidBrush (ConvertTo-Couleur $Voyant.Couleur)
        $g.FillEllipse($pinceau, $Voyant.X - $r, $Voyant.Y - $r, 2 * $r, 2 * $r)
        $pinceau.Dispose()
    } finally {
        $g.Dispose()
    }
    return $bmp
}

function Get-Png([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return , $ms.ToArray()
}

# Image d'icône au format DIB : BITMAPINFOHEADER, pixels BGRA de bas en haut, masque AND
# (entièrement transparent : la couche alpha fait foi).
function Get-Dib([System.Drawing.Bitmap]$bmp) {
    $t = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle(0, 0, $t, $t)
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $ligne = $t * 4
    $pixels = New-Object byte[] ($ligne * $t)
    try {
        for ($y = 0; $y -lt $t; $y++) {
            $src = [IntPtr]::Add($data.Scan0, $y * $data.Stride)
            # Lignes inversées : le DIB commence par la ligne du bas
            [System.Runtime.InteropServices.Marshal]::Copy($src, $pixels, ($t - 1 - $y) * $ligne, $ligne)
        }
    } finally {
        $bmp.UnlockBits($data)
    }
    $masque = New-Object byte[] ([int]([math]::Ceiling($t / 32.0) * 4 * $t))
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter($ms)
    $w.Write([uint32]40)                       # biSize
    $w.Write([int32]$t)                        # biWidth
    $w.Write([int32](2 * $t))                  # biHeight : image + masque
    $w.Write([uint16]1)                        # biPlanes
    $w.Write([uint16]32)                       # biBitCount
    $w.Write([uint32]0)                        # biCompression (BI_RGB)
    $w.Write([uint32]($pixels.Length + $masque.Length))
    $w.Write([int32]0); $w.Write([int32]0)     # résolution
    $w.Write([uint32]0); $w.Write([uint32]0)   # palette
    $w.Write($pixels)
    $w.Write($masque)
    $w.Flush()
    return , $ms.ToArray()
}

function Write-Octets([string]$nom, [byte[]]$octets) {
    $chemin = Join-Path $Sortie $nom
    [System.IO.File]::WriteAllBytes($chemin, $octets)
    $empreinte = (Get-FileHash -Algorithm SHA256 $chemin).Hash.Substring(0, 16).ToLower()
    Write-Host ("{0,-26} {1,7} octets  sha256:{2}..." -f $nom, $octets.Length, $empreinte)
}

New-Item -ItemType Directory -Force $Sortie | Out-Null

# --- SVG ------------------------------------------------------------------------------------
$pts = ($Sommet.Points | ForEach-Object { "$($_[0]),$($_[1])" }) -join ' '
$svg = @(
    '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">'
    '  <title>Apex Supervision</title>'
    "  <rect x=`"$($Fond.X)`" y=`"$($Fond.Y)`" width=`"$($Fond.Cote)`" height=`"$($Fond.Cote)`" rx=`"$($Fond.Rayon)`" fill=`"$($Fond.Couleur)`"/>"
    "  <polyline points=`"$pts`" fill=`"none`" stroke=`"$($Sommet.Couleur)`" stroke-width=`"$($Sommet.Epaisseur)`" stroke-linecap=`"round`" stroke-linejoin=`"round`"/>"
    "  <circle cx=`"$($Voyant.X)`" cy=`"$($Voyant.Y)`" r=`"$($Voyant.Rayon)`" fill=`"$($Voyant.Couleur)`"/>"
    '</svg>'
    ''
) -join "`n"
Write-Octets 'apex-supervision.svg' ([System.Text.Encoding]::UTF8.GetBytes($svg))

# --- PNG 256 px -----------------------------------------------------------------------------
$grande = New-Icone 256
Write-Octets 'apex-supervision-256.png' (Get-Png $grande)
$grande.Dispose()

# --- ICO multi-tailles ----------------------------------------------------------------------
$images = foreach ($t in $TaillesIco) {
    $bmp = New-Icone $t
    # Convention Windows : PNG pour 256 px (taille), DIB pour les petites tailles (compatibilité)
    if ($t -ge 256) { , (Get-Png $bmp) } else { , (Get-Dib $bmp) }
    $bmp.Dispose()
}
$ms = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter($ms)
$w.Write([uint16]0)                    # réservé
$w.Write([uint16]1)                    # type : icône
$w.Write([uint16]$TaillesIco.Count)
$offset = 6 + 16 * $TaillesIco.Count
for ($i = 0; $i -lt $TaillesIco.Count; $i++) {
    $t = $TaillesIco[$i]
    $octet = if ($t -ge 256) { 0 } else { $t }  # 0 signifie 256
    $w.Write([byte]$octet); $w.Write([byte]$octet)
    $w.Write([byte]0); $w.Write([byte]0)        # palette, réservé
    $w.Write([uint16]1); $w.Write([uint16]32)   # plans, bits par pixel
    $w.Write([uint32]$images[$i].Length)
    $w.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $w.Write([byte[]]$img) }
$w.Flush()
Write-Octets 'apex-supervision.ico' $ms.ToArray()
