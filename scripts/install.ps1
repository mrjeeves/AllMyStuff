# AllMyStuff end-user installer (Windows).
#
# Tries (in order):
#   1. Download a pre-built release binary from GitHub for the current platform.
#   2. Fall back to building from source via cargo.
#
# Installs the `allmystuff` CLI, the `allmystuff-gui` desktop app (the app
# is small and makes a bare `allmystuff` open it — pass -NoGui for a
# CLI-only install), and the standalone `amst` mesh terminal (`amst
# <machine>` opens a shell on any machine you own; -NoAmst to skip), then
# makes sure the `myownmesh` daemon the app's live mode runs on is in place:
#
#   * an installed daemon that's new enough (>= the version pinned in
#     .myownmesh-rev) is used as-is;
#   * an older one is asked to update itself (`myownmesh update`);
#   * none at all -> the latest MyOwnMesh release is installed next to
#     the app (same download + SHA-256 verification as the app itself).
#
# Pass -NoMesh to leave the daemon entirely alone. Mesh trouble never
# fails the install — the app always opens (demo graph) without it.
#
# Usage (PowerShell):
#   irm https://raw.githubusercontent.com/mrjeeves/AllMyStuff/main/scripts/install.ps1 | iex
#   iex "& { $(irm https://raw.githubusercontent.com/mrjeeves/AllMyStuff/main/scripts/install.ps1) } -NoGui"
#   .\scripts\install.ps1 -DryRun

[CmdletBinding()]
param(
    [switch]$DryRun,
    [switch]$FromSource,
    [switch]$NoGui,
    [switch]$NoMesh,
    [switch]$NoAmst,
    [string]$Prefix = "$env:LOCALAPPDATA\Programs\AllMyStuff",
    [string]$Repo = $(if ($env:ALLMYSTUFF_REPO) { $env:ALLMYSTUFF_REPO } else { "mrjeeves/AllMyStuff" }),
    [string]$MeshRepo = $(if ($env:MYOWNMESH_REPO) { $env:MYOWNMESH_REPO } else { "mrjeeves/MyOwnMesh" })
)

$ErrorActionPreference = "Stop"

function Log($msg)  { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "!!! $msg" -ForegroundColor Yellow }
function Err($msg)  { Write-Host "xxx $msg" -ForegroundColor Red }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { $env:PROCESSOR_ARCHITECTURE.ToLower() }
}
$asset = "allmystuff-windows-$arch.zip"
$guiAsset = "allmystuff-gui-windows-$arch.zip"
$serveAsset = "allmystuff-serve-windows-$arch.zip"
$amstAsset = "amst-windows-$arch.zip"
$meshAsset = "myownmesh-windows-$arch.zip"
# Set by Build-FromSource so amst can build from the same checkout/clone.
$script:RepoDir = $null

# Extract a release zip over $Prefix, retrying briefly. Windows can keep a file
# lock for a moment after a process exits, so a just-stopped .exe may not have
# released yet; a few retries beat a spurious "Access ... denied".
function Expand-Over([string]$zipPath) {
    for ($i = 0; $i -lt 5; $i++) {
        try {
            Expand-Archive -Path $zipPath -DestinationPath $Prefix -Force -ErrorAction Stop
            return
        } catch {
            if ($i -eq 4) { throw }
            Start-Sleep -Milliseconds 500
        }
    }
}

