# Draws the ProtoPDF icon (a white sheet with a red "PDF" label) and writes
# assets\icon.ico (several sizes) and assets\icon.png (128 px, used for the window).
# The artwork is laid out on a 256 unit grid and scaled to each size. The two
# smallest sizes use a simplified drawing so the letters stay readable.
Add-Type -AssemblyName System.Drawing
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path

function Color([string]$hex, [int]$alpha = 255) {
    $c = [System.Drawing.ColorTranslator]::FromHtml($hex)
    [System.Drawing.Color]::FromArgb($alpha, $c.R, $c.G, $c.B)
}

function RoundRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    $p
}

function New-IconPng([int]$size) {
    $small = $size -le 24
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.TextRenderingHint = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.ScaleTransform($size / 256.0, $size / 256.0)

    # The sheet with its folded corner.
    $edge = if ($small) { 12 } else { 5 }
    $pen = New-Object System.Drawing.Pen (Color '#b9bfc8'), $edge
    $pen.LineJoin = 'Round'
    $sheet = New-Object System.Drawing.Drawing2D.GraphicsPath
    $sheet.AddLine(60, 14, 160, 14)
    $sheet.AddLine(160, 14, 212, 66)
    $sheet.AddLine(212, 66, 212, 230)
    $sheet.AddArc(188, 218, 24, 24, 0, 90)
    $sheet.AddLine(200, 242, 60, 242)
    $sheet.AddArc(48, 218, 24, 24, 90, 90)
    $sheet.AddLine(48, 230, 48, 26)
    $sheet.AddArc(48, 14, 24, 24, 180, 90)
    $sheet.CloseFigure()
    $paper = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF 0, 14), (New-Object System.Drawing.PointF 0, 242), (Color '#ffffff'), (Color '#eef0f3')
    $g.FillPath($paper, $sheet)
    $g.DrawPath($pen, $sheet)

    $fold = New-Object System.Drawing.Drawing2D.GraphicsPath
    $fold.AddLine(160, 14, 160, 54)
    $fold.AddArc(160, 42, 24, 24, 180, -90)
    $fold.AddLine(172, 66, 212, 66)
    $fold.CloseFigure()
    $g.FillPath((New-Object System.Drawing.SolidBrush (Color '#d5d9e0')), $fold)
    $g.DrawPath($pen, $fold)

    # Two lines of "text" on the sheet, only where there is room for them.
    if ($size -ge 48) {
        $grey = New-Object System.Drawing.SolidBrush (Color '#cfd4db')
        $g.FillPath($grey, (RoundRect 74 88 108 10 5))
        $g.FillPath($grey, (RoundRect 74 110 76 10 5))
    }

    # The red label and its letters.
    if ($small) { $bx = 4; $by = 108; $bw = 248; $bh = 128; $br = 20; $fs = 104 }
    else        { $bx = 16; $by = 142; $bw = 224; $bh = 84; $br = 16; $fs = 68 }
    $band = RoundRect $bx $by $bw $bh $br
    $red = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF 0, ($by - 1)), (New-Object System.Drawing.PointF 0, ($by + $bh + 1)), (Color '#f2463c'), (Color '#bf141b')
    $g.FillPath($red, $band)
    $shine = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF 0, ($by - 1)), (New-Object System.Drawing.PointF 0, ($by + $bh * 0.55)), (Color '#ffffff' 56), (Color '#ffffff' 0)
    $state = $g.Save()
    $g.SetClip((New-Object System.Drawing.RectangleF $bx, $by, $bw, ($bh * 0.55)), 'Intersect')
    $g.FillPath($shine, $band)
    $g.Restore($state)

    # Letters are drawn as outlines so they scale exactly and can be centred by their real bounds.
    $letters = New-Object System.Drawing.Drawing2D.GraphicsPath
    $family = New-Object System.Drawing.FontFamily 'Arial Black'
    $letters.AddString('PDF', $family, 0, $fs, (New-Object System.Drawing.PointF 0, 0), [System.Drawing.StringFormat]::GenericTypographic)
    $b = $letters.GetBounds()
    $m = New-Object System.Drawing.Drawing2D.Matrix
    $m.Translate(($bx + ($bw - $b.Width) / 2 - $b.X), ($by + ($bh - $b.Height) / 2 - $b.Y))
    $letters.Transform($m)
    $g.FillPath((New-Object System.Drawing.SolidBrush (Color '#ffffff')), $letters)
    $g.Dispose()

    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return ,$ms.ToArray()
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$pngs = @{}
foreach ($s in $sizes) { $pngs[$s] = New-IconPng $s }
[System.IO.File]::WriteAllBytes((Join-Path $dir 'icon.png'), (New-IconPng 128))

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

# With -Preview <file>, also write one picture showing every size on a light and a dark strip.
if ($args.Count -ge 2 -and $args[0] -eq '-Preview') {
    $sheetBmp = New-Object System.Drawing.Bitmap 760, 330
    $g = [System.Drawing.Graphics]::FromImage($sheetBmp)
    $g.InterpolationMode = 'NearestNeighbor'
    $g.PixelOffsetMode = 'Half'
    $rows = @((Color '#eef1f6'), (Color '#1f2126'))
    for ($r = 0; $r -lt 2; $r++) {
        $y0 = $r * 165
        $g.FillRectangle((New-Object System.Drawing.SolidBrush $rows[$r]), 0, $y0, 760, 165)
        $big = [System.Drawing.Image]::FromStream((New-Object System.IO.MemoryStream (,$pngs[256])))
        $g.DrawImage($big, 10, $y0 + 10, 145, 145)
        $x = 175
        foreach ($s in 64, 48, 32, 24, 20, 16) {
            $img = [System.Drawing.Image]::FromStream((New-Object System.IO.MemoryStream (,$pngs[$s])))
            # Actual size on top, the same pixels enlarged three times underneath.
            $g.DrawImage($img, $x, $y0 + 8, $s, $s)
            if ($s -le 32) { $g.DrawImage($img, $x, $y0 + 60, $s * 3, $s * 3) }
            $x += 98
        }
    }
    $g.Dispose()
    $sheetBmp.Save($args[1], [System.Drawing.Imaging.ImageFormat]::Png)
}
