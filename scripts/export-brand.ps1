param()

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$repoRoot = Split-Path -Parent $PSScriptRoot
$brandRoot = Join-Path $repoRoot 'assets/brand/clatterforge'
$guiAssets = Join-Path $repoRoot 'gui/assets'
$nativeIcons = Join-Path $repoRoot 'gui/src-tauri/icons'
New-Item -ItemType Directory -Force -Path $brandRoot | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Format-Point([single]$X, [single]$Y) {
    [string]::Format([System.Globalization.CultureInfo]::InvariantCulture, '{0},{1}', $X, $Y)
}

# Shared polygon geometry drives both outlined SVG masters and raster exports.
$mark = @(
    @{ color = '#F2F5F7'; paths = @('380,180 840,180 732,288 426,288 288,426 288,610 362,684 362,436 466,436 466,486 700,486 612,574 466,574 466,706 736,706 840,820 354,820 184,650 184,376') },
    @{ color = '#FF754F'; paths = @('446,348 816,348 724,436 362,436') }
)
$appIcon = @(@{ color = '#0C1118'; paths = @('96,32 928,32 992,96 992,928 928,992 96,992 32,928 32,96') }) + $mark
$glyphs = @{
    C = @('16,0 84,0 84,22 28,22 22,28 22,72 28,78 84,78 84,100 16,100 0,84 0,16')
    L = @('0,0 22,0 22,78 84,78 84,100 0,100')
    A = @('0,100 34,0 58,0 92,100 68,100 60,78 32,78 24,100', '39,58 53,58 46,35')
    T = @('0,0 84,0 84,22 53,22 53,100 31,100 31,22 0,22')
    E = @('0,0 84,0 84,22 22,22 22,39 74,39 74,61 22,61 22,78 84,78 84,100 0,100')
    R = @('0,0 64,0 84,18 84,48 64,66 90,100 62,100 38,68 22,68 22,100 0,100', '22,22 22,46 56,46 62,40 62,28 56,22')
    F = @('0,0 84,0 84,22 22,22 22,42 74,42 74,64 22,64 22,100 0,100')
    O = @('16,0 68,0 84,16 84,84 68,100 16,100 0,84 0,16', '28,22 22,28 22,72 28,78 56,78 62,72 62,28 56,22')
    G = @('16,0 84,0 84,22 28,22 22,28 22,72 28,78 62,78 62,62 45,62 45,42 84,42 84,84 68,100 16,100 0,84 0,16')
    N = @('0,100 0,0 22,0 62,61 62,0 84,0 84,100 62,100 22,39 22,100')
    H = @('0,0 22,0 22,39 62,39 62,0 84,0 84,100 62,100 62,61 22,61 22,100 0,100')
    S = @('16,0 84,0 84,22 28,22 22,28 22,33 28,39 68,39 84,55 84,84 68,100 0,100 0,78 56,78 62,72 62,67 56,61 16,61 0,45 0,16')
    '.' = @('0,78 22,78 22,100 0,100')
}

function Get-Lettering([string]$Text, [single]$X, [single]$Y, [single]$Scale, [single]$Advance, [string]$Color) {
    foreach ($letter in $Text.ToCharArray()) {
        if ($letter -ne ' ') {
            $polygons = foreach ($polygon in $glyphs["$letter"]) {
                $points = foreach ($point in $polygon.Split(' ')) {
                    $xy = $point.Split(',')
                    Format-Point ($X + [single]$xy[0] * $Scale) ($Y + [single]$xy[1] * $Scale)
                }
                $points -join ' '
            }
            @{ color = $Color; paths = @($polygons) }
        }
        $X += $Advance * $Scale
    }
}

function Get-Logo([string]$Foreground) {
    foreach ($shape in $mark) {
        $polygons = foreach ($polygon in $shape.paths) {
            $points = foreach ($point in $polygon.Split(' ')) {
                $xy = $point.Split(',')
                Format-Point ([single]$xy[0] * 0.35 - 35) ([single]$xy[1] * 0.35 - 15)
            }
            $points -join ' '
        }
        @{ color = $(if ($shape.color -eq '#F2F5F7') { $Foreground } else { $shape.color }); paths = @($polygons) }
    }
    Get-Lettering 'CLATTERFORGE' 320 92 1 96 $Foreground
    Get-Lettering 'CONTROL THE CHAOS.' 510 235 0.3 144 '#8D9DAD'
}