# Stop a running AllMyStuff before overwriting its binaries. Windows locks a
# running .exe, so extracting over allmystuff-gui.exe / allmystuff-serve.exe /
# myownmesh.exe while the app, node, or daemon is up fails with "Access ...
# denied" — exactly what an in-place reinstall/update hits. Stop the Always-On
# service first (so it can't respawn the node mid-install), then the app, node,
# and daemon. Returns $true if the Always-On service was running, so the caller
# can bring it back on the new binary afterward.
function Stop-AllMyStuff {
    $serviceWasRunning = $false
    $svc = Get-Service -Name "AllMyStuff" -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -ne 'Stopped') {
        Log "Stopping the Always-On service so its binaries can be replaced"
        $serviceWasRunning = $true
        try { Stop-Service -Name "AllMyStuff" -Force -ErrorAction Stop }
        catch { & sc.exe stop AllMyStuff *> $null }
    }
    # The desktop app, the node, and the mesh daemon (release name + the dev
    # `myownmesh-<triple>` name). Leave the short-lived `allmystuff` CLI alone —
    # it installs fine and may be the very process running this installer
    # (e.g. `allmystuff update`).
    $names = @("allmystuff-gui", "allmystuff-serve", "myownmesh", "myownmesh-*")
    $procs = @(Get-Process -Name $names -ErrorAction SilentlyContinue)
    if ($procs.Count -gt 0) {
        $which = ($procs | Select-Object -ExpandProperty ProcessName -Unique) -join ', '
        Log "Stopping running processes so their .exe files unlock: $which"
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        # Give Windows a moment to release the file handles before we overwrite.
        Start-Sleep -Milliseconds 800
    }
    return $serviceWasRunning
}

function Install-FromZip([string]$zipPath) {
    if (-not (Test-Path $Prefix)) {
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
    }
    Expand-Over $zipPath
    $exe = Join-Path $Prefix "allmystuff.exe"
    if (-not (Test-Path $exe)) {
        throw "allmystuff.exe not found in $zipPath after extraction"
    }
    Log "Installed: $exe"

    # Add prefix to user PATH if it isn't already there.
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($userPath -split ";" | Where-Object { $_ -ieq $Prefix })) {
        Log "Adding $Prefix to user PATH"
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$Prefix", "User")
        $env:Path = "$env:Path;$Prefix"
    }
}

function Install-GuiFromZip([string]$zipPath) {
    if (-not (Test-Path $Prefix)) {
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
    }
    Expand-Over $zipPath
    $exe = Join-Path $Prefix "allmystuff-gui.exe"
    if (-not (Test-Path $exe)) {
        throw "allmystuff-gui.exe not found in $zipPath after extraction"
    }
    Log "Installed: $exe"
}

function Install-ServeFromZip([string]$zipPath) {
    if (-not (Test-Path $Prefix)) {
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
    }
    Expand-Over $zipPath
    $exe = Join-Path $Prefix "allmystuff-serve.exe"
    if (-not (Test-Path $exe)) {
        throw "allmystuff-serve.exe not found in $zipPath after extraction"
    }
    Log "Installed: $exe"
}

function Install-AmstFromZip([string]$zipPath) {
    if (-not (Test-Path $Prefix)) {
        New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
    }
    Expand-Over $zipPath
    $exe = Join-Path $Prefix "amst.exe"
    if (-not (Test-Path $exe)) {
        throw "amst.exe not found in $zipPath after extraction"
    }
    Log "Installed: $exe"
}

