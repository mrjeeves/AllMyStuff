//! Self-update for AllMyStuff.
//!
//! Modelled on `myownmesh-updater` so `allmystuff update` behaves exactly
//! like `myownmesh update`, but self-contained — it keeps its own state
//! under `~/.allmystuff/` and never links the mesh engine.
//!
//! **Three** binaries move in lockstep, the same trio the installer drops
//! side by side, each shipped as `<stem>-<platform>.{tar.gz,zip}` with a
//! mandatory `.sha256` sidecar (and, once release signing is configured, a
//! `.minisig` detached signature):
//!
//!   * `allmystuff`        — the CLI launcher (the running binary here);
//!   * `allmystuff-gui`     — the desktop app a bare `allmystuff` opens;
//!   * `allmystuff-serve`   — the node engine that carries the whole
//!     mesh/media stack. This is the half that actually runs a machine on
//!     the mesh *and the one that advertises this node's version to peers*
//!     (`node`'s `CARGO_PKG_VERSION`), so leaving it behind is what makes an
//!     "update" look like it did nothing.
//!   * `amst`              — the standalone mesh terminal (AMSTerm). Its asset
//!     stem is `amst` (not `allmystuff-amst`); like the GUI/node it's a
//!     best-effort half, refreshed only when it's actually installed here.
//!
//! Those four **binaries** are the only artifacts — every **library** crate
//! (`allmystuff-service`, `allmystuff-graph`, …) is statically linked into
//! them, so swapping the binaries carries every library change with it; a new
//! lib never needs its own updater wiring. The desktop app additionally bundles
//! a copy of `allmystuff-serve` as a Tauri sidecar; the updater finds it next
//! to the app (its `installed_path` sibling lookup) and refreshes it as the
//! `allmystuff-serve` artifact like any other, so the bundled node stays
//! current too.
//!
//! The flow is **stage now, apply on next launch**:
//!
//!   1. [`check_now`] / [`update_now`] fetch the release feed, compare
//!      versions, download and **verify** the platform assets — a published
//!      SHA-256 is mandatory (a missing one fails closed; nothing unverified is
//!      ever staged), and when this build was compiled with a release public
//!      key baked in (`ALLMYSTUFF_RELEASE_PUBKEY`) a valid detached minisign
//!      signature is required too — then extract them into
//!      `~/.allmystuff/updates/<version>/`.
//!   2. [`apply_pending_if_any`] (called first thing in `main`) atomically
//!      renames the staged binaries over the installed ones.
//!
//! The CLI is the required half: if its swap fails, the staged marker is
//! **kept** and the error surfaced so the next launch retries rather than
//! reporting a phantom success. The GUI and node halves are best-effort —
//! a host without one installed just updates the others. Each half carries
//! its own downgrade guard so a stale marker can never roll a binary back,
//! and so a half that lagged a previous partial update catches up.
//!
//! Package-manager installs (Homebrew, dpkg/apt, MSI) are detected and
//! left to the OS updater.

pub mod policy;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use policy::{compare_semver, policy_allows, ApplyPolicy};

// ---------------------------------------------------------------------------
// Release feed (build-time + runtime overridable, for white-labelling).
// ---------------------------------------------------------------------------

pub fn default_release_api_stable() -> &'static str {
    option_env!("ALLMYSTUFF_RELEASE_URL_STABLE")
        .unwrap_or("https://api.github.com/repos/mrjeeves/AllMyStuff/releases/latest")
}

pub fn default_release_api_beta() -> &'static str {
    option_env!("ALLMYSTUFF_RELEASE_URL_BETA")
        .unwrap_or("https://api.github.com/repos/mrjeeves/AllMyStuff/releases")
}

const USER_AGENT: &str = concat!("allmystuff-self-update/", env!("CARGO_PKG_VERSION"));

/// The minisign public key releases are signed with, baked in at build time.
/// `None` until release signing is configured (set `ALLMYSTUFF_RELEASE_PUBKEY`
/// to the base64 public key in the release build env — see `RELEASE-SIGNING.md`).
/// When configured, the updater refuses any artifact lacking a valid signature;
/// otherwise it still requires the mandatory SHA-256, so a missing signature on
/// an unconfigured build degrades to integrity-only, never to "unverified".
///
/// Read through [`release_pubkey`], never directly: CI exports the env var
/// unconditionally (`ALLMYSTUFF_RELEASE_PUBKEY: ${{ vars.… }}`), so when the
/// repo variable is unset the var is still *present but empty* at build time and
/// `option_env!` yields `Some("")` rather than `None`. Treating that empty
/// string as "configured" is what made an unconfigured repo demand signatures it
/// never published — failing every update closed; [`release_pubkey`] normalises
/// an empty key back to `None`.
const RELEASE_PUBKEY: Option<&str> = option_env!("ALLMYSTUFF_RELEASE_PUBKEY");

/// The configured release public key, or `None` when signing isn't set up.
/// Normalises the baked-in [`RELEASE_PUBKEY`] — see [`normalize_pubkey`].
fn release_pubkey() -> Option<&'static str> {
    normalize_pubkey(RELEASE_PUBKEY)
}

