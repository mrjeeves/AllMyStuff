# Stage an AMS-only iteration beside the running diagnostic pair. Activation
# uses the existing independent Mesh cycle helper, never the AMST shell.
param(
    [Parameter(Mandatory=$true)][ValidatePattern('^[a-zA-Z0-9-]{1,70}$')][string]$RunId,
    [Parameter(Mandatory=$true)][ValidatePattern('^[a-f0-9]{40}$')][string]$ExpectedAmsCommit
)
$ErrorActionPreference = 'Stop'
$repo = 'C:\Users\Admin\AllMyStuff-video-test'
$root = 'C:\Users\Admin\video-diag-runs'
$run = Join-Path $root $RunId
if (Test-Path $run) { throw 'Run already exists; inspect before retrying' }
if ((& git -c "safe.directory=$repo" -C $repo rev-parse HEAD) -ne $ExpectedAmsCommit) { throw 'Unexpected AMS commit' }
if (@(& git -c "safe.directory=$repo" -C $repo status --porcelain --untracked-files=no).Count) { throw 'Tracked AMS changes exist' }
$active = Get-Content "$root\active.json" -Raw | ConvertFrom-Json
if (-not $active.directory.StartsWith("$root\",[StringComparison]::OrdinalIgnoreCase)) { throw 'Not a diagnostic slot' }
$pair = @(Get-CimInstance Win32_Process -Filter "Name='allmystuff-serve.exe' OR Name='myownmesh.exe'")
if (@($pair | Where-Object Name -eq 'allmystuff-serve.exe').Count -ne 1 -or @($pair | Where-Object Name -eq 'myownmesh.exe').Count -ne 1) { throw 'Expected one backend pair' }
$previous = @(foreach ($p in $pair) {
    if ($p.ExecutablePath -ne "$($active.directory)\$($p.Name)") { throw 'Active slot/process mismatch' }
    [ordered]@{name=$p.Name;pid=$p.ProcessId;path=$p.ExecutablePath;created=$p.CreationDate.ToUniversalTime().ToString('o')}
})
$sourceManifest = Get-Content "$($active.directory)\manifest.json" -Raw | ConvertFrom-Json
foreach ($p in $previous) {
    $record = @($sourceManifest.artifacts | Where-Object path -eq $p.path)
    if ($record.Count -ne 1 -or (Get-FileHash $p.path -Algorithm SHA256).Hash -ne $record[0].sha256) { throw 'Running artifact hash mismatch' }
}
New-Item -ItemType Directory "$run\rollback" -Force | Out-Null
function State([string]$phase,[string]$detail) {
    @{phase=$phase;detail=$detail;amsCommit=$ExpectedAmsCommit;commit=$active.commit;mode=$active.mode;utc=[DateTime]::UtcNow.ToString('o')} |
        ConvertTo-Json | Set-Content "$run\status.json" -Encoding UTF8
}
Start-Transcript "$run\build.log" | Out-Null
try {
    State 'building' 'AMS-only incremental build; existing pair untouched'
    foreach ($p in $previous) { Copy-Item $p.path "$run\rollback\$($p.name)" }
    Copy-Item "$($active.directory)\myownmesh.exe" "$run\myownmesh.exe"
    $env:CARGO_HOME = 'C:\Users\Admin\.cargo'
    $env:RUSTUP_HOME = 'C:\Users\Admin\.rustup'
    $env:PATH = "$env:CARGO_HOME\bin;$env:PATH"
    Set-Location "$repo\node"
    & cargo build --release --locked -j 2 --bin allmystuff-serve
    if ($LASTEXITCODE -ne 0) { throw 'AMS build failed; test pair left running' }
    Copy-Item "$repo\node\target\release\allmystuff-serve.exe" "$run\allmystuff-serve.exe"
    Copy-Item "$($active.directory)\cycle.ps1" "$run\cycle.ps1"
    $artifacts = @(Get-ChildItem $run -Recurse -Filter '*.exe' | ForEach-Object {
        @{path=$_.FullName;sha256=(Get-FileHash $_.FullName -Algorithm SHA256).Hash}
    })
    @{commit=$active.commit;amsCommit=$ExpectedAmsCommit;mode=$active.mode;previousMode=$active.mode;previous=$previous;artifacts=$artifacts} |
        ConvertTo-Json -Depth 6 | Set-Content "$run\manifest.json" -Encoding UTF8
    State 'ready' 'AMS staged and hashed; unchanged Mesh copied. Activate explicitly.'
} catch { State 'build-failed' $_.Exception.Message; throw }
finally { Stop-Transcript | Out-Null }
