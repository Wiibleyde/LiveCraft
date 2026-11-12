# Run by Windows PowerShell: one picture per visible slide, reported line by
# line on stdout ("total N", "slide K", "error MESSAGE").
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$app = $null
$deck = $null
$started = $false
try {
    # A PowerPoint the user already has open (even on its start screen) is
    # theirs: it is neither closed nor ended by us.
    $before = @(Get-Process -Name POWERPNT -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
    $app = New-Object -ComObject PowerPoint.Application
    if ($before.Count -eq 0) {
        $mine = @(Get-Process -Name POWERPNT -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
        if ($mine.Count -eq 1) {
            $started = $true
            # Lets the program end it if this script has to be given up.
            Set-Content -LiteralPath (Join-Path $env:LC_SLIDES_OUT 'powerpoint.pid') -Value $mine[0]
        }
    }
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
    # Only a PowerPoint started here is quit: the speaker's own one stays.
    if ($app -and $started) { $app.Quit() }
}