/// Normalise the baked-in key into just the base64 key string, or `None` when
/// signing isn't configured.
///
/// `minisign.pub` is a *two-line* file — an `untrusted comment:` header then the
/// base64 key on line 2 — and `ALLMYSTUFF_RELEASE_PUBKEY` is only supposed to
/// hold that second line. But it's easy to paste the whole file, or to leave a
/// trailing newline / CRLF, and `PublicKey::from_base64` then rejects the lot
/// with "Invalid encoding in minisign data" (the comment text, spaces and
/// `\r\n` aren't base64). So tolerate it positionally: trim every line and drop
/// blanks, then take the **second** line when there is one (the whole-file
/// paste — the key always follows the header), otherwise the **first** (the key
/// pasted on its own). An empty / whitespace-only value (e.g. an unset CI
/// variable exported as `""`) yields `None` — "not configured", SHA-256-only.
///
/// This only *locates* the key; the base64 is still strictly validated
/// downstream by `PublicKey::from_base64`, so a genuinely malformed key still
/// fails closed.
fn normalize_pubkey(key: Option<&str>) -> Option<&str> {
    let mut lines = key?.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next()?;
    Some(lines.next().unwrap_or(first))
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("checksum mismatch for {asset}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("{0}")]
    Other(String),
}

impl Error {
    fn msg(s: impl Into<String>) -> Self {
        Error::Other(s.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Home / state layout under ~/.allmystuff/ (ALLMYSTUFF_HOME overrides).
// ---------------------------------------------------------------------------

fn home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("ALLMYSTUFF_HOME") {
        return Ok(PathBuf::from(h));
    }
    let base = dirs::home_dir().ok_or_else(|| Error::msg("no home directory"))?;
    Ok(base.join(".allmystuff"))
}

fn updates_dir() -> Result<PathBuf> {
    let d = home()?.join("updates");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn config_path() -> Result<PathBuf> {
    Ok(home()?.join("config.json"))
}

// ---------------------------------------------------------------------------
// Auto-update config (persisted under config.json's "auto_update" key).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoUpdateConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default = "default_auto_apply")]
    pub auto_apply: String,
    #[serde(default = "default_interval")]
    pub check_interval_hours: u32,
    #[serde(default)]
    pub stable_url: Option<String>,
    #[serde(default)]
    pub beta_url: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_channel() -> String {
    "stable".into()
}
fn default_auto_apply() -> String {
    // "Up to minor": apply patch + minor bumps automatically (major still
    // waits for the user). The friendliest default while AllMyStuff is moving
    // fast — fixes and features land without a manual click, breaking changes
    // don't.
    "minor".into()
}
fn default_interval() -> u32 {
    24
}

impl Default for AutoUpdateConfig {
    fn default() -> Self {
        AutoUpdateConfig {
            enabled: true,
            channel: default_channel(),
            auto_apply: default_auto_apply(),
            check_interval_hours: default_interval(),
            stable_url: None,
            beta_url: None,
        }
    }
}

fn load_auto_update() -> AutoUpdateConfig {
    let Ok(path) = config_path() else {
        return AutoUpdateConfig::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return AutoUpdateConfig::default();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return AutoUpdateConfig::default();
    };
    serde_json::from_value(doc.get("auto_update").cloned().unwrap_or_default()).unwrap_or_default()
}

fn save_auto_update(au: &AutoUpdateConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    doc["auto_update"] = serde_json::to_value(au)?;
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn resolve_release_url(au: &AutoUpdateConfig) -> String {
    let override_url = if au.channel == "beta" {
        au.beta_url.as_deref()
    } else {
        au.stable_url.as_deref()
    };
    match override_url {
        Some(u) if !u.is_empty() => u.to_string(),
        _ if au.channel == "beta" => default_release_api_beta().to_string(),
        _ => default_release_api_stable().to_string(),
    }
}

fn env_disabled() -> bool {
    matches!(
        std::env::var("ALLMYSTUFF_AUTOUPDATE").ok().as_deref(),
        Some("0")
    )
}

// ---------------------------------------------------------------------------
// Public types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    Raw,
    PackageManager,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current_version: String,
    pub install_kind: InstallKind,
    pub enabled: bool,
    pub channel: String,
    pub auto_apply: String,
    pub check_interval_hours: u32,
    pub last_check_at: Option<i64>,
    pub staged_version: Option<String>,
    pub release_url: String,
    pub release_url_overridden: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CheckOutcome {
    Disabled,
    PackageManager,
    NotDue,
    UpToDate {
        current: String,
        latest: String,
    },
    PolicyBlocked {
        current: String,
        latest: String,
        policy: String,
    },
    Staged {
        version: String,
    },
    /// A newer release exists, but this install can't swap its own binaries —
    /// a package-managed copy, or a per-machine install this process can't
    /// write to. Reported rather than swallowed so the app can still *tell*
    /// you an update is out and how to take it.
    ManualUpdateAvailable {
        current: String,
        latest: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum UpdateNowOutcome {
    PackageManager,
    UpToDate { current: String, latest: String },
    Updated { to: String, components: Vec<String> },
}

#[derive(Debug, Default, Deserialize)]
pub struct UpdatePrefs {
    pub enabled: Option<bool>,
    pub channel: Option<String>,
    pub auto_apply: Option<String>,
    pub check_interval_hours: Option<u32>,
    pub stable_url: Option<String>,
    pub beta_url: Option<String>,
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Artifacts: the binaries we ship.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    Cli,
    Gui,
    Serve,
    Amst,
}

/// Every artifact a release ships, in apply order (the required CLI first,
/// then the best-effort halves).
const ALL_ARTIFACTS: [ArtifactKind; 4] = [
    ArtifactKind::Cli,
    ArtifactKind::Gui,
    ArtifactKind::Serve,
    ArtifactKind::Amst,
];

impl ArtifactKind {
    fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Cli => "cli",
            ArtifactKind::Gui => "gui",
            ArtifactKind::Serve => "serve",
            ArtifactKind::Amst => "amst",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "cli" => Some(ArtifactKind::Cli),
            "gui" => Some(ArtifactKind::Gui),
            "serve" => Some(ArtifactKind::Serve),
            "amst" => Some(ArtifactKind::Amst),
            _ => None,
        }
    }
    /// Release-asset stem — `allmystuff` / `allmystuff-gui` / `allmystuff-serve`
    /// / `amst`. Note `amst` breaks the `allmystuff-*` pattern (it ships under
    /// its own short name), so this must stay explicit per kind.
    fn asset_stem(self) -> &'static str {
        match self {
            ArtifactKind::Cli => "allmystuff",
            ArtifactKind::Gui => "allmystuff-gui",
            ArtifactKind::Serve => "allmystuff-serve",
            ArtifactKind::Amst => "amst",
        }
    }
    fn bin_name(self) -> &'static str {
        if cfg!(windows) {
            match self {
                ArtifactKind::Cli => "allmystuff.exe",
                ArtifactKind::Gui => "allmystuff-gui.exe",
                ArtifactKind::Serve => "allmystuff-serve.exe",
                ArtifactKind::Amst => "amst.exe",
            }
        } else {
            self.asset_stem()
        }
    }
    /// Env var that pins this binary's installed location (mirrors the CLI
    /// launcher / `allmystuff serve`), or `None` for the CLI, which is always
    /// the running exe or its sibling.
    fn bin_env_override(self) -> Option<&'static str> {
        match self {
            ArtifactKind::Cli => None,
            ArtifactKind::Gui => Some("ALLMYSTUFF_GUI_BIN"),
            ArtifactKind::Serve => Some("ALLMYSTUFF_SERVE_BIN"),
            // amst has no launcher/sidecar that pins a custom path; it's found
            // as a sibling of the running binary or on PATH (installed_path).
            ArtifactKind::Amst => None,
        }
    }
    /// The CLI is the half whose failure must not be swallowed — it's the
    /// running binary and the one a bare `allmystuff` launches everything
    /// else from. The GUI/node halves are best-effort.
    fn is_required(self) -> bool {
        matches!(self, ArtifactKind::Cli)
    }
}

// ---------------------------------------------------------------------------
// Apply (runs at process start, or on demand).
// ---------------------------------------------------------------------------

/// Apply any staged update before real work starts. Idempotent; a
/// best-effort (GUI/node) failure is logged and swallowed so an update
/// problem never blocks boot, but a failed *CLI* swap leaves the marker in
/// place to retry next launch. Call this first in `main`.
pub fn apply_pending_if_any() {
    cleanup_old_replaced_binaries();
    if let Err(e) = apply_pending() {
        tracing::warn!("self-update apply skipped: {e}");
    }
}

/// Apply a staged update now, surfacing the applied version (the swap is on
/// disk; it takes effect on next start), or `None` if nothing was pending
/// (or nothing still needed applying). Errors only when the required CLI
/// half couldn't be swapped — the marker is kept so a retry can succeed.
pub fn apply_now() -> Result<Option<String>> {
    cleanup_old_replaced_binaries();
    apply_pending()
}

/// One staged artifact parsed out of `pending.json`: a kind plus the path to
/// the archive (or, legacy, the bare binary) it was downloaded to.
struct StagedArtifact {
    kind: ArtifactKind,
    archive: PathBuf,
}

fn parse_pending_artifacts(doc: &serde_json::Value) -> Vec<StagedArtifact> {
    let mut out = Vec::new();
    if let Some(arts) = doc["artifacts"].as_array() {
        for art in arts {
            let Some(kind) = art["kind"].as_str().and_then(ArtifactKind::parse) else {
                continue;
            };
            let Some(path) = art["path"].as_str() else {
                continue;
            };
            out.push(StagedArtifact {
                kind,
                archive: PathBuf::from(path),
            });
        }
    }
    out
}

fn apply_pending() -> Result<Option<String>> {
    let pending = updates_dir()?.join("pending.json");
    if !pending.exists() {
        return Ok(None);
    }
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    let target_version = doc["version"].as_str().unwrap_or("?").to_string();

    let mut artifacts = parse_pending_artifacts(&doc);
    if artifacts.is_empty() {
        // A marker that lists nothing usable is junk — clear it.
        let _ = std::fs::remove_file(&pending);
        return Ok(None);
    }
    // CLI first (the required half), then GUI / node (best-effort).
    artifacts.sort_by_key(|a| if a.kind.is_required() { 0 } else { 1 });

    let mut applied: Vec<&'static str> = Vec::new();
    for art in &artifacts {
        // Per-artifact downgrade guard: only swap a half that's actually
        // behind, so a stale marker can't roll one back and a half that
        // lagged a previous partial update still catches up.
        if !artifact_needs_apply(art.kind, &target_version) {
            continue;
        }
        match apply_one(art) {
            Ok(true) => {
                applied.push(art.kind.as_str());
                // Stamp the version we just installed for the half that has
                // no version we can read back (the GUI/node binaries), so a
                // later check knows they're current.
                record_artifact_version(art.kind, &target_version);
            }
            // Nothing installed to replace (e.g. a staged GUI on a host that
            // has no GUI) — not an error.
            Ok(false) => {}
            Err(e) => {
                if art.kind.is_required() {
                    // Keep the marker so the next launch retries rather than
                    // silently dropping the update and reporting success.
                    return Err(e);
                }
                tracing::warn!("self-update: {} apply skipped: {e}", art.kind.as_str());
            }
        }
    }

    let _ = std::fs::remove_file(&pending);
    if applied.is_empty() {
        return Ok(None);
    }
    tracing::info!(
        "self-update applied {target_version} ({})",
        applied.join("+")
    );
    Ok(Some(target_version))
}

/// Per-artifact downgrade guard. The CLI compares against its own running
/// version; the GUI/node halves against the version stamp the updater last
/// wrote for them (absent stamp ⇒ unknown ⇒ allow, so a half installed out
/// of band by the shell installer is synced on the first update).
fn artifact_needs_apply(kind: ArtifactKind, target_version: &str) -> bool {
    match kind {
        ArtifactKind::Cli => version_is_newer(target_version, Some(current_version())),
        _ => version_is_newer(target_version, installed_artifact_version(kind).as_deref()),
    }
}

/// Swap one staged artifact over its installed counterpart. `Ok(false)`
/// when there's nothing installed to replace (e.g. a staged GUI on a
/// CLI-only host).
fn apply_one(art: &StagedArtifact) -> Result<bool> {
    let Some(target) = installed_path(art.kind) else {
        return Ok(false);
    };
    let staged_dir = art
        .archive
        .parent()
        .ok_or_else(|| Error::msg("staged archive has no parent"))?;
    let binary = extract_binary(&art.archive, staged_dir, art.kind.bin_name())?;
    atomic_replace(&binary, &target)?;
    Ok(true)
}

/// Where an artifact is installed, mirroring the CLI launcher's discovery
/// (env override → running exe / sibling → `PATH`). Returns `None` when the
/// host doesn't have that half (a headless box with no GUI, say), or when it
/// lives in an OS bundle we shouldn't touch.
///
/// The bundle guard is load-bearing: inside a macOS `.app` an in-place Mach-O
/// swap breaks the bundle's code signature and identity, so the relaunch comes
/// back refused or running the stale cached image — the "ran the update, came
/// back on the same version" symptom. A bundled app is updated by its own
/// installer (a DMG / pkg), never by swapping a binary under it, so the updater
/// leaves bundle paths alone and reports nothing to apply — the same stance
/// MyOwnMesh's `find_installed_gui_binary` takes. Loose siblings and `PATH`
/// copies (portable / `curl | sh` installs) still swap as before.
fn installed_path(kind: ArtifactKind) -> Option<PathBuf> {
    resolve_installed_path(kind).filter(|p| !path_in_os_bundle(p))
}

/// Whether `path` lives inside an OS application bundle the updater must not
/// mutate — today a macOS `.app` (its `Contents/` subtree). Detected by an
/// `*.app` path component, so it catches the binary at
/// `Foo.app/Contents/MacOS/<bin>` and any resource beneath it.
fn path_in_os_bundle(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.ends_with(".app")))
}

