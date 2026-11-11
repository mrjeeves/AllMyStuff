# Stage Microsoft's signed runtime installer for the offline NSIS bundle.
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture = 'x64',
    [Parameter(Mandatory = $true)]
    [string]$Destination
)

$ErrorActionPreference = 'Stop'
$destinationDir = Split-Path -Parent $Destination
if ($destinationDir) { New-Item -ItemType Directory -Force -Path $destinationDir | Out-Null }
Invoke-WebRequest -Uri "https://aka.ms/vc14/vc_redist.$Architecture.exe" -OutFile $Destination -UseBasicParsing
$signature = Get-AuthenticodeSignature -LiteralPath $Destination
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(?:^|,\s*)O=Microsoft Corporation(?:,|$)') {
    Remove-Item -LiteralPath $Destination -Force
    throw 'The Visual C++ runtime installer does not have a valid Microsoft signature.'
}
Write-Host "Verified Microsoft Visual C++ runtime installer: $Destination"
