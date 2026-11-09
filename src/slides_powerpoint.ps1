# Run by Windows PowerShell: one picture per visible slide, reported line by
# line on stdout ("total N", "slide K", "error MESSAGE").
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$app = $null
$deck = $null
try {
    $app = New-Object -ComObject PowerPoint.Application
    # No dialog may wait for an answer nobody can give: a broken file fails.
    $app.DisplayAlerts = 1
    # Read only, untitled, no window: nothing appears on the screens of the
    # desk. A password nobody types makes a protected file fail instead of
    # asking for its password on a hidden window.
    $deck = $app.Presentations.Open($env:LC_SLIDES_IN + '::livecraft::', -1, 0, 0)
    $scale = [Math]::Min([double]$env:LC_SLIDES_W / $deck.PageSetup.SlideWidth,
        [double]$env:LC_SLIDES_H / $deck.PageSetup.SlideHeight)
    $w = [int][Math]::Round($deck.PageSetup.SlideWidth * $scale)
    $h = [int][Math]::Round($deck.PageSetup.SlideHeight * $scale)
    # The audience never sees hidden slides, so neither does the show.
    $shown = @($deck.Slides | Where-Object { $_.SlideShowTransition.Hidden -eq 0 })
    "total $($shown.Count)"
    $n = 0
    foreach ($slide in $shown) {
        if (Test-Path -LiteralPath (Join-Path $env:LC_SLIDES_OUT 'stop')) { break }
        $n++
        $slide.Export((Join-Path $env:LC_SLIDES_OUT "$n.png"), 'PNG', $w, $h)
        "slide $n"
    }
} catch {
    "error $($_.Exception.Message)"
    exit 1
} finally {
    if ($deck) { $deck.Close() }
    # PowerPoint may already have been open with the speaker's own files.
    if ($app -and $app.Presentations.Count -eq 0) { $app.Quit() }
}