/// The raw install-location discovery, before the OS-bundle guard
/// ([`installed_path`]) decides whether it's safe to swap.
fn resolve_installed_path(kind: ArtifactKind) -> Option<PathBuf> {
    let current = std::env::current_exe().ok();

    // The kind matching the running binary is that binary itself.
    if let Some(cur) = &current {
        if cur
            .file_name()
            .map(|n| n.to_string_lossy() == kind.bin_name())
            .unwrap_or(false)
        {
            return Some(cur.clone());
        }
    }
    // Explicit override (e.g. ALLMYSTUFF_SERVE_BIN).
    if let Some(var) = kind.bin_env_override() {
        if let Some(p) = std::env::var_os(var) {
            let p = PathBuf::from(p);
            if p.exists() {
                return Some(p);
            }
        }
    }
    // Sibling of the running binary — the layout the installer drops the
    // whole trio in.
    if let Some(sibling) = current
        .as_ref()
        .and_then(|c| c.parent())
        .map(|d| d.join(kind.bin_name()))
    {
        if sibling.exists() {
            return Some(sibling);
        }
    }
    // On PATH.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(kind.bin_name());
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Atomically replace `target` with `staged`. A same-dir temp + rename keeps
/// the swap atomic on the target's filesystem. Unix can rename over a running
/// executable (the live process keeps its old inode); Windows can't, so the
/// running binary is side-renamed to `<name>.old` (which Windows *does* allow
/// while it's mapped) and rolled back if the swap-in then fails.
fn atomic_replace(staged: &Path, target: &Path) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| Error::msg("target has no parent dir"))?;
    let tmp = dir.join(format!(".allmystuff-update-{}.tmp", std::process::id()));
    std::fs::copy(staged, &tmp).map_err(|e| {
        Error::msg(format!(
            "cannot copy staged binary into {}: {e}",
            dir.display()
        ))
    })?;
    set_exec_perms(&tmp);

    #[cfg(not(windows))]
    {
        std::fs::rename(&tmp, target).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }
    #[cfg(windows)]
    {
        match std::fs::rename(&tmp, target) {
            Ok(()) => Ok(()),
            Err(_) => rename_via_side_swap_windows(&tmp, target).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            }),
        }
    }
}

