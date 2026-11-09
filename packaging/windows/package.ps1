<#
.SYNOPSIS
Builds the Windows release of LiveCraft: the installer, the portable zip and
SHA256SUMS.txt, in target/dist. See docs/releasing.md.

.DESCRIPTION
Signs livecraft.exe, the installer and its uninstaller when -CertThumbprint (or
LIVECRAFT_SIGN_THUMBPRINT) names a code signing certificate in the
CurrentUser\My store. Without one, every file name says "unsigned", so an
unsigned build cannot pass for a signed one.
#>
[CmdletBinding()]
param(
    # Must match Cargo.toml; defaults to it.
    [string]$Version,
    [string]$CertThumbprint = $env:LIVECRAFT_SIGN_THUMBPRINT,
    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [string]$Iscc = $env:ISCC
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
Set-Location $root

function Invoke-Checked([string]$what, [scriptblock]$run) {
    & $run
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)" }
}

function Find-Iscc {
    if ($Iscc) { return $Iscc }
    $onPath = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $places = @(
        "$env:ProgramFiles\Inno Setup 7\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 7\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    foreach ($p in $places) { if (Test-Path $p) { return $p } }
    throw 'ISCC.exe (Inno Setup) not found: install it or pass -Iscc'
}

function Find-SignTool {
    $onPath = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $kits = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.Directory.Parent.Name -match '^\d+(\.\d+)+$' } |
        Sort-Object { [version]$_.Directory.Parent.Name } -Descending
    if ($kits) { return @($kits)[0].FullName }
    throw 'signtool.exe not found: install the Windows SDK'
}

$cargoVersion = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $Version) { $Version = $cargoVersion }
if ($Version -ne $cargoVersion) { throw "Version $Version differs from Cargo.toml ($cargoVersion)" }
# Windows version resources only take four numbers.
$numeric = (($Version -split '[-+]')[0] + '.0')

# Thumbprints copied from the certificate window carry spaces and invisible
# marks, and ones read back from a command a line ending; signtool takes none.
$CertThumbprint = $CertThumbprint -replace '[^0-9A-Fa-f]', ''
$signed = [bool]$CertThumbprint
$name = "LiveCraft-$Version-windows-x64"
if (-not $signed) { $name += '-unsigned' }

$dist = Join-Path $root 'target/dist'
$stage = Join-Path $dist "stage/LiveCraft-$Version"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force $stage | Out-Null

# .cargo/config.toml builds for AVX2 processors only, which suits the machine
# you develop on but crashes older ones. A release holds two builds instead:
# livecraft.exe for any x86-64 processor, which hands over to
# livecraft-avx2.exe where the processor allows (see src/main.rs and
# docs/releasing.md). RUSTFLAGS replaces the config's flags; each level gets its
# own folder, so neither throws away the other or the usual build.
Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
$callerFlags = $env:RUSTFLAGS
$builds = [ordered]@{ 'livecraft.exe' = 'x86-64'; 'livecraft-avx2.exe' = 'x86-64-v3' }
foreach ($exe in $builds.Keys) {
    $cpu = $builds[$exe]
    $env:RUSTFLAGS = "-C target-cpu=$cpu"
    $buildDir = Join-Path $root "target/package/$cpu"
    Invoke-Checked "cargo build for $cpu" { cargo build --release --locked --target-dir $buildDir }
    Copy-Item (Join-Path $buildDir 'release/livecraft.exe') (Join-Path $stage $exe)
}
$env:RUSTFLAGS = $callerFlags
Copy-Item packaging/windows/FFmpeg.txt, LICENSE-MIT, LICENSE-APACHE, NOTICE, ATTRIBUTION.md $stage

$signArgs = @()
if ($signed) {
    $signTool = Find-SignTool
    $signArgs = @('sign', '/sha1', $CertThumbprint, '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256',
        '/d', 'LiveCraft', '/du', 'https://github.com/Wiibleyde/LiveCraft')
    $exes = $builds.Keys | ForEach-Object { Join-Path $stage $_ }
    Invoke-Checked 'signing the programs' { & $signTool @signArgs @exes }
}

$isccArgs = @('/Q', "/DAppVersion=$Version", "/DNumericVersion=$numeric", "/DStageDir=$stage",
    "/DOutputDir=$dist", "/DOutputName=$name-setup")
if ($signed) {
    # ISCC runs this for the installer and the uninstaller; $q is a quote and
    # $f the quoted file. The thumbprint is not secret: the key stays in the store.
    $command = (@('$q' + $signTool + '$q') + ($signArgs | ForEach-Object { if ($_ -match ' ') { '$q' + $_ + '$q' } else { $_ } }) + '$f') -join ' '
    $isccArgs += @('/DSign', "/Slivecraft=$command")
}
$isccExe = Find-Iscc
Invoke-Checked 'ISCC' { & $isccExe @isccArgs packaging/windows/livecraft.iss }

$zip = Join-Path $dist "$name-portable.zip"
# Not Compress-Archive: Windows PowerShell 5.1 writes it with backslashes,
# which other unzip tools do not read as folders.
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($stage, $zip, [IO.Compression.CompressionLevel]::Optimal, $true)

# sha256sum -c reads this file as it is: lowercase hash, two spaces, LF.
$sums = Get-ChildItem $dist -File | Sort-Object Name | ForEach-Object {
    '{0}  {1}' -f (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLowerInvariant(), $_.Name
}
[IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"))

Remove-Item -Recurse -Force (Join-Path $dist 'stage')
Get-ChildItem $dist -File | ForEach-Object { Write-Host "$($_.Name)  $([math]::Round($_.Length / 1MB, 1)) MB" }
if (-not $signed) { Write-Warning 'Unsigned build: Windows SmartScreen will warn whoever runs it.' }
