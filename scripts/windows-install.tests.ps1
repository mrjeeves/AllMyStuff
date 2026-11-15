# Exercise the real installer parameter defaults without running installation.
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    (Join-Path $PSScriptRoot 'install.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw $parseErrors[0] }
$resolvePrefix = [scriptblock]::Create($ast.ParamBlock.Extent.Text + [Environment]::NewLine + 'return $Prefix')
$config = Get-Content -LiteralPath (Join-Path $PSScriptRoot '..\gui\src-tauri\tauri.conf.json') -Raw | ConvertFrom-Json
function Assert($condition, $message) { if (-not $condition) { throw $message } }

Assert ($config.bundle.windows.nsis.installMode -eq 'currentUser') 'NSIS must use its per-user initial directory.'
$nativeDefault = Join-Path $env:LOCALAPPDATA $config.productName
Assert ((& $resolvePrefix) -eq $nativeDefault) 'Script and native installer have different initial directories.'

# Only the parameter block executes; no binaries, PATH, or registry are changed.
$custom = Join-Path $env:TEMP 'AMS separate installation'
Assert ((& $resolvePrefix -Prefix $custom) -eq $custom) 'Explicit custom installation directory was ignored.'
$previousDefault = Join-Path $env:LOCALAPPDATA 'Programs\AllMyStuff'
Assert ((& $resolvePrefix -Prefix $previousDefault) -eq $previousDefault) 'The previous location must remain available as an explicit choice.'

Write-Host 'Windows installation path tests passed (shared initial default and explicit separate locations).'