function Try-Release {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    Log "Looking up latest release: $api"
    try {
        $release = Invoke-RestMethod -Uri $api -Headers @{ "User-Agent" = "allmystuff-installer" }
    } catch {
        Warn "GitHub releases unreachable (or no release yet): $($_.Exception.Message)"
        return $false
    }
    $match = $release.assets | Where-Object { $_.name -eq $asset } | Select-Object -First 1
    if (-not $match) {
        Warn "No release asset matched $asset."
        return $false
    }
    $url = $match.browser_download_url
    Log "Downloading $url"
    if ($DryRun) { Log "(dry-run) would download $url"; return $true }

    $tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "allmystuff-install-$([guid]::NewGuid())")
    try {
        $zip = Join-Path $tmp $asset
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        # A missing sidecar downgrades to a warning; a present-but-wrong
        # checksum must never install.
        $shaFile = "$zip.sha256"
        $haveSha = $true
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaFile -UseBasicParsing
        } catch {
            Warn "No SHA256 sidecar; skipping integrity check."
            $haveSha = $false
        }
        if ($haveSha) {
            $expected = (Get-Content $shaFile -Raw).Split()[0].Trim().ToLower()
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
            if ($expected -ne $actual) {
                Err "SHA256 mismatch for ${asset}: expected $expected, got $actual — not installing it."
                exit 1
            }
            Log "SHA256 OK"
        }
        Install-FromZip $zip
        return $true
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# Best-effort GUI install: fetch the portable `allmystuff-gui` zip and
# drop it next to the CLI. Returns $false (without aborting the
# overall install) if the asset is missing, unreachable, or the
# download fails — the CLI is the half that must succeed.
function Try-ReleaseGui {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $api -Headers @{ "User-Agent" = "allmystuff-installer" }
    } catch {
        Warn "GitHub releases unreachable; skipping GUI."
        return $false
    }
    $match = $release.assets | Where-Object { $_.name -eq $guiAsset } | Select-Object -First 1
    if (-not $match) {
        Warn "No GUI asset matched $guiAsset in the latest release."
        return $false
    }
    $url = $match.browser_download_url
    Log "Downloading $url"
    if ($DryRun) { Log "(dry-run) would download $url"; return $true }

    $tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "allmystuff-gui-install-$([guid]::NewGuid())")
    try {
        $zip = Join-Path $tmp $guiAsset
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        # Missing sidecar -> warn and continue; wrong checksum -> don't install.
        $shaFile = "$zip.sha256"
        $haveSha = $true
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaFile -UseBasicParsing
        } catch {
            Warn "No SHA256 sidecar for GUI; skipping integrity check."
            $haveSha = $false
        }
        if ($haveSha) {
            $expected = (Get-Content $shaFile -Raw).Split()[0].Trim().ToLower()
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
            if ($expected -ne $actual) {
                Warn "SHA256 mismatch for $guiAsset — not installing the GUI."
                return $false
            }
            Log "SHA256 OK"
        }
        Install-GuiFromZip $zip
        return $true
    } catch {
        Warn "GUI download/install failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# Best-effort node install: fetch the portable `allmystuff-serve` zip and
# drop it next to the CLI. This is the headless node `allmystuff serve` runs;
# without it that command prints a hint pointing here. (Windows has no
# `allmystuff service` backend yet — register `allmystuff serve` as a Task
# Scheduler task instead — but the binary itself runs fine.)
function Try-ReleaseServe {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $api -Headers @{ "User-Agent" = "allmystuff-installer" }
    } catch {
        Warn "GitHub releases unreachable; skipping the node binary."
        return $false
    }
    $match = $release.assets | Where-Object { $_.name -eq $serveAsset } | Select-Object -First 1
    if (-not $match) {
        Warn "No node asset matched $serveAsset in the latest release."
        return $false
    }
    $url = $match.browser_download_url
    Log "Downloading $url"
    if ($DryRun) { Log "(dry-run) would download $url"; return $true }

    $tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "allmystuff-serve-install-$([guid]::NewGuid())")
    try {
        $zip = Join-Path $tmp $serveAsset
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        $shaFile = "$zip.sha256"
        $haveSha = $true
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaFile -UseBasicParsing
        } catch {
            Warn "No SHA256 sidecar for the node binary; skipping integrity check."
            $haveSha = $false
        }
        if ($haveSha) {
            $expected = (Get-Content $shaFile -Raw).Split()[0].Trim().ToLower()
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
            if ($expected -ne $actual) {
                Warn "SHA256 mismatch for $serveAsset — not installing the node binary."
                return $false
            }
            Log "SHA256 OK"
        }
        Install-ServeFromZip $zip
        return $true
    } catch {
        Warn "node download/install failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# Best-effort amst (AMSTerm) install: fetch the portable `amst` zip and drop
# it next to the CLI — the standalone mesh terminal (`amst <machine>` opens a
# shell on a machine you own). Returns $false (without aborting the install)
# if the asset is missing. Only the binary lands here; the shortcuts and the
# "AMSTerm here" right-click menu are scripts\install-amst.ps1's job.
function Try-ReleaseAmst {
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $api -Headers @{ "User-Agent" = "allmystuff-installer" }
    } catch {
        Warn "GitHub releases unreachable; skipping amst."
        return $false
    }
    $match = $release.assets | Where-Object { $_.name -eq $amstAsset } | Select-Object -First 1
    if (-not $match) {
        Warn "No amst asset matched $amstAsset in the latest release."
        return $false
    }
    $url = $match.browser_download_url
    Log "Downloading $url"
    if ($DryRun) { Log "(dry-run) would download $url"; return $true }

    $tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "amst-install-$([guid]::NewGuid())")
    try {
        $zip = Join-Path $tmp $amstAsset
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        $shaFile = "$zip.sha256"
        $haveSha = $true
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaFile -UseBasicParsing
        } catch {
            Warn "No SHA256 sidecar for amst; skipping integrity check."
            $haveSha = $false
        }
        if ($haveSha) {
            $expected = (Get-Content $shaFile -Raw).Split()[0].Trim().ToLower()
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
            if ($expected -ne $actual) {
                Warn "SHA256 mismatch for $amstAsset — not installing amst."
                return $false
            }
            Log "SHA256 OK"
        }
        Install-AmstFromZip $zip
        return $true
    } catch {
        Warn "amst download/install failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# amst builds with a plain `cargo build --release --bin amst` — no media or
# Tauri toolchain — so unlike the GUI/node it can be built from source here
# too. Reuses the checkout Build-FromSource already resolved ($script:RepoDir).
# cargo output is sent to the host (Out-Host) so it doesn't pollute the bool
# this returns.
function Build-AmstFromSource {
    if (-not $script:RepoDir) { return $false }
    Log "Building amst from source…"
    Push-Location $script:RepoDir
    try {
        cargo build --release --bin amst 2>&1 | Out-Host
        if ($LASTEXITCODE -ne 0) {
            Warn "amst source build failed (cargo exit $LASTEXITCODE)."
            return $false
        }
        $built = Join-Path $script:RepoDir "target\release\amst.exe"
        if (-not (Test-Path $built)) {
            Warn "amst build did not produce $built"
            return $false
        }
        if (-not (Test-Path $Prefix)) {
            New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
        }
        Copy-Item -Force $built (Join-Path $Prefix "amst.exe")
        Log "Installed: $(Join-Path $Prefix 'amst.exe')"
        return $true
    } finally {
        Pop-Location
    }
}

# ---------------------------------------------------------------------------
# The mesh daemon. The desktop app's live mode runs on `myownmesh`
# (demo mode needs nothing), so the installer makes sure a usable daemon
# is in place — without ever failing the app install over it:
#
#   1. installed and new enough (>= the .myownmesh-rev pin) -> used as-is;
#   2. installed but older -> asked to update itself (`myownmesh update`);
#   3. missing -> the latest MyOwnMesh release is installed next to the
#      app, where the app finds it without any PATH refresh.

# The minimum daemon version this app wants: the rev pinned in
# .myownmesh-rev, read from the checkout when running from one, fetched
# from the repo otherwise. $null when the pin is unreachable or isn't a
# version tag (a sha pin can't be compared) — any installed daemon
# passes then.
function Get-MeshMinVersion {
    $rev = $null
    if ($PSScriptRoot) {
        $local = Join-Path (Split-Path $PSScriptRoot -Parent) ".myownmesh-rev"
        if (Test-Path $local) { $rev = (Get-Content $local -Raw).Trim() }
    }
    if (-not $rev) {
        try {
            $rev = (Invoke-RestMethod -Uri "https://raw.githubusercontent.com/$Repo/main/.myownmesh-rev" -Headers @{ "User-Agent" = "allmystuff-installer" }).Trim()
        } catch {
            return $null
        }
    }
    if ($rev -match '^v(\d+\.\d+(\.\d+)?)') { return [version]$Matches[1] }
    return $null
}

# `myownmesh --version` -> [version]0.2.9 ($null when it prints no version
# we can recognize). Scans the whole output (stdout + stderr) for the first
# semver-looking token rather than trusting a fixed column or the exit code:
# a build suffix ("0.2.9 (abc123)"), a version on stderr, or a quirky
# non-zero exit must not make a perfectly good daemon look unanswered — that
# false negative is what used to trip the "couldn't bring myownmesh up"
# warning below even when the daemon was already current.
function Get-MeshVersion([string]$exe) {
    try {
        $raw = (& $exe --version 2>&1 | Out-String)
        $m = [regex]::Match($raw, '\d+\.\d+(?:\.\d+)?')
        if ($m.Success) { return [version]$m.Value }
        return $null
    } catch {
        return $null
    }
}

# Fetch the daemon zip from MyOwnMesh's latest release (SHA-256 verified,
# like the app's own assets) and install it next to the app.
function Try-ReleaseMesh {
    $api = "https://api.github.com/repos/$MeshRepo/releases/latest"
    try {
        $release = Invoke-RestMethod -Uri $api -Headers @{ "User-Agent" = "allmystuff-installer" }
    } catch {
        Warn "MyOwnMesh releases unreachable: $($_.Exception.Message)"
        return $false
    }
    $match = $release.assets | Where-Object { $_.name -eq $meshAsset } | Select-Object -First 1
    if (-not $match) {
        Warn "No release asset matched $meshAsset in MyOwnMesh's latest release."
        return $false
    }
    $url = $match.browser_download_url
    Log "Downloading $url"

    $tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "myownmesh-install-$([guid]::NewGuid())")
    try {
        $zip = Join-Path $tmp $meshAsset
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        # Missing sidecar -> warn and continue; wrong checksum -> don't install.
        $shaFile = "$zip.sha256"
        $haveSha = $true
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaFile -UseBasicParsing
        } catch {
            Warn "No SHA256 sidecar for the daemon; skipping integrity check."
            $haveSha = $false
        }
        if ($haveSha) {
            $expected = (Get-Content $shaFile -Raw).Split()[0].Trim().ToLower()
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
            if ($expected -ne $actual) {
                Warn "SHA256 mismatch for $meshAsset — not installing the daemon."
                return $false
            }
            Log "SHA256 OK"
        }
        Expand-Over $zip
        $exe = Join-Path $Prefix "myownmesh.exe"
        if (-not (Test-Path $exe)) {
            Warn "myownmesh.exe not found in $meshAsset after extraction."
            return $false
        }
        Log "Installed: $exe"
        return $true
    } catch {
        Warn "Daemon download/install failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

function Ensure-Mesh {
    # Prefer a daemon sitting next to the app (where we'd install one —
    # the app checks there first too), then PATH.
    $existing = $null
    $local = Join-Path $Prefix "myownmesh.exe"
    if (Test-Path $local) {
        $existing = $local
    } else {
        $cmd = Get-Command myownmesh -ErrorAction SilentlyContinue
        if ($cmd) { $existing = $cmd.Source }
    }
    $min = Get-MeshMinVersion

    if ($existing) {
        $ver = Get-MeshVersion $existing
        if ($ver -and (-not $min -or $ver -ge $min)) {
            if ($min) { Log "Mesh: using the installed myownmesh v$ver at $existing (needs v$min+)." }
            else      { Log "Mesh: using the installed myownmesh v$ver at $existing." }
            return
        }
        if ($ver) { Log "Mesh: installed myownmesh is v$ver but this release wants v$min+." }
        else      { Log "Mesh: $existing didn't answer --version." }
        if ($DryRun) {
            Log "(dry-run) would ask it to update itself: myownmesh update"
            return
        }
        Log "Asking it to update itself (myownmesh update)…"
        try { & $existing update } catch { Warn "myownmesh update failed: $($_.Exception.Message)" }
        $ver = Get-MeshVersion $existing
        if ($ver -and (-not $min -or $ver -ge $min)) {
            Log "Mesh: myownmesh is now v$ver."
        } elseif (-not $ver) {
            # It ran `update` but prints no version we can read. That's a
            # version-probe miss, not a failed update — don't cry wolf.
            Log "Mesh: myownmesh is installed and responded to 'update', but didn't"
            Log "report a readable version. Assuming it's fine; the app will use it."
        } else {
            Warn "Mesh: couldn't bring myownmesh up to v$min (see above). The app still runs —"
            Warn "an older daemon just lacks the newer mesh features. Retry later with: myownmesh update"
        }
        return
    }

    if ($DryRun) {
        Log "(dry-run) would install the myownmesh daemon ($meshAsset) next to the app"
        return
    }
    Log "Mesh: no myownmesh daemon found — installing it next to the app…"
    if (Try-ReleaseMesh) {
        $ver = Get-MeshVersion (Join-Path $Prefix "myownmesh.exe")
        if ($ver) { Log "Mesh: installed myownmesh v$ver — the app starts it automatically." }
        else      { Log "Mesh: installed myownmesh — the app starts it automatically." }
    } else {
        Warn "Mesh: couldn't fetch the daemon. The app still opens (demo graph); for"
        Warn "live machines, re-run this installer later or use MyOwnMesh's:"
        Warn ('  iex "& { $(irm https://raw.githubusercontent.com/' + $MeshRepo + '/main/scripts/install.ps1) } -NoGui"')
    }
}

function Build-FromSource {
    Log "Building from source…"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Err "cargo not found. Install Rust via https://rustup.rs first."
        exit 1
    }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Err "git is required to build from source."
        exit 1
    }
    if ((Test-Path "Cargo.toml") -and (Test-Path "crates\allmystuff-cli")) {
        $repoDir = (Get-Location).Path
        Log "Using current directory as source: $repoDir"
    } else {
        $repoDir = Join-Path $env:TEMP "AllMyStuff-$([guid]::NewGuid())"
        Log "Cloning into $repoDir"
        if (-not $DryRun) { git clone --depth 1 "https://github.com/$Repo.git" $repoDir }
    }
    # Expose the resolved source dir so amst can build from the same checkout.
    $script:RepoDir = $repoDir
    if ($DryRun) { Log "(dry-run) would build in $repoDir"; return }

    Push-Location $repoDir
    try {
        cargo build --release --bin allmystuff
        $built = Join-Path $repoDir "target\release\allmystuff.exe"
        if (-not (Test-Path $built)) {
            Err "Build did not produce $built"
            exit 1
        }
        if (-not (Test-Path $Prefix)) {
            New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
        }
        Copy-Item -Force $built (Join-Path $Prefix "allmystuff.exe")
        Log "Installed: $(Join-Path $Prefix 'allmystuff.exe')"

        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not ($userPath -split ";" | Where-Object { $_ -ieq $Prefix })) {
            [Environment]::SetEnvironmentVariable("Path", "$userPath;$Prefix", "User")
            $env:Path = "$env:Path;$Prefix"
        }
    } finally {
        Pop-Location
    }
}