#[cfg(windows)]
fn rename_via_side_swap_windows(src: &Path, dst: &Path) -> Result<()> {
    let old = old_binary_path(dst);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(dst, &old).map_err(|e| {
        Error::msg(format!(
            "could not rename running binary aside to {}: {e}",
            old.display()
        ))
    })?;
    if let Err(e) = std::fs::rename(src, dst) {
        // Roll back so we never leave the install without a binary.
        let _ = std::fs::rename(&old, dst);
        return Err(Error::msg(format!(
            "swap-in failed after side-rename ({e}); restored original binary"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn old_binary_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("allmystuff"));
    name.push(".old");
    target.with_file_name(name)
}

/// Delete the `<exe>.old` litter a previous Windows side-swap left behind —
/// for every half we can locate. Cheap, idempotent, runs at startup.
fn cleanup_old_replaced_binaries() {
    #[cfg(windows)]
    {
        for kind in ALL_ARTIFACTS {
            if let Some(p) = installed_path(kind) {
                let old = old_binary_path(&p);
                if old.exists() {
                    let _ = std::fs::remove_file(&old);
                }
            }
        }
    }
}

#[cfg(unix)]
fn set_exec_perms(to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn set_exec_perms(_to: &Path) {}

/// True when `target` is strictly newer than `installed`, treating an
/// unknown (`None`) installed version as "needs update" so an out-of-band
/// install gets synced once.
fn version_is_newer(target: &str, installed: Option<&str>) -> bool {
    match installed {
        Some(v) => compare_semver(target, v) == std::cmp::Ordering::Greater,
        None => true,
    }
}

/// The version stamp file for a half that exposes no readable version of its
/// own (the GUI shell, the node engine). The CLI has no stamp — it's
/// compared against its own running `CARGO_PKG_VERSION`.
fn artifact_version_marker(kind: ArtifactKind) -> Option<PathBuf> {
    match kind {
        ArtifactKind::Cli => None,
        _ => updates_dir()
            .ok()
            .map(|d| d.join(format!("{}.version", kind.as_str()))),
    }
}

fn installed_artifact_version(kind: ArtifactKind) -> Option<String> {
    let s = std::fs::read_to_string(artifact_version_marker(kind)?).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn record_artifact_version(kind: ArtifactKind, version: &str) {
    if let Some(path) = artifact_version_marker(kind) {
        let _ = std::fs::write(path, format!("{version}\n"));
    }
}

/// Whether the half `kind` should be brought to `latest`. False when it
/// isn't installed on this host; otherwise the per-artifact downgrade guard.
fn artifact_needs_update(kind: ArtifactKind, latest: &str) -> bool {
    if installed_path(kind).is_none() {
        return false;
    }
    artifact_needs_apply(kind, latest)
}

// ---------------------------------------------------------------------------
// Check + stage.
// ---------------------------------------------------------------------------

/// Run one check. With `force`, ignore the interval cooldown. Stages a
/// permitted update; never applies (that happens on next launch).
pub async fn check_now(force: bool) -> Result<CheckOutcome> {
    let au = load_auto_update();
    // `force` is an attended check (startup, header Refresh, or Check now).
    // The preference disables the background ticker, not the user's ability
    // to discover a release. An explicit environment kill switch still wins.
    if check_is_disabled(au.enabled, force, env_disabled()) {
        return Ok(CheckOutcome::Disabled);
    }
    if !force && !is_due(au.check_interval_hours) {
        return Ok(CheckOutcome::NotDue);
    }

    // A package-managed / unwritable install can't stage or apply anything,
    // but it can still *look*. Checking anyway is what turns "self-update is
    // off here" into "0.2.50 is out, update it the way you installed it" —
    // previously this returned before the fetch, so such an install never
    // contacted the release feed at all and silently reported nothing forever.
    let managed = detect_install_kind() == InstallKind::PackageManager;

    let release = fetch_release(&au).await?;
    let latest = release_tag(&release)?;
    let current = current_version().to_string();
    // Stamped only now: a failed fetch must not burn the interval, or one
    // offline moment costs a full `check_interval_hours` before the next try.
    stamp_check_now();

    if managed {
        return Ok(
            if compare_semver(&current, &latest) == std::cmp::Ordering::Less {
                CheckOutcome::ManualUpdateAvailable { current, latest }
            } else {
                CheckOutcome::UpToDate { current, latest }
            },
        );
    }

    // Which halves are behind `latest` — the CLI gauged by the running binary's
    // version, each installed sibling (GUI, node) by its recorded stamp. Gating
    // on *this* rather than the running binary alone is what lets a lagging
    // serve/gui half be caught up even once the running half is already current.
    // The old `current >= latest => UpToDate` early return ran before this, so a
    // sibling left behind by a partial update (or a fresh install that bumped
    // the binary but not the stamp) was stranded — every check short-circuited
    // and it was never re-staged. `serve` advertises this node's version to
    // peers, so a stranded serve makes the update "look like it did nothing".
    let want = wanted_artifacts(&current, &latest);
    if want.is_empty() {
        return Ok(CheckOutcome::UpToDate { current, latest });
    }

    // The auto-apply policy gates a genuine *upgrade* of the running binary.
    // Catching a lagging sibling up to a version the running half already
    // reached is a lockstep repair, not a new upgrade — so it isn't policy
    // gated (and gating it on the running version would wrongly report
    // PolicyBlocked whenever current == latest).
    if compare_semver(&current, &latest) == std::cmp::Ordering::Less {
        let pol = ApplyPolicy::parse(&au.auto_apply).unwrap_or(ApplyPolicy::Patch);
        if !policy_allows(pol, &current, &latest) {
            return Ok(CheckOutcome::PolicyBlocked {
                current,
                latest,
                policy: au.auto_apply.clone(),
            });
        }
    }

    stage_release(&release, &latest, &want).await?;
    Ok(CheckOutcome::Staged { version: latest })
}

/// Automatic-update preference gates only unattended ticks. The environment
/// switch is an operator kill switch and therefore also gates attended checks.
fn check_is_disabled(auto_enabled: bool, force: bool, disabled_by_env: bool) -> bool {
    disabled_by_env || (!auto_enabled && !force)
}

/// Which halves a release should bring forward: the CLI when it's behind, and
/// each installed sibling (GUI, node) whose recorded version lags `latest`.
fn wanted_artifacts(current: &str, latest: &str) -> Vec<ArtifactKind> {
    ALL_ARTIFACTS
        .into_iter()
        .filter(|&kind| match kind {
            // The CLI is gauged against the running binary's own version…
            ArtifactKind::Cli => compare_semver(current, latest) == std::cmp::Ordering::Less,
            // …the siblings against their recorded version stamp, and only
            // when actually installed on this host.
            _ => artifact_needs_update(kind, latest),
        })
        .collect()
}

/// One attended check: log the outcome, then hand it to `notify` so the app
/// can surface it. Shared by the launch check and every interval tick of
/// [`tick_forever`]. `force` skips only the interval cooldown ([`is_due`]);
/// the enabled flag inside [`check_now`] still applies, so a forced launch
/// check no-ops cleanly when auto-update is off. A package-managed or
/// unwritable install still checks — it reports
/// [`CheckOutcome::ManualUpdateAvailable`] rather than staging.
async fn run_attended_check(force: bool, notify: &(dyn Fn(&CheckOutcome) + Send + Sync)) {
    match check_now(force).await {
        Ok(outcome) => {
            log_check_outcome(&outcome);
            notify(&outcome);
        }
        Err(e) => tracing::warn!("self-update check failed: {e}"),
    }
}

/// Log what a check decided — *every* outcome, not just the interesting one.
/// These used to be swallowed by a bare `Ok(_) => {}`, which meant a ticker
/// that was silently disabled (package-managed install, auto-update off) and a
/// ticker that was running fine and finding nothing looked exactly alike: no
/// output at all, at any log level. That is the single hardest part of "it
/// never checks" to diagnose, so each branch now says so.
fn log_check_outcome(outcome: &CheckOutcome) {
    match outcome {
        CheckOutcome::Staged { version } => {
            tracing::info!("self-update staged {version}; applies on next launch");
        }
        CheckOutcome::ManualUpdateAvailable { current, latest } => {
            tracing::info!(
                "self-update: {latest} is available (running {current}), but this install is \
                 package-managed or not writable by this process — update it the way you \
                 installed it"
            );
        }
        CheckOutcome::UpToDate { current, latest } => {
            tracing::debug!("self-update: up to date (running {current}, latest {latest})");
        }
        CheckOutcome::PolicyBlocked {
            current,
            latest,
            policy,
        } => {
            tracing::info!(
                "self-update: {latest} is available (running {current}) but the '{policy}' \
                 apply policy holds it back — apply it from Settings → Updates"
            );
        }
        CheckOutcome::NotDue => tracing::debug!("self-update: not due yet, skipping this tick"),
        CheckOutcome::Disabled => tracing::debug!("self-update: disabled, skipping this tick"),
        CheckOutcome::PackageManager => {
            tracing::debug!("self-update: package-managed install, skipping this tick")
        }
    }
}

/// Background auto-update ticker — the half of self-update that makes it
/// "set and forget". Runs forever: a **launch check** fires shortly after the
/// process starts and ignores the interval cooldown, so opening the app
/// catches a release even when the last check was under `check_interval_hours`
/// ago (a relaunch within the day used to no-op on the cooldown and wait the
/// full interval). It is *claimed* rather than unconditional — one launch
/// check per machine coming up, not one per process that links this crate, so
/// the GUI and the node it spawns don't fetch the same feed twice on every
/// launch (see [`claim_launch_check`]). It then starts the 24h timer: another
/// check every `check_interval_hours` (re-read each loop so a settings change
/// takes effect without a restart). Each tick is gated on
/// the enabled flag and the apply policy, so spawning it in a disabled install
/// no-ops. A package-managed or unwritable install still *checks* — it just
/// reports [`CheckOutcome::ManualUpdateAvailable`] instead of staging, so the
/// app can say a release is out even where it can't install it itself.
/// Whatever it does stage applies on the next launch (see
/// [`apply_pending_if_any`]).
///
/// Every long-lived process that links the updater spawns this: the desktop
/// app's Tauri shell and the headless `allmystuff-serve` node. Without it the
/// release feed is only ever hit by the on-demand `check_now(true)` behind the
/// UI's "Check now" / `allmystuff update check` — i.e. auto-update never fires
/// on its own. The short-lived CLI subcommands don't spawn it: they act once
/// and exit.
pub async fn tick_forever() {
    tick_forever_notify(|_| {}).await
}

/// [`tick_forever`], plus a callback fired with the outcome of every check.
///
/// The ticker is otherwise mute: it logs, and a staged update sits on disk
/// until someone thinks to open Settings → Updates, because the background
/// task holds no handle to the UI and so cannot tell it anything. The desktop
/// app passes a closure here that emits a Tauri event, which is what makes an
/// "update ready" actually reach the user — the notify half of an update
/// notification system.
pub async fn tick_forever_notify<F>(notify: F)
where
    F: Fn(&CheckOutcome) + Send + Sync + 'static,
{
    // Let a freshly launched app/node settle (bind sockets, bring the session
    // online) before the first network hit.
    tokio::time::sleep(Duration::from_secs(30)).await;
    // The launch check: force past the interval cooldown so opening the app is
    // its own check (still gated on the enabled flag). Claimed, so the GUI and
    // the node it spawned don't both fetch the same feed on the way up — see
    // [`claim_launch_check`]. Then the interval timer takes over for the rest
    // of the run.
    if claim_launch_check() {
        run_attended_check(true, &notify).await;
    }
    loop {
        let hours = load_auto_update().check_interval_hours.max(1);
        tokio::time::sleep(Duration::from_secs(hours as u64 * 3600)).await;
        run_attended_check(false, &notify).await;
    }
}

/// Background auto-update for an **unattended** long-lived process — the
/// headless `allmystuff-serve` node, including when it runs as an OS service
/// (systemd / launchd / Windows SCM). Like [`tick_forever`], but a service
/// box has no one to click "relaunch" and may not restart for months, so a
/// staged update would otherwise never take effect. Here each staged update is
/// *applied* immediately and `relaunch` is called to bring the new version up
/// — the "always on, always current" half of self-update, on all three OSes.
///
/// `relaunch` must restart the process onto the just-applied binaries and not
/// return: a re-exec on unix and on a Windows console; an exit that lets the
/// Service Control Manager restart the service under Windows. It's only ever
/// invoked after a successful apply, so it always runs the *new* binary. The
/// same lockstep rules as elsewhere mean every installed half (CLI, GUI, node)
/// that's reachable from this process is brought forward together, not just
/// the node itself.
///
/// All the gating of [`tick_forever`] still applies (enabled flag, package
/// manager, apply policy, interval), so this no-ops cleanly when auto-update
/// is off or the install is package-managed — `relaunch` never fires then.
/// Like the attended ticker, the launch check ignores the interval cooldown
/// (a node that bounces inside the interval still checks once on the way up)
/// and is claimed against the other halves starting alongside it, then the 24h
/// timer takes over. Standing down on a lost claim doesn't cost this node its
/// currency: it adopts whatever the winner staged instead (see
/// [`adopt_launch_stage`]).
pub async fn tick_forever_unattended(relaunch: fn() -> !) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    // Launch check first (cooldown ignored, but claimed so a GUI starting
    // alongside this node doesn't fetch the same feed twice), then the
    // recurring interval.
    if claim_launch_check() {
        run_unattended_check(true, relaunch).await;
    } else {
        adopt_launch_stage(relaunch).await;
    }
    loop {
        let hours = load_auto_update().check_interval_hours.max(1);
        tokio::time::sleep(Duration::from_secs(hours as u64 * 3600)).await;
        run_unattended_check(false, relaunch).await;
    }
}

/// The unattended node lost the launch claim: another half of this install
/// (the desktop GUI, almost always) is running the launch check right now.
/// Fetching the same feed a second time is exactly what the claim exists to
/// prevent — but standing down mustn't cost this node the "always current"
/// half of its job, or a service box would sit on a staged update until its
/// next interval tick, which is the one thing running unattended is for.
///
/// The winner stages into the same `pending.json` this node applies from, so
/// there's nothing to fetch: wait for its check to land, then apply whatever
/// is there. Silent when nothing was staged (the common case — the machine
/// was already current), which is why this only logs once it has something.
async fn adopt_launch_stage(relaunch: fn() -> !) {
    // Long enough to cover the winner's fetch, download and staging of every
    // artifact in the set; short enough that a service box comes up current
    // rather than a tick later. Nothing is lost if it's still mid-stage — the
    // interval tick catches it, exactly as it would have before.
    tokio::time::sleep(Duration::from_secs(120)).await;
    let Some(version) = staged_version() else {
        return;
    };
    tracing::info!("self-update {version} staged by another half of this install; applying now");
    match apply_now() {
        Ok(Some(applied)) => {
            tracing::info!("self-update applied {applied}; relaunching to run the new version");
            relaunch();
        }
        Ok(None) => tracing::debug!("self-update {version} needed nothing applied here"),
        Err(e) => tracing::warn!("self-update apply failed: {e}; retrying next tick"),
    }
}

/// One unattended check: stage, then *apply + relaunch* immediately (the
/// "always on, always current" half — a service box has no one to click
/// "relaunch"). Shared by the launch check and every interval tick of
/// [`tick_forever_unattended`]. `force` skips only the interval cooldown;
/// enabled / package-manager gating inside [`check_now`] still applies.
async fn run_unattended_check(force: bool, relaunch: fn() -> !) {
    match check_now(force).await {
        Ok(CheckOutcome::Staged { version }) => {
            tracing::info!("self-update staged {version}; applying now (unattended node)");
            match apply_now() {
                Ok(Some(applied)) => {
                    tracing::info!(
                        "self-update applied {applied}; relaunching to run the new version"
                    );
                    relaunch();
                }
                // Staged but nothing needed applying (already current on
                // disk), or a best-effort half hiccupped — keep serving the
                // running version and retry on the next tick.
                Ok(None) => tracing::warn!(
                    "self-update staged {version} but nothing was applied; retrying next tick"
                ),
                Err(e) => tracing::warn!("self-update apply failed: {e}; retrying next tick"),
            }
        }
        Ok(other) => log_check_outcome(&other),
        Err(e) => tracing::warn!("self-update check failed: {e}"),
    }
}

/// User-driven "update everything now" — the surface behind a bare
/// `allmystuff update`. Ignores policy + interval (consent implied) but
/// still defers to the OS package manager. Applies to disk immediately;
/// running processes pick it up on restart.
pub async fn update_now() -> Result<UpdateNowOutcome> {
    if detect_install_kind() == InstallKind::PackageManager {
        return Ok(UpdateNowOutcome::PackageManager);
    }
    let au = load_auto_update();
    let release = fetch_release(&au).await?;
    let latest = release_tag(&release)?;
    let current = current_version().to_string();

    // Nothing to do only when the CLI is current *and* every installed
    // sibling already matches — otherwise a previous partial update left a
    // half behind and we still have work to do.
    let want = wanted_artifacts(&current, &latest);
    if want.is_empty() {
        return Ok(UpdateNowOutcome::UpToDate { current, latest });
    }

    stamp_check_now();
    let kinds = stage_release(&release, &latest, &want).await?;
    // Apply right now rather than waiting for the next launch. A failed CLI
    // swap propagates here (the marker is kept) so we never report a phantom
    // success; a best-effort GUI/node hiccup is logged and the rest applied.
    apply_now()?;
    Ok(UpdateNowOutcome::Updated {
        to: latest,
        components: kinds.iter().map(|k| k.as_str().to_string()).collect(),
    })
}

/// The latest release version on the configured channel — read-only. It
/// fetches the channel feed and returns the release tag (e.g. `"0.2.0"`)
/// without staging, applying, or touching any local state. This is what
/// lets one machine tell that *another* (whose running version rides its
/// presence advert) is behind the channel, so it can offer to upgrade it.
/// `Ok(None)` only when the feed carries no usable tag.
pub async fn latest_version() -> Result<Option<String>> {
    let au = load_auto_update();
    let release = fetch_release(&au).await?;
    Ok(release_tag(&release).ok())
}

/// Current updater status (no network access).
pub fn status() -> Result<UpdateStatus> {
    let au = load_auto_update();
    let override_url = if au.channel == "beta" {
        au.beta_url.as_deref()
    } else {
        au.stable_url.as_deref()
    };
    Ok(UpdateStatus {
        current_version: current_version().to_string(),
        install_kind: detect_install_kind(),
        enabled: au.enabled && !env_disabled(),
        channel: au.channel.clone(),
        auto_apply: au.auto_apply.clone(),
        check_interval_hours: au.check_interval_hours,
        last_check_at: last_check_at(),
        staged_version: staged_version(),
        release_url: resolve_release_url(&au),
        release_url_overridden: override_url.map(|s| !s.is_empty()).unwrap_or(false),
    })
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    set_prefs(UpdatePrefs {
        enabled: Some(enabled),
        ..Default::default()
    })
    .map(|_| ())
}

pub fn set_prefs(prefs: UpdatePrefs) -> Result<UpdateStatus> {
    let mut au = load_auto_update();
    if let Some(v) = prefs.enabled {
        au.enabled = v;
    }
    if let Some(v) = prefs.channel {
        au.channel = v;
    }
    if let Some(v) = prefs.auto_apply {
        au.auto_apply = v;
    }
    if let Some(v) = prefs.check_interval_hours {
        au.check_interval_hours = v;
    }
    // Empty string clears an override back to the default feed.
    if let Some(v) = prefs.stable_url {
        au.stable_url = (!v.is_empty()).then_some(v);
    }
    if let Some(v) = prefs.beta_url {
        au.beta_url = (!v.is_empty()).then_some(v);
    }
    save_auto_update(&au)?;
    status()
}

// ---- stamps ----------------------------------------------------------

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn last_check_at() -> Option<i64> {
    let p = updates_dir().ok()?.join("last_check.json");
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    doc["at"].as_i64()
}

fn stamp_check_now() {
    if let Ok(dir) = updates_dir() {
        let _ = std::fs::write(
            dir.join("last_check.json"),
            serde_json::json!({ "at": now_secs() }).to_string(),
        );
    }
}

fn is_due(interval_hours: u32) -> bool {
    match last_check_at() {
        Some(at) => now_secs() - at >= (interval_hours as i64) * 3600,
        None => true,
    }
}

/// How long a launch check stays claimed. A launch check deliberately ignores
/// `check_interval_hours` — opening the app should catch a release even when
/// the last check was minutes ago — but "launch" belongs to the *machine*
/// coming up, not to each process that happens to link this crate. A desktop
/// box starts two of them together: the GUI runs [`tick_forever_notify`] and
/// the `allmystuff-serve` node it spawns runs [`tick_forever_unattended`],
/// each forcing past the interval seconds apart. That is two identical fetches
/// of the same feed on every single launch, and the budget it spends is
/// smaller than it looks — the feed call is unauthenticated, and GitHub allows
/// 60 of those per hour per IP, shared by every install behind one address.
///
/// So whoever gets there first does the launch check and anyone starting
/// alongside it takes that answer. Wide enough to cover a node that comes up
/// well after the GUI (cold Windows starts are slow) or a service that starts
/// staggered; far short of any real interval, so a genuine relaunch later in
/// the day still checks.
const LAUNCH_CLAIM_SECS: i64 = 10 * 60;

fn launch_claim_path() -> Result<PathBuf> {
    Ok(updates_dir()?.join("launch_check.json"))
}

fn launch_claim_at() -> Option<i64> {
    let p = launch_claim_path().ok()?;
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    doc["at"].as_i64()
}

/// Claim this machine's launch check. True for the one process that wins it,
/// false for the halves starting alongside it (they skip straight to their
/// interval timer).
///
/// The claim is written *before* the fetch, not after: `last_check.json` is
/// only stamped once a fetch succeeds — deliberately, so one offline moment
/// can't burn a whole interval — which makes it useless for coordinating
/// processes that start within a network round-trip of each other. That is
/// exactly the window here: the GUI spawns the node a second or two after
/// itself, so both would read a not-yet-written stamp and both fetch.
///
/// `create_new` is the atomic primitive — on all three OSes exactly one
/// process can create a file that doesn't already exist, so the race has a
/// single winner rather than a likely one. A claim older than
/// [`LAUNCH_CLAIM_SECS`] is from a previous launch (or a process that died
/// mid-check) and is cleared, or one crash would suppress every future launch
/// check on the machine. Any error coordinating leaves this process checking:
/// a duplicate fetch is the old behaviour, while a swallowed launch check is a
/// release nobody hears about.
fn claim_launch_check() -> bool {
    let Ok(path) = launch_claim_path() else {
        return true;
    };
    if let Some(at) = launch_claim_at() {
        if now_secs() - at < LAUNCH_CLAIM_SECS {
            return false;
        }
        let _ = std::fs::remove_file(&path);
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            let _ = f.write_all(
                serde_json::json!({ "at": now_secs() })
                    .to_string()
                    .as_bytes(),
            );
            true
        }
        // Someone else claimed it between our read and our create — they're
        // doing the launch check, so we stand down.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => true,
    }
}

fn staged_version() -> Option<String> {
    let p = updates_dir().ok()?.join("pending.json");
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    doc["version"].as_str().map(str::to_string)
}

// ---- install-kind detection ------------------------------------------

pub fn detect_install_kind() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Raw;
    };
    // A foreign package manager owns the binary outright — never self-update.
    if detect_install_kind_from_path(&exe.to_string_lossy()) == InstallKind::PackageManager {
        return InstallKind::PackageManager;
    }
    // Otherwise the only question left is whether we can actually perform the
    // swap. A per-machine install (`C:\Program Files\…` from the MSI, `/opt/…`)
    // is owned by Administrators/root, so an unelevated process can stage a
    // download it will never be able to apply — retrying, and failing, forever.
    // Treating that as package-managed keeps the *check* running (so a new
    // release is still reported) while routing the install itself back through
    // the installer that owns it.
    match exe.parent() {
        Some(dir) if !dir_is_writable(dir) => InstallKind::PackageManager,
        _ => InstallKind::Raw,
    }
}

/// Whether `dir` accepts writes from this process. Probing by *doing* it is
/// the only reliable test on Windows, where ACLs (and virtualisation) make a
/// permissions read meaningless — and it's what the swap itself will attempt.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".allmystuff-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Path-only classification: is this binary owned by a *foreign* package
/// manager (Homebrew, apt, Chocolatey, Scoop) that must do the updating?
///
/// Deliberately **not** a test for "somewhere under Program Files". Our own
/// Windows MSI installs to `C:\Program Files\AllMyStuff\`, and treating that
/// as package-managed silently disabled self-update — check included — for
/// every machine that took the MSI rather than the NSIS setup. Whether such an
/// install can be *written* is a separate question, answered by
/// [`dir_is_writable`] in [`detect_install_kind`].
fn detect_install_kind_from_path(path_str: &str) -> InstallKind {
    // Homebrew (macOS/Linux).
    if path_str.contains("/Cellar/")
        || path_str.starts_with("/opt/homebrew/")
        || path_str.starts_with("/home/linuxbrew/")
        || path_str.starts_with("/usr/local/Cellar/")
    {
        return InstallKind::PackageManager;
    }
    #[cfg(target_os = "linux")]
    if path_str.starts_with("/usr/bin/") || path_str.starts_with("/usr/sbin/") {
        return InstallKind::PackageManager;
    }
    {
        let lower = path_str.to_lowercase();
        if lower.contains("\\chocolatey\\lib\\") || lower.contains("\\scoop\\apps\\") {
            return InstallKind::PackageManager;
        }
    }
    InstallKind::Raw
}

// ---- network: fetch / stage ------------------------------------------

fn release_tag(release: &serde_json::Value) -> Result<String> {
    // The "latest" endpoint returns an object; the "list" endpoint an array
    // — take the first entry there.
    let obj = if release.is_array() {
        release
            .get(0)
            .ok_or_else(|| Error::msg("empty release list"))?
    } else {
        release
    };
    obj["tag_name"]
        .as_str()
        .map(|s| s.trim_start_matches('v').to_string())
        .ok_or_else(|| Error::msg("release missing tag_name"))
}

async fn fetch_release(au: &AutoUpdateConfig) -> Result<serde_json::Value> {
    let url = resolve_release_url(au);
    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

async fn stage_release(
    release: &serde_json::Value,
    version: &str,
    want: &[ArtifactKind],
) -> Result<Vec<ArtifactKind>> {
    let obj = if release.is_array() {
        release
            .get(0)
            .ok_or_else(|| Error::msg("empty release list"))?
    } else {
        release
    };
    let assets = obj["assets"]
        .as_array()
        .ok_or_else(|| Error::msg("release has no assets"))?;
    let dir = updates_dir()?.join(version);
    std::fs::create_dir_all(&dir)?;

    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
    let mut staged = Vec::new();
    let mut manifest = Vec::new();
    // Real per-half failure reasons, so a release where nothing could be staged
    // reports *why* (download/checksum/signature/io) instead of the misleading
    // "no matching platform asset" — the asset usually matched fine and the
    // actual error was lost to a best-effort warning no CLI subscriber prints.
    let mut errors: Vec<String> = Vec::new();
    for &kind in want {
        let asset_name = platform_asset(kind.asset_stem());
        let asset = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(&asset_name));

        // The CLI is the required half — its asset must be present and must
        // download. The GUI/node halves are best-effort: a missing asset
        // (older release) or a transient download error logs and continues so
        // a sibling hiccup never blocks the CLI update.
        let staged_one = async {
            let asset =
                asset.ok_or_else(|| Error::msg(format!("release has no asset {asset_name}")))?;
            let url = asset["browser_download_url"]
                .as_str()
                .ok_or_else(|| Error::msg("asset missing download url"))?;
            let dest = dir.join(&asset_name);
            download_verify_stage(&client, assets, url, &dest, &asset_name).await?;
            Ok::<PathBuf, Error>(dest)
        }
        .await;

        match staged_one {
            Ok(dest) => {
                manifest.push(serde_json::json!({
                    "kind": kind.as_str(),
                    "path": dest.to_string_lossy(),
                }));
                staged.push(kind);
            }
            Err(e) if kind.is_required() => return Err(e),
            Err(e) => {
                tracing::warn!(
                    "self-update: staging the {} half failed ({e}); skipping it",
                    kind.as_str()
                );
                errors.push(format!("{}: {e}", kind.as_str()));
            }
        }
    }
    if staged.is_empty() {
        // Surface the real reasons. A bare "no matching platform asset" only
        // ever fits the case where no half even had an asset for this platform;
        // a download/checksum/signature/io failure must say so or it's
        // undiagnosable (the per-half warnings above go nowhere in the CLI).
        return Err(Error::msg(if errors.is_empty() {
            "no matching platform asset in release".to_string()
        } else {
            format!("could not stage any update — {}", errors.join("; "))
        }));
    }

    std::fs::write(
        updates_dir()?.join("pending.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "version": version,
            "artifacts": manifest,
        }))?,
    )?;
    Ok(staged)
}

