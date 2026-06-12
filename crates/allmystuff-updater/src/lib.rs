//! Self-update for AllMyStuff.
//!
//! Modelled on `myownmesh-updater` so `allmystuff update` behaves exactly
//! like `myownmesh update`, but self-contained — it keeps its own state
//! under `~/.allmystuff/` and never links the mesh engine.
//!
//! Two binaries move in lockstep: the `allmystuff` CLI and the
//! `allmystuff-gui` desktop app, shipped as
//! `allmystuff[-gui]-<platform>.{tar.gz,zip}` with a `.sha256` sidecar.
//! The flow is **stage now, apply on next launch**:
//!
//!   1. [`check_now`] / [`update_now`] fetch the release feed, compare
//!      versions, download + SHA-256-verify the platform assets, and
//!      extract them into `~/.allmystuff/updates/<version>/`.
//!   2. [`apply_pending_if_any`] (called first thing in `main`) atomically
//!      renames the staged binaries over the installed ones.
//!
//! Package-manager installs (Homebrew, dpkg/apt, MSI) are detected and
//! left to the OS updater.

pub mod policy;

use std::path::{Path, PathBuf};

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
/// to the base64 key when building the shipped binary). When set, a valid
/// detached signature over each artifact is **required** before it is staged —
/// SHA-256 proves integrity, the signature proves provenance.
const RELEASE_PUBKEY: Option<&str> = option_env!("ALLMYSTUFF_RELEASE_PUBKEY");

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
    "patch".into()
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
// Artifacts: the two binaries we ship.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    Cli,
    Gui,
}

impl ArtifactKind {
    fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Cli => "cli",
            ArtifactKind::Gui => "gui",
        }
    }
    /// Release-asset stem — `allmystuff` / `allmystuff-gui`.
    fn asset_stem(self) -> &'static str {
        match self {
            ArtifactKind::Cli => "allmystuff",
            ArtifactKind::Gui => "allmystuff-gui",
        }
    }
    fn bin_name(self) -> &'static str {
        if cfg!(windows) {
            match self {
                ArtifactKind::Cli => "allmystuff.exe",
                ArtifactKind::Gui => "allmystuff-gui.exe",
            }
        } else {
            self.asset_stem()
        }
    }
}

// ---------------------------------------------------------------------------
// Apply (runs at process start, or on demand).
// ---------------------------------------------------------------------------

/// Apply any staged update before real work starts. Idempotent; errors are
/// logged and swallowed so an update problem never blocks boot. Call first
/// in `main`.
pub fn apply_pending_if_any() {
    if let Err(e) = apply_pending() {
        tracing::warn!("self-update apply skipped: {e}");
    }
}

/// Apply a staged update now, surfacing the applied version (the swap is on
/// disk; it takes effect on next start), or `None` if nothing was pending.
pub fn apply_now() -> Result<Option<String>> {
    apply_pending()
}

fn apply_pending() -> Result<Option<String>> {
    let pending = updates_dir()?.join("pending.json");
    if !pending.exists() {
        return Ok(None);
    }
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    let target_version = doc["version"].as_str().unwrap_or("?").to_string();

    // Downgrade guard: never roll back below what's installed.
    if !version_is_newer(&target_version, installed_version().as_deref()) {
        let _ = std::fs::remove_file(&pending);
        return Ok(None);
    }

    let mut applied = Vec::new();
    if let Some(arts) = doc["artifacts"].as_array() {
        for art in arts {
            let kind = match art["kind"].as_str() {
                Some("cli") => ArtifactKind::Cli,
                Some("gui") => ArtifactKind::Gui,
                _ => continue,
            };
            let Some(archive) = art["path"].as_str() else {
                continue;
            };
            match apply_one(kind, Path::new(archive)) {
                Ok(true) => applied.push(kind.as_str().to_string()),
                Ok(false) => {}
                Err(e) => tracing::warn!("self-update: {} apply skipped: {e}", kind.as_str()),
            }
        }
    }

    let _ = std::fs::remove_file(&pending);
    if applied.is_empty() {
        return Ok(None);
    }
    record_installed_version(&target_version);
    tracing::info!(
        "self-update applied {target_version} ({})",
        applied.join("+")
    );
    Ok(Some(target_version))
}

/// Swap one staged artifact over its installed counterpart. `Ok(false)`
/// when there's nothing installed to replace (e.g. a staged GUI on a
/// CLI-only host).
fn apply_one(kind: ArtifactKind, archive: &Path) -> Result<bool> {
    let Some(target) = installed_path(kind) else {
        return Ok(false);
    };
    let staged_dir = archive
        .parent()
        .ok_or_else(|| Error::msg("staged archive has no parent"))?;
    let binary = extract_binary(archive, staged_dir, kind.bin_name())?;
    atomic_replace(&binary, &target)?;
    Ok(true)
}

