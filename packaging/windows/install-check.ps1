<#
.SYNOPSIS
Installs a LiveCraft installer silently for the current user into a temporary
folder, checks what it set up and that the program starts, then uninstalls it
and checks nothing is left behind.

.PARAMETER Screenshot
Also starts the window and saves a picture of it there. Needs OpenGL, which
hosted CI runners lack.
#>
param(
    [Parameter(Mandatory)][string]$Setup,
    [string]$Screenshot,
    # Go on although a LiveCraft is installed for this user; the check then
    # shares its registry keys and removes the ones it finds at the end.
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dir = Join-Path ([IO.Path]::GetTempPath()) ('livecraft-install-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$exe = Join-Path $dir 'livecraft.exe'
$link = Join-Path ([Environment]::GetFolderPath('Programs')) 'LiveCraft.lnk'
$classes = 'HKCU:\Software\Classes'
$silent = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')

function Assert([bool]$ok, [string]$what) {
    if (-not $ok) { throw "Check failed: $what" }
    Write-Host "ok: $what"
}

function Get-Default([string]$key) {
    if (-not (Test-Path $key)) { return $null }
    (Get-Item $key).GetValue('')
}

function Test-Value([string]$key, [string]$name) {
    (Test-Path $key) -and ((Get-Item $key).GetValueNames() -contains $name)
}

$installed = @(Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
    Where-Object { $_.GetValue('DisplayName') -like 'LiveCraft*' })
if ($installed -and -not $Force) {
    throw 'LiveCraft is installed for this user: this check would uninstall it. Pass -Force if that is what you want.'
}

$extensions = '.livecraft', '.vmix', '.regie'
$existedBefore = @($extensions | Where-Object { Test-Path "$classes\$_" })

$p = Start-Process -Wait -PassThru $Setup -ArgumentList ($silent + "/DIR=`"$dir`"" + "/LOG=`"$dir.log`"")
Assert ($p.ExitCode -eq 0) "installer exit code $($p.ExitCode)"
Assert (Test-Path $exe) "livecraft.exe in $dir"
Assert (Test-Path (Join-Path $dir 'FFmpeg.txt')) 'FFmpeg instructions installed'
Assert (Test-Path $link) "Start menu entry $link"
Assert ((Get-Default "$classes\.livecraft") -eq 'LiveCraft.Project') '.livecraft opens with LiveCraft'
Assert ((Get-Default "$classes\LiveCraft.Project\shell\open\command") -eq "`"$exe`" --project `"%1`"") 'open command'
Assert (Test-Value "$classes\.vmix\OpenWithProgids" 'LiveCraft.Project') '.vmix offers LiveCraft in Open with'
$entry = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
    Where-Object { $_.GetValue('DisplayName') -eq 'LiveCraft' }
Assert ($null -ne $entry) 'listed in Apps & features'

# Piping makes PowerShell wait for a windowed program and read what it prints.
$answer = & $exe --import-report (Join-Path $dir 'absent.vmix') 2>&1 | Out-String
Assert ($answer -match 'ECHEC') "the program starts on this processor and answers: $($answer.Trim())"
if ($Screenshot) {
    if (Test-Path $Screenshot) { Remove-Item $Screenshot }
    $run = Start-Process -PassThru $exe -ArgumentList '--screenshot', "`"$Screenshot`""
    $avx2 = Join-Path $dir 'livecraft-avx2.exe'
    $handedOver = $false
    while (-not $run.HasExited) {
        $handedOver = $handedOver -or [bool](Get-Process livecraft-avx2 -ErrorAction SilentlyContinue | Where-Object Path -eq $avx2)
        Start-Sleep -Milliseconds 200
    }
    Assert (Test-Path $Screenshot) "window picture saved to $Screenshot"
    $intrinsics = 'System.Runtime.Intrinsics.X86.Avx2' -as [type]
    if ($intrinsics) {
        $able = [System.Runtime.Intrinsics.X86.Avx2]::IsSupported -and [System.Runtime.Intrinsics.X86.Bmi2]::IsSupported -and
            [System.Runtime.Intrinsics.X86.Fma]::IsSupported -and [System.Runtime.Intrinsics.X86.Lzcnt]::IsSupported
        Assert ($handedOver -eq $able) "livecraft.exe handed over to livecraft-avx2.exe: $handedOver, as the processor allows: $able"
    }
}

$p = Start-Process -Wait -PassThru (Join-Path $dir 'unins000.exe') -ArgumentList $silent
Assert ($p.ExitCode -eq 0) "uninstaller exit code $($p.ExitCode)"
# The uninstaller finishes from a copy of itself, which removes the folder last.
$deadline = (Get-Date).AddSeconds(60)
while ((Test-Path $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Assert (-not (Test-Path $dir)) 'install folder removed'
Assert (-not (Test-Path $link)) 'Start menu entry removed'
Assert (-not (Test-Path $entry.PSPath)) 'no longer listed in Apps & features'
Assert (-not (Test-Path "$classes\LiveCraft.Project")) 'file type removed'
Assert ($null -eq (Get-Default "$classes\.livecraft")) '.livecraft no longer points at LiveCraft'
Assert (-not (Test-Value "$classes\.vmix\OpenWithProgids" 'LiveCraft.Project')) '.vmix Open with entry removed'
foreach ($e in $extensions | Where-Object { $existedBefore -notcontains $_ }) {
    Assert (-not (Test-Path "$classes\$e")) "no empty $e key left behind"
}
Remove-Item "$dir.log" -ErrorAction SilentlyContinue
