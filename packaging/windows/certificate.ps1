<#
.SYNOPSIS
Puts the code signing certificate held in CERT_BASE64 (a base64 .pfx) and
CERT_PASSWORD into the CurrentUser\My store, or takes it out with -Remove.

.DESCRIPTION
The release workflow runs this with its signing secrets. The .pfx is read from
memory, never written to disk, and the password never appears on a command
line: signtool then picks the certificate by thumbprint, which is not secret,
and which goes to LIVECRAFT_SIGN_THUMBPRINT for the next steps.
#>
param([switch]$Remove)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.Security
$my = [Security.Cryptography.X509Certificates.X509Store]::new('My', 'CurrentUser')
$my.Open('ReadWrite')
try {
    if ($Remove) {
        if ($env:LIVECRAFT_SIGN_THUMBPRINT) {
            $thumbprint = $env:LIVECRAFT_SIGN_THUMBPRINT -replace '[^0-9A-Fa-f]', ''
            $found = $my.Certificates.Find('FindByThumbprint', $thumbprint, $false)
            # Taking the certificate out of the store leaves its key behind.
            foreach ($c in $found) {
                $key = [Security.Cryptography.X509Certificates.RSACertificateExtensions]::GetRSAPrivateKey($c)
                if ($key -is [Security.Cryptography.RSACng]) {
                    $key.Key.Delete()
                } elseif ($key -is [Security.Cryptography.RSACryptoServiceProvider]) {
                    $key.PersistKeyInCsp = $false
                    $key.Clear()
                }
            }
            $my.RemoveRange($found)
        }
        return
    }
    if (-not $env:CERT_BASE64) { throw 'CERT_BASE64 is empty' }
    $flags = [Security.Cryptography.X509Certificates.X509KeyStorageFlags]'UserKeySet, PersistKeySet'
    $all = [Security.Cryptography.X509Certificates.X509Certificate2Collection]::new()
    $all.Import([Convert]::FromBase64String($env:CERT_BASE64), $env:CERT_PASSWORD, $flags)
    $signer = @($all | Where-Object HasPrivateKey)
    if ($signer.Count -ne 1) { throw "Expected one certificate with a private key, found $($signer.Count)" }
    $my.Add($signer[0])
    # Intermediates go where signtool looks for them, so the signature carries
    # the whole chain.
    $ca = [Security.Cryptography.X509Certificates.X509Store]::new('CA', 'CurrentUser')
    $ca.Open('ReadWrite')
    $all | Where-Object { -not $_.HasPrivateKey -and $_.Subject -ne $_.Issuer } | ForEach-Object { $ca.Add($_) }
    $ca.Close()
    $line = "LIVECRAFT_SIGN_THUMBPRINT=$($signer[0].Thumbprint)"
    if ($env:GITHUB_ENV) { $line >> $env:GITHUB_ENV } else { Write-Output $line }
} finally {
    $my.Close()
}
