# Reject Windows release binaries that still require an unbundled MSVC runtime.
# The same binaries ship in NSIS, portable archives, and the self-updater.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string[]]$BinaryPaths
)

$ErrorActionPreference = 'Stop'
$dumpbinCommand = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
if ($dumpbinCommand) {
    $dumpbin = $dumpbinCommand.Source
} else {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $dumpbin = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -Last 1
}
if (-not $dumpbin) { throw 'dumpbin.exe is required to check Windows runtime dependencies.' }

foreach ($binary in $BinaryPaths) {
    $resolved = (Resolve-Path -LiteralPath $binary).Path
    $dependencies = & $dumpbin /nologo /dependents $resolved 2>&1
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect ${binary}: $dependencies" }
    $runtime = @($dependencies | Select-String -Pattern '(?i)\b(?:vcruntime|msvcp|concrt|vcomp)\w*\.dll\b')
    if ($runtime.Count -gt 0) {
        throw "${binary} still requires the Visual C++ Redistributable: $($runtime.Line.Trim() -join ', '). Build with target-feature=+crt-static."
    }
    Write-Host "Runtime dependencies OK: $binary"
}