# Stop a running app/node/daemon (and the Always-On service) first, so we never
# try to overwrite a locked, in-use .exe. Skipped on a dry run.
$serviceWasRunning = $false
if (-not $DryRun) {
    $serviceWasRunning = Stop-AllMyStuff
}

$installedFromRelease = $false
if ($FromSource -or -not (Try-Release)) {
    Build-FromSource
} else {
    $installedFromRelease = $true
}

# Desktop app (allmystuff-gui). On by default; -NoGui skips it. Only
# attempted on the release path — building the GUI from source needs
# the full Tauri/pnpm toolchain, out of scope for this installer.
$guiInstalled = $false
if (-not $NoGui) {
    if ($installedFromRelease) {
        if (Try-ReleaseGui) {
            $guiInstalled = $true
        } else {
            throw "Full AllMyStuff install failed: the GUI binary was not installed."
        }
    } elseif ($DryRun) {
        Log "(dry-run) would install the GUI binary ($guiAsset) next to allmystuff"
    } else {
        Warn "Built the CLI from source; skipping the GUI binary (needs the Tauri/pnpm toolchain)."
        Warn "Build it with:  cd gui; pnpm install; pnpm tauri build"
    }
}

# The headless node binary (allmystuff-serve) — what `allmystuff serve` runs.
# Installed on every release install; a from-source CLI build skips it (it
# links the media toolchain).
$serveInstalled = $false
if ($installedFromRelease) {
    if (Try-ReleaseServe) {
        $serveInstalled = $true
    } else {
        throw "Full AllMyStuff install failed: allmystuff-serve was not installed."
    }
} elseif ($DryRun) {
    Log "(dry-run) would install the node binary ($serveAsset) next to allmystuff"
} else {
    Warn "Built the CLI from source; skipping the node binary (needs the media toolchain)."
    Warn "Build it with:  cargo build --release --manifest-path node\Cargo.toml"
}

