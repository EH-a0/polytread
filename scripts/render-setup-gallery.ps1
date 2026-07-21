param(
    [Parameter(Mandatory = $true)]
    [string]$OutputDir
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$resolvedOutput = [System.IO.Path]::GetFullPath($OutputDir)
[System.IO.Directory]::CreateDirectory($resolvedOutput) | Out-Null

$previousGalleryDir = $env:POLYTREAD_SETUP_GALLERY_DIR
try {
    $env:POLYTREAD_SETUP_GALLERY_DIR = $resolvedOutput
    Push-Location $repoRoot
    try {
        cargo test --locked setup_ui::gallery::export_complete_setup_state_gallery -- --ignored --exact
        if ($LASTEXITCODE -ne 0) {
            throw "The Ratatui setup-state exporter failed."
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    $env:POLYTREAD_SETUP_GALLERY_DIR = $previousGalleryDir
}

$jsonPath = Join-Path $resolvedOutput "setup-gallery.json"
$gallery = Get-Content -LiteralPath $jsonPath -Raw | ConvertFrom-Json

Add-Type -AssemblyName System.Drawing

$fontCandidates = @("Cascadia Mono", "Cascadia Code", "Consolas")
$installedFonts = [System.Drawing.FontFamily]::Families.Name
$fontName = $fontCandidates |
    Where-Object { $installedFonts -contains $_ } |
    Select-Object -First 1
if (-not $fontName) {
    throw "A supported terminal font (Cascadia Mono, Cascadia Code, or Consolas) is required."
}

$fontSize = 16.0
$regularFont = [System.Drawing.Font]::new(
    $fontName,
    $fontSize,
    [System.Drawing.FontStyle]::Regular,
    [System.Drawing.GraphicsUnit]::Pixel
)
$boldFont = [System.Drawing.Font]::new(
    $fontName,
    $fontSize,
    [System.Drawing.FontStyle]::Bold,
    [System.Drawing.GraphicsUnit]::Pixel
)
$titleFont = [System.Drawing.Font]::new(
    "Segoe UI",
    27.0,
    [System.Drawing.FontStyle]::Bold,
    [System.Drawing.GraphicsUnit]::Pixel
)
$subtitleFont = [System.Drawing.Font]::new(
    "Segoe UI",
    15.0,
    [System.Drawing.FontStyle]::Regular,
    [System.Drawing.GraphicsUnit]::Pixel
)
$cardTitleFont = [System.Drawing.Font]::new(
    "Segoe UI",
    18.0,
    [System.Drawing.FontStyle]::Bold,
    [System.Drawing.GraphicsUnit]::Pixel
)
$cardBodyFont = [System.Drawing.Font]::new(
    "Segoe UI",
    13.0,
    [System.Drawing.FontStyle]::Regular,
    [System.Drawing.GraphicsUnit]::Pixel
)

$textFormat = [System.Drawing.StringFormat]::GenericTypographic.Clone()
$textFormat.FormatFlags = $textFormat.FormatFlags -bor
    [System.Drawing.StringFormatFlags]::MeasureTrailingSpaces -bor
    [System.Drawing.StringFormatFlags]::NoWrap -bor
    [System.Drawing.StringFormatFlags]::NoClip

$measureBitmap = [System.Drawing.Bitmap]::new(16, 16)
$measureGraphics = [System.Drawing.Graphics]::FromImage($measureBitmap)
$measureGraphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
$measureText = "M" * 100
$cellWidth = $measureGraphics.MeasureString(
    $measureText,
    $regularFont,
    [System.Drawing.PointF]::Empty,
    $textFormat
).Width / 100.0
$cellHeight = 20.0
$measureGraphics.Dispose()
$measureBitmap.Dispose()

$script:brushes = @{}
function Get-ColorBrush {
    param([string]$Hex)

    if (-not $script:brushes.ContainsKey($Hex)) {
        $red = [Convert]::ToInt32($Hex.Substring(1, 2), 16)
        $green = [Convert]::ToInt32($Hex.Substring(3, 2), 16)
        $blue = [Convert]::ToInt32($Hex.Substring(5, 2), 16)
        $color = [System.Drawing.Color]::FromArgb(255, $red, $green, $blue)
        $script:brushes[$Hex] = [System.Drawing.SolidBrush]::new($color)
    }
    return $script:brushes[$Hex]
}

function Get-HtmlEncoded {
    param([AllowEmptyString()][string]$Value)
    return [System.Net.WebUtility]::HtmlEncode($Value)
}

function New-TerminalBitmap {
    param($Shot)

    $width = [int]$Shot.width
    $height = [int]$Shot.height
    $pixelWidth = [int][Math]::Ceiling($width * $cellWidth)
    $pixelHeight = [int][Math]::Ceiling($height * $cellHeight)
    $bitmap = [System.Drawing.Bitmap]::new(
        $pixelWidth,
        $pixelHeight,
        [System.Drawing.Imaging.PixelFormat]::Format24bppRgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.Clear([System.Drawing.Color]::Black)
    $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::None
    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half

    for ($row = 0; $row -lt $height; $row++) {
        $column = 0
        while ($column -lt $width) {
            $start = $column
            $background = [string]$Shot.cells[$row * $width + $column].background
            while (
                $column -lt $width -and
                [string]$Shot.cells[$row * $width + $column].background -eq $background
            ) {
                $column++
            }
            $graphics.FillRectangle(
                (Get-ColorBrush $background),
                [single]($start * $cellWidth),
                [single]($row * $cellHeight),
                [single](($column - $start) * $cellWidth + 0.5),
                [single]($cellHeight + 0.5)
            )
        }

        $column = 0
        while ($column -lt $width) {
            $start = $column
            $first = $Shot.cells[$row * $width + $column]
            $foreground = [string]$first.foreground
            $bold = [bool]$first.bold
            $builder = [System.Text.StringBuilder]::new()
            while ($column -lt $width) {
                $cell = $Shot.cells[$row * $width + $column]
                if ([string]$cell.foreground -ne $foreground -or [bool]$cell.bold -ne $bold) {
                    break
                }
                [void]$builder.Append([string]$cell.symbol)
                $column++
            }
            $text = $builder.ToString()
            if ($text.Trim().Length -gt 0) {
                $font = if ($bold) { $boldFont } else { $regularFont }
                $graphics.DrawString(
                    $text,
                    $font,
                    (Get-ColorBrush $foreground),
                    [single]($start * $cellWidth),
                    [single]($row * $cellHeight + 1.0),
                    $textFormat
                )
            }
        }
    }

    $graphics.Dispose()
    return $bitmap
}

$statesDir = Join-Path $resolvedOutput "states"
[System.IO.Directory]::CreateDirectory($statesDir) | Out-Null
$stateFiles = @{}
foreach ($shot in $gallery.shots) {
    $fileName = "{0:D2}-{1}.png" -f [int]$shot.number, [string]$shot.slug
    $path = Join-Path $statesDir $fileName
    $bitmap = New-TerminalBitmap $shot
    try {
        $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $bitmap.Dispose()
    }
    $stateFiles[[int]$shot.number] = $fileName
}

function New-GallerySheet {
    param(
        [object[]]$Shots,
        [string]$Path,
        [string]$Title,
        [string]$Subtitle
    )

    $columns = 2
    $pagePadding = 36
    $columnGap = 28
    $rowGap = 28
    $headerHeight = 112
    $terminalMaxWidth = [int][Math]::Ceiling(100 * $cellWidth)
    $terminalMaxHeight = [int][Math]::Ceiling(32 * $cellHeight)
    $cardPadding = 22
    $cardHeaderHeight = 72
    $cardWidth = $terminalMaxWidth + 2 * $cardPadding
    $cardHeight = $cardHeaderHeight + $terminalMaxHeight + $cardPadding
    $rows = [int][Math]::Ceiling($Shots.Count / [double]$columns)
    $pageWidth = 2 * $pagePadding + $columns * $cardWidth + ($columns - 1) * $columnGap
    $pageHeight = $headerHeight + $pagePadding + $rows * $cardHeight + [Math]::Max(0, $rows - 1) * $rowGap

    $bitmap = [System.Drawing.Bitmap]::new(
        $pageWidth,
        $pageHeight,
        [System.Drawing.Imaging.PixelFormat]::Format24bppRgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.Clear([System.Drawing.Color]::FromArgb(5, 5, 5))
    $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias

    $graphics.DrawString($Title, $titleFont, (Get-ColorBrush "#ff9900"), 36.0, 26.0)
    $graphics.DrawString($Subtitle, $subtitleFont, (Get-ColorBrush "#999999"), 38.0, 68.0)

    for ($index = 0; $index -lt $Shots.Count; $index++) {
        $shot = $Shots[$index]
        $column = $index % $columns
        $row = [Math]::Floor($index / $columns)
        $cardX = $pagePadding + $column * ($cardWidth + $columnGap)
        $cardY = $headerHeight + $row * ($cardHeight + $rowGap)
        $cardRect = [System.Drawing.RectangleF]::new($cardX, $cardY, $cardWidth, $cardHeight)
        $graphics.FillRectangle((Get-ColorBrush "#111111"), $cardRect)
        $graphics.DrawRectangle(
            [System.Drawing.Pen]::new([System.Drawing.Color]::FromArgb(36, 36, 36), 1.0),
            $cardX,
            $cardY,
            $cardWidth - 1,
            $cardHeight - 1
        )

        $label = "[{0:D2}] {1}" -f [int]$shot.number, [string]$shot.title
        $graphics.DrawString(
            $label,
            $cardTitleFont,
            (Get-ColorBrush "#e8e8e8"),
            [single]($cardX + $cardPadding),
            [single]($cardY + 14)
        )
        $descriptionRect = [System.Drawing.RectangleF]::new(
            $cardX + $cardPadding,
            $cardY + 42,
            $cardWidth - 2 * $cardPadding,
            27
        )
        $graphics.DrawString(
            [string]$shot.description,
            $cardBodyFont,
            (Get-ColorBrush "#999999"),
            $descriptionRect
        )

        $statePath = Join-Path $statesDir $stateFiles[[int]$shot.number]
        $terminal = [System.Drawing.Image]::FromFile($statePath)
        try {
            $terminalX = $cardX + $cardPadding + ($terminalMaxWidth - $terminal.Width) / 2.0
            $terminalY = $cardY + $cardHeaderHeight + ($terminalMaxHeight - $terminal.Height) / 2.0
            $graphics.DrawImage($terminal, [single]$terminalX, [single]$terminalY)
        }
        finally {
            $terminal.Dispose()
        }
    }

    try {
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

$allShots = @($gallery.shots)
$overviewPath = Join-Path $resolvedOutput "polytread-setup-all-states.png"
$overviewArguments = @{
    Shots = $allShots
    Path = $overviewPath
    Title = "PolyTread first-time setup — complete display-state gallery"
    Subtitle = "26 deterministic states rendered from the real Ratatui implementation • orange / true-black theme"
}
New-GallerySheet @overviewArguments

$categoryOrder = @(
    "Entry and credential input",
    "Connectivity and DNS",
    "Wallet and authentication",
    "Outcomes and constraints"
)
$categorySlugs = @(
    "01-entry-and-credentials",
    "02-connectivity-and-dns",
    "03-wallet-and-authentication",
    "04-outcomes-and-constraints"
)
for ($categoryIndex = 0; $categoryIndex -lt $categoryOrder.Count; $categoryIndex++) {
    $category = $categoryOrder[$categoryIndex]
    $categoryShots = @($gallery.shots | Where-Object { $_.category -eq $category })
    $categoryPath = Join-Path $resolvedOutput ($categorySlugs[$categoryIndex] + ".png")
    $categoryArguments = @{
        Shots = $categoryShots
        Path = $categoryPath
        Title = $category
        Subtitle = "PolyTread setup design-review sheet • state numbers match the complete gallery"
    }
    New-GallerySheet @categoryArguments
}

$html = [System.Text.StringBuilder]::new()
[void]$html.AppendLine("<!doctype html>")
[void]$html.AppendLine('<html lang="en"><head><meta charset="utf-8">')
[void]$html.AppendLine('<meta name="viewport" content="width=device-width,initial-scale=1">')
[void]$html.AppendLine('<title>PolyTread setup display-state gallery</title>')
[void]$html.AppendLine(@'
<style>
:root{color-scheme:dark;--bg:#050505;--card:#111;--raised:#151515;--border:#242424;--text:#e8e8e8;--muted:#999;--orange:#ff9900}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font-family:Inter,"Segoe UI",sans-serif}
header{max-width:1540px;margin:0 auto;padding:48px 28px 28px}h1{margin:0 0 10px;color:var(--orange);font-size:34px;letter-spacing:-.02em}
header p{max-width:900px;margin:0;color:var(--muted);line-height:1.55}.legend{margin-top:18px;padding:14px 16px;background:var(--raised);border:1px solid var(--border);border-radius:8px}
main{max-width:1540px;margin:0 auto;padding:0 28px 60px}section{margin-top:28px}h2{margin:0 0 14px;font-size:22px}.grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:22px}
article{background:var(--card);border:1px solid var(--border);border-radius:10px;overflow:hidden}article h3{margin:0;padding:17px 18px 5px;font-size:17px}article p{min-height:42px;margin:0;padding:0 18px 13px;color:var(--muted);font-size:13px;line-height:1.45}
article img{display:block;width:100%;height:auto;background:#000;border-top:1px solid var(--border);image-rendering:auto}
@media(max-width:980px){.grid{grid-template-columns:1fr}header{padding-top:30px}h1{font-size:28px}}
</style></head><body>
'@)
[void]$html.AppendLine('<header><h1>PolyTread first-time setup — complete display-state gallery</h1>')
[void]$html.AppendLine('<p>Every frame below comes from the production Ratatui renderer. Values are disposable examples; no real private key is present. Animation screens use one stable frame.</p>')
[void]$html.AppendLine('<div class="legend">Use the bracketed state number when requesting a change—for example: “Change [10] so the YES acknowledgement feels safer.”</div></header><main>')
foreach ($category in $categoryOrder) {
    [void]$html.AppendLine("<section><h2>$(Get-HtmlEncoded $category)</h2><div class=`"grid`">")
    foreach ($shot in @($gallery.shots | Where-Object { $_.category -eq $category })) {
        $number = [int]$shot.number
        $file = "states/" + $stateFiles[$number]
        $label = "[{0:D2}] {1}" -f $number, [string]$shot.title
        [void]$html.AppendLine('<article>')
        [void]$html.AppendLine("<h3>$(Get-HtmlEncoded $label)</h3>")
        [void]$html.AppendLine("<p>$(Get-HtmlEncoded ([string]$shot.description))</p>")
        [void]$html.AppendLine("<img src=`"$(Get-HtmlEncoded $file)`" alt=`"$(Get-HtmlEncoded $label)`">")
        [void]$html.AppendLine('</article>')
    }
    [void]$html.AppendLine('</div></section>')
}
[void]$html.AppendLine('</main></body></html>')
[System.IO.File]::WriteAllText(
    (Join-Path $resolvedOutput "index.html"),
    $html.ToString(),
    [System.Text.UTF8Encoding]::new($false)
)

foreach ($brush in $script:brushes.Values) {
    $brush.Dispose()
}
$regularFont.Dispose()
$boldFont.Dispose()
$titleFont.Dispose()
$subtitleFont.Dispose()
$cardTitleFont.Dispose()
$cardBodyFont.Dispose()
$textFormat.Dispose()

Write-Output "Setup gallery written to $resolvedOutput"
