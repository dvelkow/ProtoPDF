# Draws the ProtoPDF icon and writes assets\icon.ico (several sizes) and assets\icon64.png.
Add-Type -AssemblyName System.Drawing
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path

function New-IconPng([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.TextRenderingHint = 'AntiAliasGridFit'
    $g.Clear([System.Drawing.Color]::Transparent)

    # Rounded green tile.
    $r = [Math]::Max(2, [int]($size * 0.2))
    $m = [Math]::Max(0, [int]($size * 0.03))
    $w = $size - 2 * $m - 1
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($m, $m, 2 * $r, 2 * $r, 180, 90)
    $path.AddArc($m + $w - 2 * $r, $m, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($m + $w - 2 * $r, $m + $w - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $path.AddArc($m, $m + $w - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure()
    $green = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 31, 138, 76))
    $g.FillPath($green, $path)

    # White band with the letters.
    $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)
    $fmt = New-Object System.Drawing.StringFormat
    $fmt.Alignment = 'Center'
    $fmt.LineAlignment = 'Center'
    $fontSize = [Math]::Max(5.0, $size * 0.34)
    $font = New-Object System.Drawing.Font 'Arial', $fontSize, ([System.Drawing.FontStyle]::Bold), ([System.Drawing.GraphicsUnit]::Pixel)
    $rect = New-Object System.Drawing.RectangleF 0, ($size * 0.04), $size, $size
    $g.DrawString('PDF', $font, $white, $rect, $fmt)
    $g.Dispose()

    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return ,$ms.ToArray()
}

$sizes = 16, 24, 32, 48, 64, 256
$pngs = @{}
foreach ($s in $sizes) { $pngs[$s] = New-IconPng $s }
[System.IO.File]::WriteAllBytes((Join-Path $dir 'icon64.png'), $pngs[64])

# ICO container holding the PNGs.
$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $out
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
    $b = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([Byte]$b); $bw.Write([Byte]$b); $bw.Write([Byte]0); $bw.Write([Byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32]$pngs[$s].Length); $bw.Write([UInt32]$offset)
    $offset += $pngs[$s].Length
}
foreach ($s in $sizes) { $bw.Write($pngs[$s]) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $dir 'icon.ico'), $out.ToArray())
"icon.ico $($out.Length) bytes"
