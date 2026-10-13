# Draws assets\scry.ico: a white magnifier on a blue tile, drawn afresh
# at every size Windows asks for (PNG images inside the .ico). build.rs
# embeds it in both programs.
#
#   powershell -File assets\icon.ps1 [-Preview sheet.png]

param([string]$Preview)

Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = 'Stop'

$sizes = 16, 20, 24, 32, 40, 48, 64, 256

function Draw-Icon([int]$s) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)

    # The tile: full bleed at tray sizes, a margin at the big ones.
    $m = if ($s -ge 24) { [math]::Floor($s / 16) } else { 0 }
    $t = $s - 2 * $m
    $r = [math]::Max(2.0, 0.22 * $t)
    $tile = New-Object System.Drawing.Drawing2D.GraphicsPath
    $tile.AddArc($m, $m, 2 * $r, 2 * $r, 180, 90)
    $tile.AddArc($m + $t - 2 * $r, $m, 2 * $r, 2 * $r, 270, 90)
    $tile.AddArc($m + $t - 2 * $r, $m + $t - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $tile.AddArc($m, $m + $t - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $tile.CloseFigure()
    $top = [System.Drawing.Color]::FromArgb(255, 0x3A, 0x8E, 0xE6)
    $bottom = [System.Drawing.Color]::FromArgb(255, 0x0B, 0x5C, 0xAD)
    $p0 = New-Object System.Drawing.PointF $m, $m
    $p1 = New-Object System.Drawing.PointF ($m + $t), ($m + $t)
    $fill = New-Object System.Drawing.Drawing2D.LinearGradientBrush $p0, $p1, $top, $bottom
    $g.FillPath($fill, $tile)

    # The magnifier: a ring up and left, the handle down to the corner.
    $w = [math]::Max(1.6, 0.095 * $t)
    $c = $m + 0.43 * $t
    $lr = 0.2 * $t
    $white = [System.Drawing.Color]::White
    $ring = New-Object System.Drawing.Pen $white, $w
    $g.DrawEllipse($ring, $c - $lr, $c - $lr, 2 * $lr, 2 * $lr)
    $handle = New-Object System.Drawing.Pen $white, (1.4 * $w)
    $handle.StartCap = 'Round'
    $handle.EndCap = 'Round'
    $from = $c + ($lr + $w / 2) * 0.7071
    $to = $m + 0.76 * $t
    $g.DrawLine($handle, $from, $from, $to, $to)

    $g.Dispose()
    return $bmp
}

$images = foreach ($s in $sizes) {
    $bmp = Draw-Icon $s
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}

# ICONDIR, one ICONDIRENTRY per image, then the images.
$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $out
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $side = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
    $bw.Write([byte]$side); $bw.Write([byte]$side); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]$images[$i].Length); $bw.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'scry.ico'), $out.ToArray())

if ($Preview) {
    # Every size as it is, on dark and on light, and the small ones blown
    # up 8x to judge their pixels.
    $sheet = New-Object System.Drawing.Bitmap 1160, 560
    $g = [System.Drawing.Graphics]::FromImage($sheet)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 0x20, 0x20, 0x20))
    $g.FillRectangle([System.Drawing.Brushes]::WhiteSmoke, 0, 280, 1160, 280)
    $g.InterpolationMode = 'NearestNeighbor'
    $g.PixelOffsetMode = 'Half'
    $x = 10
    foreach ($s in $sizes) {
        $bmp = Draw-Icon $s
        $g.DrawImage($bmp, $x, 10, $s, $s)
        $g.DrawImage($bmp, $x, 290, $s, $s)
        $x += $s + 12
        $bmp.Dispose()
    }
    $x = 620
    foreach ($s in 16, 20, 24) {
        $bmp = Draw-Icon $s
        $g.DrawImage($bmp, $x, 10, 8 * $s, 8 * $s)
        $g.DrawImage($bmp, $x, 290, 8 * $s, 8 * $s)
        $x += 8 * $s + 20
        $bmp.Dispose()
    }
    $g.Dispose()
    $sheet.Save($Preview, [System.Drawing.Imaging.ImageFormat]::Png)
}
