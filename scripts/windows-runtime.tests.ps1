# Standalone regression tests; no Pester dependency and no system installation.
$ErrorActionPreference = 'Stop'
$installerScript = Join-Path $PSScriptRoot 'install.ps1'
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($installerScript, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw $parseErrors[0] }
$function = $ast.Find({ param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Ensure-VisualCppRuntime'
}, $true)
. ([scriptblock]::Create($function.Extent.Text))

function Log($message) {}
function Warn($message) {}
function Assert($condition, $message) { if (-not $condition) { throw $message } }
function Invoke-WebRequest {
    param($Uri, $OutFile, [switch]$UseBasicParsing)
    $script:downloaded = $OutFile
    $script:downloadUrl = $Uri
    [System.IO.File]::WriteAllText($OutFile, 'test fixture')
}
function Get-AuthenticodeSignature {
    param($LiteralPath)
    @{ Status = $script:signatureStatus; SignerCertificate = @{ Subject = $script:publisher } }
}
function Start-Process {
    param($FilePath, $ArgumentList, $WindowStyle, [switch]$Wait, [switch]$PassThru)
    $script:started++
    Assert ($ArgumentList -contains '/norestart') 'Runtime installer could restart Windows.'
    @{ ExitCode = $script:runtimeExitCode }
}

$arch = 'x86_64'
$DryRun = $false
$signatureStatus = 'Valid'
$publisher = 'CN=Microsoft Corporation, O=Microsoft Corporation, C=US'
foreach ($code in @(0, 1638, 3010, 1603, 1223)) {
    $runtimeExitCode = $code
    $started = 0
    $failed = $false
    try { Ensure-VisualCppRuntime } catch { $failed = $true }
    Assert ($failed -eq ($code -notin @(0, 1638, 3010))) "Incorrect handling of exit code $code."
    Assert ($started -eq 1) 'Signed installer did not run exactly once.'
    Assert (-not (Test-Path -LiteralPath (Split-Path -Parent $downloaded))) 'Temporary installer was not cleaned up.'
}

foreach ($case in @(
    @{ Status = 'NotSigned'; Publisher = $publisher },
    @{ Status = 'Valid'; Publisher = 'CN=Example, O=Example, C=US' }
)) {
    $signatureStatus = $case.Status
    $publisher = $case.Publisher
    $started = 0
    $failed = $false
    try { Ensure-VisualCppRuntime } catch { $failed = $true }
    Assert ($failed -and $started -eq 0) 'Untrusted runtime installer was executed.'
}

$signatureStatus = 'Valid'
$publisher = 'CN=Microsoft Corporation, O=Microsoft Corporation, C=US'
$runtimeExitCode = 0
$arch = 'aarch64'
Ensure-VisualCppRuntime
Assert ($downloadUrl -eq 'https://aka.ms/vc14/vc_redist.arm64.exe') 'ARM64 selected the wrong runtime.'

$DryRun = $true
$started = 0
$downloaded = $null
Ensure-VisualCppRuntime
Assert ($started -eq 0 -and $null -eq $downloaded) 'Dry run downloaded or executed the runtime installer.'
Write-Host 'Windows runtime installer tests passed (exit codes, signatures, architecture, dry run, cleanup).'