function Write-Svg([string]$Name, [int]$Width, [int]$Height, [array]$Shapes, [string]$Background = '') {
    $body = if ($Background) { '<path fill="' + $Background + '" d="M0 0H' + $Width + 'V' + $Height + 'H0Z"/>' } else { '' }
    foreach ($shape in $Shapes) {
        $data = ($shape.paths | ForEach-Object { 'M' + ($_.Replace(' ', 'L')) + 'Z' }) -join ''
        $body += '<path fill="' + $shape.color + '" fill-rule="evenodd" d="' + $data + '"/>'
    }
    $svg = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ' + $Width + ' ' + $Height + '"><title>Clatterforge</title>' + $body + '</svg>'
    [System.IO.File]::WriteAllText((Join-Path $brandRoot $Name), $svg + "`n", $utf8)
}

function Write-Png([string]$Path, [int]$Width, [int]$Height, [int]$ViewWidth, [int]$ViewHeight, [array]$Shapes, [string]$Background = '') {
    $sample = if ($Width -le 256) { 4 } else { 2 }
    $large = New-Object System.Drawing.Bitmap(($Width * $sample), ($Height * $sample))
    $graphics = [System.Drawing.Graphics]::FromImage($large)
    $graphics.Clear([System.Drawing.Color]::Transparent)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    if ($Background) { $graphics.Clear([System.Drawing.ColorTranslator]::FromHtml($Background)) }
    $graphics.ScaleTransform(($Width * $sample / $ViewWidth), ($Height * $sample / $ViewHeight))
    foreach ($shape in $Shapes) {
        $drawingPath = New-Object System.Drawing.Drawing2D.GraphicsPath
        foreach ($polygon in $shape.paths) {
            [System.Drawing.PointF[]]$points = foreach ($point in $polygon.Split(' ')) {
                $xy = $point.Split(',')
                New-Object System.Drawing.PointF([single]$xy[0], [single]$xy[1])
            }
            $drawingPath.AddPolygon($points)
        }
        $brush = New-Object System.Drawing.SolidBrush([System.Drawing.ColorTranslator]::FromHtml($shape.color))
        $graphics.FillPath($brush, $drawingPath)
        $brush.Dispose()
        $drawingPath.Dispose()
    }
    $graphics.Dispose()
    $bitmap = New-Object System.Drawing.Bitmap($Width, $Height)
    $resize = [System.Drawing.Graphics]::FromImage($bitmap)
    $resize.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
    $resize.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $attributes = New-Object System.Drawing.Imaging.ImageAttributes
    $attributes.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY)
    $resize.DrawImage($large, (New-Object System.Drawing.Rectangle(0, 0, $Width, $Height)), 0, 0, $large.Width, $large.Height, [System.Drawing.GraphicsUnit]::Pixel, $attributes)
    $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $attributes.Dispose()
    $resize.Dispose()
    $bitmap.Dispose()
    $large.Dispose()
}

function Write-Ico([string]$Path) {
    $sizes = @(16, 24, 32, 48, 64, 128, 256)
    $writer = New-Object System.IO.BinaryWriter([System.IO.File]::Create($Path))
    $writer.Write([uint16]0)
    $writer.Write([uint16]1)
    $writer.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    foreach ($size in $sizes) {
        $bytes = [System.IO.File]::ReadAllBytes((Join-Path $brandRoot "clatterforge-icon-$size.png"))
        $dimension = if ($size -eq 256) { 0 } else { $size }
        $writer.Write([byte]$dimension)
        $writer.Write([byte]$dimension)
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]32)
        $writer.Write([uint32]$bytes.Length)
        $writer.Write([uint32]$offset)
        $offset += $bytes.Length
    }
    foreach ($size in $sizes) { $writer.Write([System.IO.File]::ReadAllBytes((Join-Path $brandRoot "clatterforge-icon-$size.png"))) }
    $writer.Dispose()
}

function Write-BigEndian([System.IO.BinaryWriter]$Writer, [int]$Value) {
    $bytes = [BitConverter]::GetBytes($Value)
    [array]::Reverse($bytes)
    $Writer.Write($bytes)
}