# AMSTerm (amst) — the standalone mesh terminal: a shell on any machine you
# own, from your own terminal. On by default (-NoAmst skips it). Unlike the
# GUI/node it installs in BOTH paths: the portable binary on a release
# install, or `cargo build --bin amst` from source (no media/Tauri toolchain).
# Only the binary lands here; the shortcuts and "AMSTerm here" right-click
# menu are scripts\install-amst.ps1's job.
$amstInstalled = $false
if ($NoAmst) {
    Log "Skipping AMSTerm (-NoAmst)."
} elseif ($DryRun) {
    if ($installedFromRelease) {
        Log "(dry-run) would install the amst binary ($amstAsset) next to allmystuff"
    } else {
        Log "(dry-run) would build and install amst from source (cargo build --bin amst)"
    }
} elseif ($installedFromRelease) {
    if (Try-ReleaseAmst) {
        $amstInstalled = $true
    } else {
        throw "Full AllMyStuff install failed: amst was not installed."
    }
} else {
    if (Build-AmstFromSource) {
        $amstInstalled = $true
    } else {
        Warn "Couldn't build amst from source; install it separately: scripts\install-amst.ps1 -FromSource"
    }
}

# Mesh daemon — see the block above Ensure-Mesh for the rules. Both the
# desktop app *and* the headless node (`allmystuff serve`) run on it, so it's
# installed whenever either is; a from-source build skips it.
if ($NoMesh) {
    Log "Skipping the mesh daemon (-NoMesh)."
} elseif ($guiInstalled -or $serveInstalled) {
    Ensure-Mesh
} elseif ($DryRun) {
    Ensure-Mesh
} else {
    Log "Mesh: skipped — neither the desktop app nor the node binary was"
    Log "installed (only they use the daemon; scan/capabilities don't)."
}