/// Where an artifact is installed. The running binary for its own kind;
/// a same-directory sibling for the other (the layout the release bundle
/// installs both halves in).
fn installed_path(kind: ArtifactKind) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let running_is = current
        .file_name()
        .map(|n| n.to_string_lossy() == kind.bin_name())
        .unwrap_or(false);
    if running_is {
        return Some(current);
    }
    let sibling = current.parent()?.join(kind.bin_name());
    sibling.exists().then_some(sibling)
}

/// Atomically replace `target` with `staged`. Same-dir temp + rename so the
/// swap is atomic on the target's filesystem; on Windows a running exe is
/// moved aside first.
fn atomic_replace(staged: &Path, target: &Path) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| Error::msg("target has no parent dir"))?;
    let tmp = dir.join(format!(
        ".{}.new",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("allmystuff")
    ));
    std::fs::copy(staged, &tmp)?;
    copy_exec_perms(target, &tmp);

    #[cfg(windows)]
    {
        // Can't overwrite a running/locked exe; move it aside first.
        let old = dir.join(format!(
            ".{}.old",
            target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("allmystuff")
        ));
        let _ = std::fs::remove_file(&old);
        if target.exists() {
            let _ = std::fs::rename(target, &old);
        }
    }

    std::fs::rename(&tmp, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(unix)]