$logo = @(Get-Logo '#F2F5F7')
$logoLight = @(Get-Logo '#0C1118')
$markLight = @($mark | ForEach-Object { @{ color = $(if ($_.color -eq '#F2F5F7') { '#0C1118' } else { $_.color }); paths = $_.paths } })
Write-Svg 'clatterforge-mark.svg' 1024 1024 $mark
Write-Svg 'clatterforge-mark-light.svg' 1024 1024 $markLight
Write-Svg 'clatterforge-logo.svg' 1600 330 $logo
Write-Svg 'clatterforge-logo-light.svg' 1600 330 $logoLight
Write-Svg 'clatterforge-app-icon.svg' 1024 1024 $appIcon
Write-Png (Join-Path $brandRoot 'clatterforge-logo.png') 2400 495 1600 330 $logo
Write-Png (Join-Path $brandRoot 'clatterforge-logo-light.png') 2400 495 1600 330 $logoLight
Write-Png (Join-Path $brandRoot 'clatterforge-mark-light.png') 1024 1024 1024 1024 $markLight
foreach ($size in @(16, 24, 30, 32, 44, 48, 64, 71, 89, 107, 128, 142, 150, 180, 256, 284, 310, 512, 1024)) {
    Write-Png (Join-Path $brandRoot "clatterforge-mark-$size.png") $size $size 1024 1024 $mark
    Write-Png (Join-Path $brandRoot "clatterforge-icon-$size.png") $size $size 1024 1024 $appIcon
}
Write-Png (Join-Path $brandRoot 'clatterforge-app-icon.png') 1024 1024 1024 1024 $appIcon
Write-Png (Join-Path $guiAssets 'apple-touch-icon.png') 180 180 1024 1024 $mark '#0C1118'
Write-Ico (Join-Path $brandRoot 'clatterforge.ico')
Copy-Item (Join-Path $brandRoot 'clatterforge.ico') (Join-Path $guiAssets 'favicon.ico') -Force
Copy-Item (Join-Path $brandRoot 'clatterforge-logo.svg') (Join-Path $guiAssets 'clatterforge-logo.svg') -Force
Copy-Item (Join-Path $brandRoot 'clatterforge-logo.svg') (Join-Path $guiAssets 'header.svg') -Force
Copy-Item (Join-Path $brandRoot 'clatterforge-mark.svg') (Join-Path $guiAssets 'clatterforge-mark.svg') -Force
Copy-Item (Join-Path $brandRoot 'clatterforge.ico') (Join-Path $nativeIcons 'icon.ico') -Force
foreach ($entry in @(@('32x32.png',32), @('128x128.png',128), @('128x128@2x.png',256), @('icon.png',1024), @('StoreLogo.png',48))) {
    Copy-Item (Join-Path $brandRoot "clatterforge-icon-$($entry[1]).png") (Join-Path $nativeIcons $entry[0]) -Force
}
foreach ($size in @(30, 44, 71, 89, 107, 142, 150, 284, 310)) {
    Copy-Item (Join-Path $brandRoot "clatterforge-icon-$size.png") (Join-Path $nativeIcons "Square$($size)x$($size)Logo.png") -Force
}
$icnsSizes = [ordered]@{ icp4 = 16; icp5 = 32; icp6 = 64; ic07 = 128; ic08 = 256; ic09 = 512; ic10 = 1024 }
$icnsLength = 8
foreach ($size in $icnsSizes.Values) { $icnsLength += 8 + (Get-Item (Join-Path $brandRoot "clatterforge-icon-$size.png")).Length }
$icnsWriter = New-Object System.IO.BinaryWriter([System.IO.File]::Create((Join-Path $nativeIcons 'icon.icns')))
$icnsWriter.Write([System.Text.Encoding]::ASCII.GetBytes('icns'))
Write-BigEndian $icnsWriter $icnsLength
foreach ($entry in $icnsSizes.GetEnumerator()) {
    $bytes = [System.IO.File]::ReadAllBytes((Join-Path $brandRoot "clatterforge-icon-$($entry.Value).png"))
    $icnsWriter.Write([System.Text.Encoding]::ASCII.GetBytes($entry.Key))
    Write-BigEndian $icnsWriter (8 + $bytes.Length)
    $icnsWriter.Write($bytes)
}
$icnsWriter.Dispose()
Copy-Item (Join-Path $nativeIcons 'icon.icns') (Join-Path $brandRoot 'clatterforge.icns') -Force
Write-Output "Exported transparent/vector masters, GUI assets and native icons to $brandRoot"
