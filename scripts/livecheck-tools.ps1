# Fetches what `livecraft --livecheck` needs besides FFmpeg and VB-CABLE:
# MediaMTX, and Softcam built from its sources (it publishes no binaries).
# Needs git and Visual Studio or its Build Tools with C++. Run from anywhere;
# prints the two environment variables the check reads, as NAME=value lines.
param([string]$Into = "$PSScriptRoot/../target/livecheck-tools")

$ErrorActionPreference = 'Stop'
# Pinned so that a new release cannot change the check under our feet.
$mediamtx = 'v1.21.3'
$softcam = 'v1.8.1'

New-Item -ItemType Directory -Force $Into | Out-Null
$Into = (Resolve-Path $Into).Path

# Versioned folders: a cached older copy is never mistaken for the pinned one.
$serverDir = Join-Path $Into "mediamtx-$mediamtx"
$server = Join-Path $serverDir 'mediamtx.exe'
if (-not (Test-Path $server)) {
    $zip = Join-Path $Into 'mediamtx.zip'
    $url = "https://github.com/bluenviron/mediamtx/releases/download/$mediamtx/mediamtx_${mediamtx}_windows_amd64.zip"
    Invoke-WebRequest $url -OutFile $zip
    Expand-Archive $zip -DestinationPath $serverDir -Force
    Remove-Item $zip
}

$sources = Join-Path $Into "softcam-$softcam"
$dll = Join-Path $sources 'dist\bin\x64\softcam.dll'
if (-not (Test-Path $dll)) {
    if (-not (Test-Path $sources)) {
        git -c advice.detachedHead=false clone --quiet --depth 1 --branch $softcam https://github.com/tshino/softcam.git $sources
        if ($LASTEXITCODE) { throw 'Softcam could not be downloaded' }
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

"LIVECRAFT_MEDIAMTX=$server"
"LIVECRAFT_SOFTCAM=$dll"
