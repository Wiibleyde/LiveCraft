# Fetches what `livecraft --livecheck` needs besides FFmpeg and VB-CABLE:
# MediaMTX, and Softcam built from its sources (it publishes no binaries).
# Needs git and Visual Studio or its Build Tools with C++. Run from anywhere;
# prints the two environment variables the check reads, as NAME=value lines (in GitHub Actions it
# appends just those two lines to $GITHUB_ENV instead).
param([string]$Into = "$PSScriptRoot/../target/livecheck-tools")

$ErrorActionPreference = 'Stop'
# Pinned so that a new release cannot change the check under our feet.
$mediamtx = 'v1.21.3'
# SHA-256 of the release zip, as published in that release's checksums.sha256.
$mediamtxSha256 = 'b2f5246b07cb72961df03b8d5e8cad43dc657bcf3f49a87bb031bdf22721af74'
$softcam = 'v1.8.1'
# The commit the tag must point to: a moved tag is refused.
$softcamCommit = 'afb424c2415b04da627fa1a7b4706575c2ba3010'

New-Item -ItemType Directory -Force $Into | Out-Null
$Into = (Resolve-Path $Into).Path

# Versioned folders: a cached older copy is never mistaken for the pinned one.
$serverDir = Join-Path $Into "mediamtx-$mediamtx"
$server = Join-Path $serverDir 'mediamtx.exe'
if (-not (Test-Path $server)) {
    $zip = Join-Path $Into 'mediamtx.zip'
    $url = "https://github.com/bluenviron/mediamtx/releases/download/$mediamtx/mediamtx_${mediamtx}_windows_amd64.zip"
    Invoke-WebRequest $url -OutFile $zip
    $got = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($got -ne $mediamtxSha256) {
        Remove-Item $zip
        throw "MediaMTX zip has SHA-256 $got, expected $mediamtxSha256"
    }
    Expand-Archive $zip -DestinationPath $serverDir -Force
    Remove-Item $zip
}

$sources = Join-Path $Into "softcam-$softcam"
$dll = Join-Path $sources 'dist\bin\x64\softcam.dll'
if (-not (Test-Path $dll)) {
    if (-not (Test-Path $sources)) {
        git -c advice.detachedHead=false clone --quiet --depth 1 --branch $softcam https://github.com/tshino/softcam.git $sources
        if ($LASTEXITCODE) { throw 'Softcam could not be downloaded' }
        $head = (git -C $sources rev-parse HEAD).Trim()
        if ($head -ne $softcamCommit) {
            Remove-Item -Recurse -Force $sources
            throw "Softcam $softcam is commit $head, expected $softcamCommit"
        }
    }
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $msbuild = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild `
        -find 'MSBuild\**\Bin\amd64\MSBuild.exe' | Select-Object -First 1
    if (-not $msbuild) { throw 'MSBuild not found: install Visual Studio or its Build Tools with C++' }
    # Only the DLL: the solution's test projects need NuGet packages.
    & $msbuild (Join-Path $sources 'softcam.sln') -t:softcam -p:Configuration=Release -p:Platform=x64 `
        -v:minimal -nologo | Write-Host
    if ($LASTEXITCODE) { throw 'Softcam did not build' }
}

$lines = "LIVECRAFT_MEDIAMTX=$server", "LIVECRAFT_SOFTCAM=$dll"
if ($env:GITHUB_ENV) {
    # In CI only these two lines go to the environment file; everything else was Write-Host.
    $lines | Out-File -Append -Encoding utf8 $env:GITHUB_ENV
} else {
    $lines
}