/// Find the expected SHA-256 for `asset_name`: a `<asset>.sha256` sidecar
/// asset, else a `SHA256SUMS` line. `None` if neither is published — the
/// caller treats that as fail-closed and refuses to stage (never "unverified").
async fn find_sha256(
    assets: &[serde_json::Value],
    asset_name: &str,
    client: &reqwest::Client,
) -> Option<String> {
    let sidecar = format!("{asset_name}.sha256");
    if let Some(a) = assets.iter().find(|a| a["name"].as_str() == Some(&sidecar)) {
        if let Some(url) = a["browser_download_url"].as_str() {
            if let Ok(text) = fetch_text(client, url).await {
                return text.split_whitespace().next().map(str::to_string);
            }
        }
    }
    if let Some(a) = assets
        .iter()
        .find(|a| a["name"].as_str() == Some("SHA256SUMS"))
    {
        if let Some(url) = a["browser_download_url"].as_str() {
            if let Ok(text) = fetch_text(client, url).await {
                for line in text.lines() {
                    let mut it = line.split_whitespace();
                    let (sum, name) = (it.next(), it.next());
                    if name.map(|n| n.trim_start_matches('*')) == Some(asset_name) {
                        return sum.map(str::to_string);
                    }
                }
            }
        }
    }
    None
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// Download `url`, **verify** it, and write it to `dest`. Fails closed: a
/// published SHA-256 is mandatory (a missing one refuses to stage rather than
/// the old behaviour of warning and staging unverified), and when this build
/// has a release public key baked in, a valid detached minisign signature over
/// the artifact is required too. SHA-256 proves only integrity against the same
/// release; the signature is what makes a swapped release asset detectable.
async fn download_verify_stage(
    client: &reqwest::Client,
    assets: &[serde_json::Value],
    url: &str,
    dest: &Path,
    asset_name: &str,
) -> Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    // Integrity: a published checksum is mandatory. A missing sidecar used to
    // fall through to a warning, which let anyone able to omit it serve any
    // payload — now it refuses to stage.
    let expected = find_sha256(assets, asset_name, client)
        .await
        .ok_or_else(|| {
            Error::msg(format!(
                "no checksum sidecar for {asset_name}; refusing to stage unverified"
            ))
        })?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(Error::ChecksumMismatch {
            asset: asset_name.to_string(),
            expected,
            actual,
        });
    }

    // Authenticity: when a release signing key is baked in, a valid detached
    // minisign signature over the artifact is required before staging.
    match release_pubkey() {
        Some(pubkey) => {
            let sig_name = format!("{asset_name}.minisig");
            let sig_asset = assets
                .iter()
                .find(|a| a["name"].as_str() == Some(sig_name.as_str()))
                .ok_or_else(|| {
                    Error::msg(format!("no signature for {asset_name}; refusing to stage"))
                })?;
            let sig_url = sig_asset["browser_download_url"]
                .as_str()
                .ok_or_else(|| Error::msg("signature asset missing url"))?;
            let sig_text = fetch_text(client, sig_url).await?;
            verify_signature(pubkey, &bytes, &sig_text)
                .map_err(|e| Error::msg(format!("signature check failed for {asset_name}: {e}")))?;
        }
        None => tracing::warn!(
            "release signing not configured in this build; {asset_name} verified by SHA-256 only"
        ),
    }

    std::fs::write(dest, &bytes)?;
    Ok(())
}