# If we stopped the Always-On service to replace its binaries, bring it back on
# the new build so headless always-on resumes.
if ($serviceWasRunning -and -not $DryRun) {
    Log "Restarting the Always-On service on the updated binary"
    try { Start-Service -Name "AllMyStuff" -ErrorAction Stop }
    catch { & sc.exe start AllMyStuff *> $null }
}

if (-not $NoGui) {
    Log "Done. Try: allmystuff (opens the app) | allmystuff scan | allmystuff capabilities"
    Log "The app opens into a demo graph even with no mesh. Live machines run on the"
    Log "'myownmesh' daemon (handled above), which the app starts and manages"
    Log "automatically."
    if ($NoMesh) {
        Log "You skipped it (-NoMesh) — when you want live mode:"
        Log ('  iex "& { $(irm https://raw.githubusercontent.com/' + $MeshRepo + '/main/scripts/install.ps1) } -NoGui"')
    }
} else {
    Log "Done. Try: allmystuff scan | allmystuff capabilities | allmystuff update"
    if ($serveInstalled -or $DryRun) {
        Log "Headless node: 'allmystuff serve' runs this machine on the mesh (no GUI)."
        Log "On Windows, register it as a startup task (Task Scheduler) to keep it running."
    }
}
if ($amstInstalled -or $DryRun) {
    Log "AMSTerm: 'amst' opens a shell on this machine over the mesh; 'amst <machine>' on another"
    Log "you own ('amst --list' to see them). For the launcher + 'AMSTerm here' menu: scripts\install-amst.ps1."
}
Log "Open a new terminal so the updated PATH takes effect."