fn copy_exec_perms(_from: &Path, to: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn copy_exec_perms(_from: &Path, _to: &Path) {}

fn version_is_newer(target: &str, installed: Option<&str>) -> bool {
    match installed {
        Some(v) => compare_semver(target, v) == std::cmp::Ordering::Greater,
        None => true,
    }
}

/// Best-known installed version: the newer of the running binary and the
/// last-applied stamp.
fn installed_version() -> Option<String> {
    let running = current_version().to_string();
    match read_installed_stamp() {
        Some(stamp) if compare_semver(&stamp, &running) == std::cmp::Ordering::Greater => {
            Some(stamp)
        }
        _ => Some(running),
    }
}

fn read_installed_stamp() -> Option<String> {
    let p = updates_dir().ok()?.join("installed.json");
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    doc["version"].as_str().map(str::to_string)
}

fn record_installed_version(version: &str) {
    if let Ok(dir) = updates_dir() {
        let _ = std::fs::write(
            dir.join("installed.json"),
            serde_json::json!({ "version": version }).to_string(),
        );
    }
}

// ---------------------------------------------------------------------------
// Check + stage.
// ---------------------------------------------------------------------------

/// Run one check. With `force`, ignore the interval cooldown. Stages a
/// permitted update; never applies (that happens on next launch).
pub async fn check_now(force: bool) -> Result<CheckOutcome> {
    let au = load_auto_update();
    if !au.enabled || env_disabled() {
        return Ok(CheckOutcome::Disabled);
    }
    if detect_install_kind() == InstallKind::PackageManager {
        return Ok(CheckOutcome::PackageManager);
    }
    if !force && !is_due(au.check_interval_hours) {
        return Ok(CheckOutcome::NotDue);
    }
    stamp_check_now();

    let release = fetch_release(&au).await?;
    let latest = release_tag(&release)?;
    let current = current_version().to_string();

    if compare_semver(&current, &latest) != std::cmp::Ordering::Less {
        return Ok(CheckOutcome::UpToDate { current, latest });
    }

    let pol = ApplyPolicy::parse(&au.auto_apply).unwrap_or(ApplyPolicy::Patch);
    if !policy_allows(pol, &current, &latest) {
        return Ok(CheckOutcome::PolicyBlocked {
            current,
            latest,
            policy: au.auto_apply.clone(),
        });
    }

    stage_release(&release, &latest, &[ArtifactKind::Cli, ArtifactKind::Gui]).await?;
    Ok(CheckOutcome::Staged { version: latest })
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

    if compare_semver(&current, &latest) != std::cmp::Ordering::Less {
        return Ok(UpdateNowOutcome::UpToDate { current, latest });
    }

    stamp_check_now();
    let kinds = stage_release(&release, &latest, &[ArtifactKind::Cli, ArtifactKind::Gui]).await?;
    apply_now()?;
    Ok(UpdateNowOutcome::Updated {
        to: latest,
        components: kinds.iter().map(|k| k.as_str().to_string()).collect(),
    })
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

fn staged_version() -> Option<String> {
    let p = updates_dir().ok()?.join("pending.json");
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    doc["version"].as_str().map(str::to_string)
}

// ---- install-kind detection ------------------------------------------

pub fn detect_install_kind() -> InstallKind {
    let path = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    detect_install_kind_from_path(&path)
}

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
    #[cfg(target_os = "windows")]
    if path_str.contains("\\Program Files\\") || path_str.to_lowercase().contains("\\chocolatey\\")
    {
        return InstallKind::PackageManager;
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
    for &kind in want {
        let asset_name = platform_asset(kind.asset_stem());
        let Some(asset) = assets
            .iter()
            .find(|a| a["name"].as_str() == Some(&asset_name))
        else {
            // The CLI-only or GUI-only release simply omits the other asset.
            continue;
        };
        let url = asset["browser_download_url"]
            .as_str()
            .ok_or_else(|| Error::msg("asset missing download url"))?;
        let dest = dir.join(&asset_name);
        let expected = find_sha256(assets, &asset_name, &client).await;
        let signature = find_signature(assets, &asset_name, &client).await;
        download_and_verify(
            &client,
            url,
            &dest,
            expected.as_deref(),
            signature.as_deref(),
            &asset_name,
        )
        .await?;
        manifest.push(serde_json::json!({
            "kind": kind.as_str(),
            "path": dest.to_string_lossy(),
        }));
        staged.push(kind);
    }
    if staged.is_empty() {
        return Err(Error::msg("no matching platform asset in release"));
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
/// asset, else a `SHA256SUMS` line. `None` if neither is published — staging
/// then fails closed (a published checksum is mandatory).
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

/// Fetch the detached minisign signature (`<asset>.minisig`) for `asset_name`,
/// if the release publishes one. Returns the `.minisig` file's text.
async fn find_signature(
    assets: &[serde_json::Value],
    asset_name: &str,
    client: &reqwest::Client,
) -> Option<String> {
    let sidecar = format!("{asset_name}.minisig");
    let a = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(&sidecar))?;
    let url = a["browser_download_url"].as_str()?;
    fetch_text(client, url).await.ok()
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

async fn download_and_verify(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha256: Option<&str>,
    expected_sig: Option<&str>,
    asset_name: &str,
) -> Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    // Integrity: a published SHA-256 is mandatory. We never stage an
    // unverified binary — a missing checksum used to fall through to a bare
    // warning, which let anyone who could omit the sidecar serve any payload.
    let Some(expected) = expected_sha256 else {
        return Err(Error::msg(format!(
            "no SHA-256 published for {asset_name}; refusing to stage unverified"
        )));
    };
    let actual = hex::encode(Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Error::ChecksumMismatch {
            asset: asset_name.to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    // Authenticity: when a release signing key is baked in, a valid detached
    // signature over the artifact is required. SHA-256 comes from the same
    // release and proves only integrity; the signature is what makes a swapped
    // release asset detectable.
    match RELEASE_PUBKEY {
        Some(pubkey) => {
            let sig_text = expected_sig.ok_or_else(|| {
                Error::msg(format!(
                    "no signature published for {asset_name}; refusing to stage"
                ))
            })?;
            verify_signature(pubkey, &bytes, sig_text)
                .map_err(|e| Error::msg(format!("signature check failed for {asset_name}: {e}")))?;
        }
        None => tracing::warn!(
            "release signing not configured in this build; {asset_name} verified by SHA-256 only"
        ),
    }
    std::fs::write(dest, &bytes)?;
    Ok(())
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

    #[test]
    fn signature_verification_fails_closed_on_garbage() {
        // Malformed key or signature must error, never silently pass — the
        // download path treats any Err here as "do not stage".
        assert!(verify_signature("not-a-valid-key", b"payload", "not-a-sig").is_err());
        assert!(verify_signature("", b"payload", "").is_err());
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
    fn platform_asset_names_have_stem_triple_and_ext() {
        let a = platform_asset("allmystuff-gui");
        assert!(a.starts_with("allmystuff-gui-"));
        assert!(a.ends_with(".tar.gz") || a.ends_with(".zip"));
    }

    #[test]
    fn release_tag_handles_object_and_array() {
        let obj = serde_json::json!({ "tag_name": "v0.2.0" });
        assert_eq!(release_tag(&obj).unwrap(), "0.2.0");
        let arr = serde_json::json!([{ "tag_name": "v0.3.1" }]);
        assert_eq!(release_tag(&arr).unwrap(), "0.3.1");
    }

    #[test]
    fn config_round_trips_under_a_temp_home() {
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
