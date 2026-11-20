# Generates assets/icon.ico (16-256 px) and assets/icon-64.png (used for notifications).
# Usage: ./scripts/make-icon.ps1 -OutDir assets
param([string]$OutDir = "assets")
Add-Type -AssemblyName System.Drawing

function RoundRect([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $p = New-Object Drawing.Drawing2D.GraphicsPath
    $d = [Math]::Max(0.01, 2 * $r)
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function Draw([int]$s) {
    $bmp = New-Object Drawing.Bitmap $s, $s, ([Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'; $g.PixelOffsetMode = 'HighQuality'; $g.Clear([Drawing.Color]::Transparent)

    # Tile
    $tile = RoundRect 0 0 $s $s ($s * 0.22)
    $g.FillPath((New-Object Drawing.SolidBrush ([Drawing.Color]::FromArgb(255, 32, 34, 40))), $tile)

    # Battery body outline + terminal, rotated -35° for a diagonal look.
    $g.TranslateTransform($s * 0.5, $s * 0.5)
    $g.RotateTransform(-90)
    $g.TranslateTransform(-$s * 0.5, -$s * 0.5)

    $stroke = [Math]::Max(1.0, $s * 0.075)
    $bx = $s * 0.14; $by = $s * 0.31; $bw = $s * 0.62; $bh = $s * 0.38
    $pen = New-Object Drawing.Pen ([Drawing.Color]::White), $stroke
    $g.DrawPath($pen, (RoundRect $bx $by $bw $bh ($s * 0.07)))
    $tw = [Math]::Max(1.0, $s * 0.07); $th = $bh * 0.42
    $g.FillPath([Drawing.Brushes]::White, (RoundRect ($bx + $bw + $stroke * 0.6) ($by + ($bh - $th) / 2) $tw $th ($tw * 0.4)))

    # Charge level (orange gradient, ~70%)
    $inset = $stroke * 1.4
    $fw = ($bw - 2 * $inset) * 0.7
    $fillRect = New-Object Drawing.RectangleF ($bx + $inset), ($by + $inset), $fw, ($bh - 2 * $inset)
    $gradient = New-Object Drawing.Drawing2D.LinearGradientBrush(
        $fillRect,
        [Drawing.Color]::FromArgb(255, 255, 170, 60),
        [Drawing.Color]::FromArgb(255, 235, 110, 10),
        [Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillPath($gradient, (RoundRect $fillRect.X $fillRect.Y $fillRect.Width $fillRect.Height ($s * 0.03)))
    $gradient.Dispose()

    $g.ResetTransform()

    $g.Dispose()
    $ms = New-Object IO.MemoryStream
    if ($s -ge 256) {
        $bmp.Save($ms, [Drawing.Imaging.ImageFormat]::Png)
    } else {
        # Classic DIB entry: BITMAPINFOHEADER (height doubled), bottom-up BGRA, empty AND mask.
        $bw = New-Object IO.BinaryWriter $ms
        $maskRow = [int]([Math]::Ceiling($s / 32.0) * 4)
        $bw.Write([UInt32]40); $bw.Write([Int32]$s); $bw.Write([Int32]($s * 2)); $bw.Write([UInt16]1); $bw.Write([UInt16]32)
        $bw.Write([UInt32]0); $bw.Write([UInt32]($s * $s * 4 + $maskRow * $s)); $bw.Write([Int32]0); $bw.Write([Int32]0); $bw.Write([UInt32]0); $bw.Write([UInt32]0)
        for ($y = $s - 1; $y -ge 0; $y--) {
            for ($x = 0; $x -lt $s; $x++) {
                $c = $bmp.GetPixel($x, $y)
                $bw.Write([byte]$c.B); $bw.Write([byte]$c.G); $bw.Write([byte]$c.R); $bw.Write([byte]$c.A)
            }
        }
        $bw.Write((New-Object byte[] ($maskRow * $s)))
        $bw.Flush()
    }
    if ($s -eq 64) { $bmp.Save((Join-Path $OutDir 'icon-64.png'), [Drawing.Imaging.ImageFormat]::Png) }
    $bmp.Dispose()
    return ,$ms.ToArray()
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$images = foreach ($s in $sizes) { ,(Draw $s) }

# ICO container with PNG-compressed entries (supported since Windows Vista).
$out = New-Object IO.MemoryStream
$w = New-Object IO.BinaryWriter $out
$w.Write([UInt16]0); $w.Write([UInt16]1); $w.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $len = $images[$i].Length
    $dim = if ($s -ge 256) { 0 } else { $s }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([UInt16]1); $w.Write([UInt16]32); $w.Write([UInt32]$len); $w.Write([UInt32]$offset)
    $offset += $len
}
foreach ($img in $images) { $w.Write($img) }
$w.Flush()
[IO.File]::WriteAllBytes((Join-Path $OutDir 'icon.ico'), $out.ToArray())
"wrote icon.ico ($($out.Length) bytes)"