/// Verify a detached minisign signature over `data` against the baked-in
/// release public key. Pure verification (no signing); fails closed on any
/// malformed input.
fn verify_signature(
    pubkey_b64: &str,
    data: &[u8],
    minisig_text: &str,
) -> std::result::Result<(), String> {
    let pk = minisign_verify::PublicKey::from_base64(pubkey_b64)
        .map_err(|e| format!("bad release public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(minisig_text)
        .map_err(|e| format!("bad signature file: {e}"))?;
    pk.verify(data, &sig, false).map_err(|e| e.to_string())
}

// ---- platform asset naming + archive extraction ----------------------

/// `<stem>-<platform>.<ext>`, e.g. `allmystuff-gui-macos-aarch64.tar.gz`.
fn platform_asset(stem: &str) -> String {
    format!("{stem}-{}.{}", platform_triple(), archive_ext())
}

fn platform_triple() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "macos-x86_64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "macos-aarch64"
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "windows-x86_64"
    }
    #[cfg(not(any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        "unknown"
    }
}

fn archive_ext() -> &'static str {
    if cfg!(windows) {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Extract `bin_name` from a `.tar.gz` / `.zip` archive into `out_dir`,
/// returning the extracted binary's path. If `archive` is already a bare
/// binary (legacy), it's returned as-is.
fn extract_binary(archive: &Path, out_dir: &Path, bin_name: &str) -> Result<PathBuf> {
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let out = out_dir.join(bin_name);

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let f = std::fs::File::open(archive)?;
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(f));
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.file_name().and_then(|n| n.to_str()) == Some(bin_name) {
                entry.unpack(&out)?;
                return Ok(out);
            }
        }
        Err(Error::msg(format!("{bin_name} not found in {name}")))
    } else if name.ends_with(".zip") {
        let f = std::fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| Error::msg(e.to_string()))?;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).map_err(|e| Error::msg(e.to_string()))?;
            let fname = Path::new(file.name())
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string);
            if fname.as_deref() == Some(bin_name) {
                let mut dst = std::fs::File::create(&out)?;
                std::io::copy(&mut file, &mut dst)?;
                return Ok(out);
            }
        }
        Err(Error::msg(format!("{bin_name} not found in {name}")))
    } else {
        // Already a bare binary.
        Ok(archive.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALLMYSTUFF_HOME` is process-global; serialize the tests that mutate
    /// it so cargo's parallel runner can't cross their temp dirs.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn attended_checks_ignore_only_the_automatic_update_preference() {
        assert!(check_is_disabled(false, false, false));
        assert!(!check_is_disabled(false, true, false));
        assert!(!check_is_disabled(true, false, false));
        assert!(check_is_disabled(true, true, true));
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_kind_from_path("/opt/homebrew/bin/allmystuff"),
            InstallKind::PackageManager
        );
        assert_eq!(
            detect_install_kind_from_path("/usr/local/Cellar/allmystuff/0.1.0/bin/allmystuff"),
            InstallKind::PackageManager
        );
        assert_eq!(
            detect_install_kind_from_path("/home/me/.local/bin/allmystuff"),
            InstallKind::Raw
        );
    }

    #[test]
    fn windows_package_managers_are_detected_but_program_files_is_not() {
        // Chocolatey and Scoop genuinely own their copy — they must update it.
        assert_eq!(
            detect_install_kind_from_path(
                r"C:\ProgramData\chocolatey\lib\allmystuff\allmystuff.exe"
            ),
            InstallKind::PackageManager
        );
        assert_eq!(
            detect_install_kind_from_path(
                r"C:\Users\me\scoop\apps\allmystuff\current\allmystuff.exe"
            ),
            InstallKind::PackageManager
        );
        // `C:\Program Files\AllMyStuff` is where our OWN MSI lands. Classifying
        // it as package-managed is what silently switched self-update off — the
        // check included — for every MSI install, so the path alone must not
        // decide it. Whether that install is writable is a separate question
        // (`dir_is_writable`), asked at runtime rather than guessed from text.
        assert_eq!(
            detect_install_kind_from_path(r"C:\Program Files\AllMyStuff\allmystuff-gui.exe"),
            InstallKind::Raw
        );
        assert_eq!(
            detect_install_kind_from_path(r"C:\Program Files (x86)\AllMyStuff\allmystuff.exe"),
            InstallKind::Raw
        );
        // The per-user NSIS location was always fine and stays fine.
        assert_eq!(
            detect_install_kind_from_path(
                r"C:\Users\me\AppData\Local\AllMyStuff\allmystuff-gui.exe"
            ),
            InstallKind::Raw
        );
    }

    #[test]
    fn write_probe_tells_a_writable_dir_from_a_missing_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(dir_is_writable(tmp.path()));
        // The probe must clean up after itself — a stray file next to the
        // installed binary on every status() call would be its own bug report.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        // A directory that doesn't exist can't be written to.
        assert!(!dir_is_writable(&tmp.path().join("nope")));
    }

    #[test]
    fn os_bundle_paths_are_left_alone() {
        // A binary inside a macOS .app must never be swapped in place — the
        // updater treats it as bundle-owned and reports nothing to apply.
        assert!(path_in_os_bundle(Path::new(
            "/Applications/AllMyStuff.app/Contents/MacOS/allmystuff-gui"
        )));
        assert!(path_in_os_bundle(Path::new(
            "/Applications/AllMyStuff.app/Contents/MacOS/allmystuff-serve"
        )));
        // Loose installs (portable / curl|sh / dev) are fair game to swap.
        assert!(!path_in_os_bundle(Path::new("/usr/local/bin/allmystuff")));
        assert!(!path_in_os_bundle(Path::new(
            "/home/me/.local/bin/allmystuff-gui"
        )));
        assert!(!path_in_os_bundle(Path::new(
            r"C:\Program Files\AllMyStuff\allmystuff-gui.exe"
        )));
    }

    #[test]
    fn empty_baked_pubkey_is_treated_as_unconfigured() {
        // CI exports ALLMYSTUFF_RELEASE_PUBKEY unconditionally, so an unset repo
        // variable reaches the compiler as Some("") (option_env! of a
        // present-but-empty var), not None. That empty string must degrade to
        // SHA-256-only — never "require a signature verified against an empty
        // key", which made download_verify_stage demand a .minisig that was
        // never published and fail every update closed.
        assert_eq!(normalize_pubkey(Some("")), None);
        assert_eq!(normalize_pubkey(None), None);
        // Whitespace / CR / blank lines only → nothing left → not configured.
        assert_eq!(normalize_pubkey(Some("   \r\n  ")), None);
        assert_eq!(normalize_pubkey(Some("\n\n")), None);
        let real = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        assert_eq!(normalize_pubkey(Some(real)), Some(real));
    }

    #[test]
    fn pasted_minisign_pub_and_stray_whitespace_normalise_to_the_key() {
        // The variable is meant to hold just line 2 of minisign.pub, but pasting
        // the whole two-line file (with CRLF), or leaving a trailing newline, is
        // an easy slip — and made PublicKey::from_base64 reject the comment text
        // / CR as "Invalid encoding in minisign data". normalize_pubkey recovers
        // the key line so the base64 still reaches strict validation downstream.
        let real = "RWRruk/CLy4NQfZshGlgxLS64p+Hqn5yngTMlWzq3HSUPR+tu64wkW4w";
        // Whole minisign.pub pasted, Windows CRLF, trailing newline.
        let whole_file =
            format!("untrusted comment: minisign public key 410D2E2FC24FBA6B\r\n{real}\r\n");
        assert_eq!(normalize_pubkey(Some(&whole_file)), Some(real));
        // Just the key, but with a trailing newline (the common rotation slip).
        assert_eq!(normalize_pubkey(Some(&format!("{real}\n"))), Some(real));
        // Surrounding whitespace.
        assert_eq!(normalize_pubkey(Some(&format!("  {real}  "))), Some(real));
        // Selection is positional — line 2 is the key whatever line 1 says, and
        // a key on its own (no line 2) is used as-is. We don't sniff for an
        // "untrusted comment:" prefix.
        assert_eq!(
            normalize_pubkey(Some(&format!("anything on the first line\n{real}"))),
            Some(real)
        );
        let solo = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        assert_eq!(normalize_pubkey(Some(solo)), Some(solo));
    }

    #[test]
    fn signature_verification_fails_closed_on_garbage() {
        // A real minisign public key (base64, line 2 of a minisign.pub). Any
        // malformed key/signature, or a good key over a non-signature, must
        // return Err — the updater treats every Err here as "refuse to stage".
        let pubkey = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        assert!(verify_signature(pubkey, b"some artifact bytes", "not a signature").is_err());
        assert!(verify_signature("not-a-key", b"data", "also not a signature").is_err());
        assert!(verify_signature("", b"", "").is_err());
    }

    #[test]
    fn platform_asset_names_have_stem_triple_and_ext() {
        let a = platform_asset("allmystuff-gui");
        assert!(a.starts_with("allmystuff-gui-"));
        assert!(a.ends_with(".tar.gz") || a.ends_with(".zip"));
    }

    #[test]
    fn every_half_has_a_distinct_asset_and_bin_name() {
        // The node engine (`allmystuff-serve`) is a first-class half — a
        // regression that dropped it is exactly what left "updated" machines
        // advertising the old version. All three stems/binaries are distinct.
        let stems: Vec<_> = ALL_ARTIFACTS.iter().map(|k| k.asset_stem()).collect();
        assert_eq!(
            stems,
            vec!["allmystuff", "allmystuff-gui", "allmystuff-serve", "amst"]
        );
        assert!(ALL_ARTIFACTS.contains(&ArtifactKind::Serve));
        assert!(ALL_ARTIFACTS.contains(&ArtifactKind::Amst));

        // amst ships under its own short stem, not `allmystuff-amst`.
        let amst = platform_asset(ArtifactKind::Amst.asset_stem());
        assert!(amst.starts_with("amst-"));
        assert_eq!(
            ArtifactKind::Amst.bin_name(),
            if cfg!(windows) { "amst.exe" } else { "amst" }
        );
        assert!(!ArtifactKind::Amst.is_required());

        let serve = platform_asset(ArtifactKind::Serve.asset_stem());
        assert!(serve.starts_with("allmystuff-serve-"));
        // The serve stem must not collide with the CLI stem's prefix scan.
        assert!(!serve.starts_with("allmystuff-windows"));
        assert!(!serve.starts_with("allmystuff-linux"));
        assert!(!serve.starts_with("allmystuff-macos"));

        if cfg!(windows) {
            assert_eq!(ArtifactKind::Serve.bin_name(), "allmystuff-serve.exe");
        } else {
            assert_eq!(ArtifactKind::Serve.bin_name(), "allmystuff-serve");
        }
        // Only the CLI is the required half.
        assert!(ArtifactKind::Cli.is_required());
        assert!(!ArtifactKind::Gui.is_required());
        assert!(!ArtifactKind::Serve.is_required());
    }

    #[test]
    fn artifact_kind_parse_roundtrips() {
        for k in ALL_ARTIFACTS {
            assert_eq!(ArtifactKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ArtifactKind::parse("daemon"), None);
    }

    #[test]
    fn version_gate_allows_newer_and_unknown_only() {
        assert!(version_is_newer("0.1.16", Some("0.1.15")));
        assert!(!version_is_newer("0.1.15", Some("0.1.15")));
        assert!(!version_is_newer("0.1.14", Some("0.1.15")));
        // Unknown installed version (no GUI/node stamp yet) ⇒ sync once, so a
        // sibling installed out of band by the shell installer is brought
        // into lockstep on the first update.
        assert!(version_is_newer("0.1.15", None));
    }

    #[test]
    fn cli_apply_guard_compares_against_running_version() {
        // The CLI arm reads no files — it compares the target against the
        // running binary's own version, so a stale marker can't downgrade it
        // and a no-longer-needed apply is skipped.
        assert!(artifact_needs_apply(ArtifactKind::Cli, "999.0.0"));
        assert!(!artifact_needs_apply(ArtifactKind::Cli, current_version()));
    }

    #[test]
    fn parse_pending_reads_all_kinds_and_skips_junk() {
        let doc = serde_json::json!({
            "version": "0.1.16",
            "artifacts": [
                { "kind": "cli", "path": "/u/0.1.16/allmystuff" },
                { "kind": "gui", "path": "/u/0.1.16/allmystuff-gui" },
                { "kind": "serve", "path": "/u/0.1.16/allmystuff-serve" },
                { "kind": "mystery", "path": "/u/0.1.16/nope" },
                { "kind": "cli" }
            ]
        });
        let arts = parse_pending_artifacts(&doc);
        assert_eq!(arts.len(), 3);
        let kinds: Vec<_> = arts.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            vec![ArtifactKind::Cli, ArtifactKind::Gui, ArtifactKind::Serve]
        );
        assert_eq!(
            arts[2].archive,
            std::path::PathBuf::from("/u/0.1.16/allmystuff-serve")
        );
    }

    #[test]
    fn artifact_version_stamps_round_trip() {
        // GUI/node stamps live under the updates dir; the CLI has none (it's
        // gauged against its own running version).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ALLMYSTUFF_HOME", tmp.path());

        assert!(artifact_version_marker(ArtifactKind::Cli).is_none());
        assert!(installed_artifact_version(ArtifactKind::Serve).is_none());

        record_artifact_version(ArtifactKind::Serve, "0.1.16");
        assert_eq!(
            installed_artifact_version(ArtifactKind::Serve).as_deref(),
            Some("0.1.16")
        );
        // A recorded stamp gates the apply guard: equal/older never re-applies,
        // newer does.
        assert!(!artifact_needs_apply(ArtifactKind::Serve, "0.1.16"));
        assert!(artifact_needs_apply(ArtifactKind::Serve, "0.1.17"));

        std::env::remove_var("ALLMYSTUFF_HOME");
    }

    #[test]
    fn lagging_sibling_is_behind_even_when_the_cli_is_current() {
        // The lockstep bug check_now now guards against: once the running CLI
        // reaches `latest`, the old early `UpToDate` return fired before the
        // sibling stamps were ever consulted, so a serve/gui half left behind
        // by a partial update (or a fresh install that bumped the binary but
        // not the stamp) was stranded and never re-staged. The version gates
        // underpinning the fix must flag that half as behind regardless of the
        // CLI's version — that half is what check_now's `wanted_artifacts`
        // emptiness gate now keys on instead of the running binary alone.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ALLMYSTUFF_HOME", tmp.path());

        // serve stamped a version behind the latest release…
        record_artifact_version(ArtifactKind::Serve, "0.2.0");
        // …is "behind" latest 0.2.1 even though the running CLI is already there.
        assert!(artifact_needs_apply(ArtifactKind::Serve, "0.2.1"));
        assert!(!artifact_needs_apply(ArtifactKind::Cli, current_version()));
        // Once caught up, it's no longer behind — no churn on the next check.
        record_artifact_version(ArtifactKind::Serve, "0.2.1");
        assert!(!artifact_needs_apply(ArtifactKind::Serve, "0.2.1"));

        std::env::remove_var("ALLMYSTUFF_HOME");
    }

    #[test]
    fn only_one_half_of_an_install_runs_the_launch_check() {
        // The duplicate this claim exists to kill: on a desktop box the GUI
        // ticker and the `allmystuff-serve` node it spawns both wake ~30s in
        // and both force past the interval, fetching the same feed seconds
        // apart on every single launch. Whoever claims it first checks; the
        // half starting alongside stands down.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ALLMYSTUFF_HOME", tmp.path());

        assert!(claim_launch_check(), "the first half to start claims it");
        assert!(
            !claim_launch_check(),
            "the half starting alongside it must not fetch the same feed again"
        );

        std::env::remove_var("ALLMYSTUFF_HOME");
    }

    #[test]
    fn a_stale_launch_claim_never_suppresses_a_later_launch() {
        // The claim is a launch-scoped debounce, not a lock: it must not
        // outlive the launch it belongs to. A process killed mid-check leaves
        // its claim behind, and if that were honoured forever the machine
        // would never run another launch check — the exact failure the claim
        // was added to avoid, made permanent.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ALLMYSTUFF_HOME", tmp.path());

        let path = launch_claim_path().unwrap();
        let stale = now_secs() - LAUNCH_CLAIM_SECS - 1;
        std::fs::write(&path, serde_json::json!({ "at": stale }).to_string()).unwrap();

        assert!(
            claim_launch_check(),
            "a claim older than the debounce is from a previous launch"
        );
        // …and the fresh claim it just wrote is honoured again.
        assert!(!claim_launch_check());

        std::env::remove_var("ALLMYSTUFF_HOME");
    }

    #[test]
    fn release_tag_handles_object_and_array() {
        let obj = serde_json::json!({ "tag_name": "v0.2.0" });
        assert_eq!(release_tag(&obj).unwrap(), "0.2.0");
        let arr = serde_json::json!([{ "tag_name": "v0.3.1" }]);
        assert_eq!(release_tag(&arr).unwrap(), "0.3.1");
    }

    #[test]
    fn auto_apply_defaults_to_up_to_minor() {
        // The shipped default: patch + minor bumps apply on their own, major
        // waits. (Existing installs keep whatever they saved.)
        assert_eq!(AutoUpdateConfig::default().auto_apply, "minor");
        assert_eq!(
            ApplyPolicy::parse(&AutoUpdateConfig::default().auto_apply),
            Some(ApplyPolicy::Minor)
        );
    }

    #[test]
    fn config_round_trips_under_a_temp_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ALLMYSTUFF_HOME", tmp.path());
        let au = AutoUpdateConfig {
            channel: "beta".into(),
            auto_apply: "minor".into(),
            ..AutoUpdateConfig::default()
        };
        save_auto_update(&au).unwrap();
        let back = load_auto_update();
        assert_eq!(back.channel, "beta");
        assert_eq!(back.auto_apply, "minor");
        // Other config keys are preserved across an auto_update write.
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path().unwrap()).unwrap())
                .unwrap();
        assert!(cfg.get("auto_update").is_some());
        std::env::remove_var("ALLMYSTUFF_HOME");
    }

    #[test]
    fn extract_binary_pulls_the_named_file_from_a_tar_gz() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("allmystuff-linux-x86_64.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            let payload = b"#!/bin/sh\necho hi\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "allmystuff", &payload[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let bin = extract_binary(&archive, &out, "allmystuff").unwrap();
        assert!(bin.exists());
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::fs::File::open(&bin).unwrap(), &mut s).unwrap();
        assert!(s.contains("echo hi"));
        let _ = writeln!(std::io::sink(), "ok");
    }
}
