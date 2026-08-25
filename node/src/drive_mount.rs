//! Native drive mounts backed by a live mesh Storage route.
//!
//! The operating system talks WebDAV to a loopback-only listener. This
//! adapter turns DAV filesystem operations into the same scoped FileEvent
//! RPCs that cross the encrypted mesh route; no WebDAV listener is exposed
//! to the LAN and no local source path is disclosed to the receiver.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::io::SeekFrom;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use allmystuff_session::{FileEntry, FileEvent};
use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::{future, FutureExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{server::conn::http1, service::service_fn, Request};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use dav_server::{fakels::FakeLs, DavHandler};

use crate::mesh::Mesh;

// `dav-server` deliberately leaves the quota properties out of its special
// empty-body PROPFIND response for Microsoft clients. Windows WebClient uses
// exactly that request for Explorer's capacity query, so `get_quota` is never
// reached even though the mesh source reports the correct used/total values.
// Turn that one shorthand request into an explicit property request before
// handing it to the library. Explicit client bodies remain untouched.
const EMPTY_PROPFIND_WITH_QUOTA: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:">
  <D:prop>
    <D:creationdate/><D:displayname/><D:getcontentlanguage/>
    <D:getcontentlength/><D:getcontenttype/><D:getetag/>
    <D:getlastmodified/><D:lockdiscovery/><D:resourcetype/>
    <D:supportedlock/><D:quota-available-bytes/><D:quota-used-bytes/>
    <Z:Win32CreationTime/><Z:Win32FileAttributes/>
    <Z:Win32LastAccessTime/><Z:Win32LastModifiedTime/>
  </D:prop>
</D:propfind>"#;

fn add_quota_to_empty_propfind<B>(
    request: Request<B>,
) -> Request<http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let should_expand = request.method().as_str() == "PROPFIND" && request.body().is_end_stream();
    let (mut parts, body) = request.into_parts();
    if should_expand {
        use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};

        parts.headers.remove(TRANSFER_ENCODING);
        parts.headers.insert(
            CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("application/xml; charset=utf-8"),
        );
        parts.headers.insert(
            CONTENT_LENGTH,
            hyper::header::HeaderValue::from_str(&EMPTY_PROPFIND_WITH_QUOTA.len().to_string())
                .expect("static PROPFIND length is a valid header"),
        );
        Request::from_parts(
            parts,
            Full::new(Bytes::from_static(EMPTY_PROPFIND_WITH_QUOTA))
                .map_err(|error: Infallible| match error {})
                .boxed_unsync(),
        )
    } else {
        Request::from_parts(parts, body.map_err(std::io::Error::other).boxed_unsync())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeDriveInfo {
    pub route: String,
    pub label: String,
    pub mount: String,
    pub port: u16,
}

struct ActiveMount {
    info: NativeDriveInfo,
    server: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Default)]
pub struct DriveMounts {
    active: Arc<Mutex<HashMap<String, ActiveMount>>>,
}

impl DriveMounts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self) -> Vec<NativeDriveInfo> {
        self.active
            .lock()
            .values()
            .map(|mount| mount.info.clone())
            .collect()
    }

    pub async fn cleanup_stale(&self) {
        cleanup_stale_native_mounts().await;
    }

    /// Remove a native mount that belongs to AllMyStuff even when its live
    /// route has already disappeared. Windows can retain the `net use` entry
    /// after the in-memory mount task is gone; the private registry lease is
    /// the proof that lets us clean that half without touching another app's
    /// drive mapping.
    pub async fn remove_known(&self, mount: &str) -> Result<(), String> {
        let active_route = self
            .active
            .lock()
            .iter()
            .find(|(_, active)| active.info.mount.eq_ignore_ascii_case(mount))
            .map(|(route, _)| route.clone());
        if let Some(route) = active_route {
            self.stop(&route);
            return Ok(());
        }
        #[cfg(windows)]
        {
            remove_known_native_mount(mount).await
        }
        #[cfg(not(windows))]
        {
            remove_known_native_mount(mount).await
        }
    }

    pub async fn mount(
        &self,
        mesh: Arc<Mesh>,
        route: String,
        label: String,
        requested_mount: String,
    ) -> Result<NativeDriveInfo, String> {
        if let Some(existing) = self.active.lock().get(&route) {
            return Ok(existing.info.clone());
        }
        let active = self.list();
        #[cfg(windows)]
        if let Some(requested) = normalize_requested_mount(&requested_mount)? {
            // A reconnect is allowed to reclaim only a letter carrying our
            // private lease marker. This is the half-open state produced when
            // Windows remembers a dead WebDAV mapping after the old route and
            // listener are gone. Never touch a user's unrelated mapping.
            if !active
                .iter()
                .any(|entry| entry.mount.eq_ignore_ascii_case(&requested))
            {
                reclaim_stale_owned_mount(&requested).await?;
            }
        }
        let mount = choose_mount(&requested_mount, &label, &active).await?;
        wait_for_route(&mesh, &route).await?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| format!("couldn't start the local drive bridge: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let server = DavHandler::builder()
            .filesystem(Box::new(RemoteDavFs::new(mesh, route.clone())))
            .locksystem(FakeLs::new())
            .build_handler();
        let server_task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = server.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |request| {
                                let handler = handler.clone();
                                async move {
                                    let request = add_quota_to_empty_propfind(request);
                                    Ok::<_, Infallible>(handler.handle(request).await)
                                }
                            }),
                        )
                        .await
                    {
                        tracing::debug!("native drive WebDAV connection ended: {error}");
                    }
                });
            }
        });
        #[cfg(windows)]
        let url = format!("http://localhost:{port}/");
        #[cfg(not(windows))]
        let url = format!("http://127.0.0.1:{port}/");
        let info = NativeDriveInfo {
            route: route.clone(),
            label: label.clone(),
            mount: mount.clone(),
            port,
        };
        // Unix mounts can survive their node process. Record ownership before
        // invoking the OS helper so even a partial mount followed by an error
        // remains safe to clean on the next start. Windows records ownership
        // in `label_native` below instead.
        if let Err(error) = remember_native_mount(&info) {
            server_task.abort();
            return Err(format!("couldn't record the native drive mount: {error}"));
        }
        // Seed Explorer's network-drive label BEFORE `net use` publishes the
        // mount. On a reconnect Explorer enumerates the new loopback UNC as
        // soon as the mapping appears and caches its generated
        // "DavWWWRoot (\\localhost@port)" name; writing `_LabelFromReg`
        // afterwards leaves the registry correct but the visible name stale.
        // Creating the MountPoints2 value first makes the very first
        // enumeration read the user's chosen label.
        if let Err(error) = label_native(&mount, &label, &route, port).await {
            tracing::warn!("couldn't label native drive {mount} as {label}: {error}");
        }
        if let Err(error) = mount_native(&mount, &url, &label).await {
            server_task.abort();
            // `net use` can create a reconnecting/remembered entry before it
            // reports failure. Remove that partial mapping now; otherwise the
            // next retry sees its own letter as occupied and Explorer keeps a
            // ghost drive around.
            if unmount_native(&mount).await.is_ok() {
                let _ = forget_native_mount(&mount);
            }
            // `label_native` may have written only part of its registry state
            // before failing; cleanup is deliberately unconditional.
            let _ = clear_native_label(&mount, Some(port)).await;
            return Err(error);
        }
        // Explorer can create or refresh its MountPoints2 entry while
        // `net use` publishes the mapping. Re-apply the friendly label after
        // that race as well as before it so new mappings and reconnects both
        // retain the name selected in AllMyStuff.
        if let Err(error) = label_native(&mount, &label, &route, port).await {
            tracing::warn!("couldn't refresh native drive {mount} label {label}: {error}");
        }
        self.active.lock().insert(
            route,
            ActiveMount {
                info: info.clone(),
                server: server_task,
            },
        );
        Ok(info)
    }

    pub fn stop(&self, route: &str) {
        let Some(active) = self.active.lock().remove(route) else {
            return;
        };
        active.server.abort();
        crate::spawn(async move {
            let unmounted = match unmount_native(&active.info.mount).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        "couldn't remove native drive {} for {}: {error}",
                        active.info.mount,
                        active.info.route
                    );
                    false
                }
            };
            if let Err(error) = clear_native_label(&active.info.mount, Some(active.info.port)).await
            {
                tracing::warn!(
                    "couldn't clear native drive label for {}: {error}",
                    active.info.mount
                );
            }
            if unmounted {
                if let Err(error) = forget_native_mount(&active.info.mount) {
                    tracing::warn!(
                        "couldn't forget native drive lease for {}: {error}",
                        active.info.mount
                    );
                }
            }
        });
    }
}

async fn choose_mount(
    requested: &str,
    _label: &str,
    active: &[NativeDriveInfo],
) -> Result<String, String> {
    #[cfg(windows)]
    {
        let remembered = remembered_network_mounts().await?;
        if let Some(mount) = normalize_requested_mount(requested)? {
            if !mount_available(&mount, active, &remembered) {
                return Err(format!("{mount} is already in use"));
            }
            return Ok(mount);
        }
        for letter in ('D'..='Z').rev() {
            let mount = format!("{letter}:");
            if mount_available(&mount, active, &remembered) {
                return Ok(mount);
            }
        }
        Err("there are no free drive letters".into())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        choose_unix_mount(requested, _label, active).await
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (requested, _label, active);
        Err("native drive mounting is not available on this operating system".into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn choose_unix_mount(
    requested: &str,
    label: &str,
    active: &[NativeDriveInfo],
) -> Result<String, String> {
    if !requested.trim().is_empty() {
        let mount = PathBuf::from(requested.trim());
        validate_unix_mount(&mount, active).await?;
        return Ok(mount.to_string_lossy().into_owned());
    }

    let root = default_unix_mount_root()?;
    std::fs::create_dir_all(&root).map_err(|error| {
        format!(
            "couldn't create the AllMyStuff drive folder {}: {error}",
            root.display()
        )
    })?;
    let stem = safe_mount_name(label);
    for suffix in 1..=1_000 {
        let name = if suffix == 1 {
            stem.clone()
        } else {
            format!("{stem} {suffix}")
        };
        let mount = root.join(name);
        if validate_unix_mount(&mount, active).await.is_ok() {
            return Ok(mount.to_string_lossy().into_owned());
        }
    }
    Err("couldn't choose an available mount point".into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn validate_unix_mount(mount: &Path, active: &[NativeDriveInfo]) -> Result<(), String> {
    if !mount.is_absolute() || mount.parent().is_none() {
        return Err("mount point must be an absolute folder path, not the filesystem root".into());
    }
    if active.iter().any(|entry| Path::new(&entry.mount) == mount)
        || native_mount_is_active(&mount.to_string_lossy()).await?
    {
        return Err(format!("{} is already mounted", mount.display()));
    }
    match std::fs::symlink_metadata(mount) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("mount point cannot be a symbolic link".into());
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err("mount point must be a folder".into());
        }
        Ok(_) => {
            let mut entries = std::fs::read_dir(mount).map_err(|error| error.to_string())?;
            if entries.next().is_some() {
                return Err(format!("{} is not empty", mount.display()));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(mount).map_err(|error| {
                format!("couldn't create mount point {}: {error}", mount.display())
            })?;
        }
        Err(error) => return Err(format!("couldn't inspect {}: {error}", mount.display())),
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn safe_mount_name(label: &str) -> String {
    let cleaned = label
        .trim()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let cleaned = cleaned.trim_matches([' ', '.']).trim();
    if cleaned.is_empty() {
        "Remote Drive".into()
    } else {
        cleaned.to_string()
    }
}

#[cfg(target_os = "macos")]
fn default_unix_mount_root() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join("AllMyStuff Drives"))
        .ok_or_else(|| "couldn't find this user's home folder".into())
}

#[cfg(target_os = "linux")]
fn default_unix_mount_root() -> Result<PathBuf, String> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(PathBuf::from("/mnt/allmystuff"))
    } else {
        dirs::home_dir()
            .map(|home| home.join("AllMyStuff Drives"))
            .ok_or_else(|| "couldn't find this user's home folder".into())
    }
}

#[cfg(windows)]
fn normalize_requested_mount(requested: &str) -> Result<Option<String>, String> {
    let requested = requested.trim().trim_end_matches(['\\', '/']);
    if requested.is_empty() {
        return Ok(None);
    }
    let mut chars = requested.chars();
    let Some(letter) = chars.next().map(|letter| letter.to_ascii_uppercase()) else {
        return Err("choose a drive letter".into());
    };
    if !letter.is_ascii_alphabetic() || !matches!(chars.next(), Some(':')) || chars.next().is_some()
    {
        return Err("drive letter must look like X:".into());
    }
    Ok(Some(format!("{letter}:")))
}

#[cfg(windows)]
fn mount_available(
    mount: &str,
    active: &[NativeDriveInfo],
    remembered: &std::collections::HashSet<String>,
) -> bool {
    if std::path::Path::new(&format!("{mount}\\")).exists()
        || active
            .iter()
            .any(|entry| entry.mount.eq_ignore_ascii_case(mount))
        || remembered.contains(&mount.to_ascii_uppercase())
    {
        return false;
    }
    true
}

#[cfg(windows)]
async fn remembered_network_mounts() -> Result<std::collections::HashSet<String>, String> {
    // One snapshot is both faster and more reliable than `net use X:` per
    // candidate: querying a disconnected remembered letter can make Windows
    // attempt a reconnect before it answers. The listing includes connected,
    // disconnected, and reconnecting entries; drive-letter tokens themselves
    // are stable even when the surrounding output is localized.
    let output = crate::child_process::command("net.exe")
        .arg("use")
        .output()
        .await
        .map_err(|error| format!("couldn't inspect Windows drive mappings: {error}"))?;
    if !output.status.success() {
        return Err("Windows couldn't list its existing drive mappings".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter(|token| {
            token.len() == 2
                && token.as_bytes()[0].is_ascii_alphabetic()
                && token.as_bytes()[1] == b':'
        })
        .map(str::to_ascii_uppercase)
        .collect())
}

#[cfg(windows)]
async fn reclaim_stale_owned_mount(mount: &str) -> Result<(), String> {
    let letter = mount.trim_end_matches(':');
    let marker = format!(
        r"{}\Software\AllMyStuff\MappedDrives\{letter}",
        drive_registry_root()?
    );
    let marker_query = crate::child_process::command("reg.exe")
        .args(["query", &marker])
        .output()
        .await
        .map_err(|error| format!("couldn't inspect the AllMyStuff drive lease: {error}"))?;
    if !marker_query.status.success() {
        return Ok(()); // not ours: ordinary availability checks decide
    }
    let port = parse_registry_dword(&marker_query.stdout).and_then(|p| u16::try_from(p).ok());
    tracing::info!("reclaiming stale AllMyStuff drive mapping {mount}");
    let _ = unmount_native(mount).await;
    clear_native_label(mount, port).await?;

    // The Windows redirector may release a disconnected mapping slightly
    // after `net use /delete` returns. Wait for the provider's own listing,
    // not a fixed sleep, so reconnects are both fast and deterministic.
    for _ in 0..30 {
        if !remembered_network_mounts()
            .await?
            .contains(&mount.to_ascii_uppercase())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "Windows is still releasing {mount}; AllMyStuff will retry"
    ))
}

#[cfg(windows)]
async fn remove_known_native_mount(mount: &str) -> Result<(), String> {
    let mount = normalize_requested_mount(mount)?
        .ok_or_else(|| "the saved drive mapping has no drive letter".to_string())?;
    let letter = mount.trim_end_matches(':');
    let marker = format!(
        r"{}\Software\AllMyStuff\MappedDrives\{letter}",
        drive_registry_root()?
    );
    let port = crate::child_process::command("reg.exe")
        .args(["query", &marker, "/v", "Port"])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| parse_registry_dword(&output.stdout))
        .and_then(|port| u16::try_from(port).ok());
    tracing::info!("removing saved AllMyStuff drive mapping {mount}");
    // The persisted mapping relationship is itself ownership proof. Do not
    // require the auxiliary registry lease: a partial/crashed label write can
    // lose that marker while Windows still retains the WebDAV drive.
    let _ = unmount_native(&mount).await;
    clear_native_label(&mount, port).await
}

async fn wait_for_route(mesh: &Arc<Mesh>, route: &str) -> Result<(), String> {
    // Accept and Storage media use separate mesh deliveries. The local route
    // is active before the source necessarily processes its Accept; letting
    // Windows issue the first DAV request in that gap makes the source drop it
    // and the redirector sit on a 30-second retry. Prove the scoped file plane
    // answers first, using the same Stat request Windows is about to make.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let req = mesh.next_file_request_id();
        let probe = FileEvent::Stat {
            req,
            path: String::new(),
        };
        if mesh
            .drive_file_request_timeout(route, probe, Duration::from_secs(1))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("the source accepted the drive but its file connection never became ready".into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command_result(output: std::process::Output, context: String) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    if detail.is_empty() {
        Err(context)
    } else {
        Err(format!("{context}: {detail}"))
    }
}

#[cfg(target_os = "linux")]
async fn native_mount_is_active(mount: &str) -> Result<bool, String> {
    let mount = mount
        .replace('\\', r"\134")
        .replace(' ', r"\040")
        .replace('\t', r"\011")
        .replace('\n', r"\012");
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("couldn't inspect Linux mounts: {error}"))?;
    Ok(mounts.lines().any(|line| {
        line.split_whitespace()
            .nth(4)
            .is_some_and(|mounted| mounted == mount)
    }))
}

#[cfg(target_os = "macos")]
async fn native_mount_is_active(mount: &str) -> Result<bool, String> {
    let output = crate::child_process::command("/sbin/mount")
        .output()
        .await
        .map_err(|error| format!("couldn't inspect macOS mounts: {error}"))?;
    if !output.status.success() {
        return Err("macOS couldn't list its mounted filesystems".into());
    }
    let marker = format!(" on {mount} (");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(&marker)))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_mount_lease_path() -> Option<PathBuf> {
    allmystuff_protocol::myownmesh_state_dir().map(|dir| dir.join("allmystuff-native-mounts.json"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
static NATIVE_MOUNT_LEASE_LOCK: std::sync::LazyLock<Mutex<()>> =
    std::sync::LazyLock::new(|| Mutex::new(()));

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn load_native_mount_leases() -> Vec<NativeDriveInfo> {
    native_mount_lease_path()
        .map(|path| crate::persist::load_json(&path))
        .unwrap_or_default()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn save_native_mount_leases(leases: &[NativeDriveInfo]) -> Result<(), String> {
    let path = native_mount_lease_path()
        .ok_or_else(|| "couldn't find the AllMyStuff state folder".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("couldn't create the state folder: {error}"))?;
    }
    let json = serde_json::to_vec_pretty(leases)
        .map_err(|error| format!("couldn't encode native mount leases: {error}"))?;
    crate::persist::write_atomic(&path, &json)
        .map_err(|error| format!("couldn't save native mount leases: {error}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remember_native_mount(info: &NativeDriveInfo) -> Result<(), String> {
    let _guard = NATIVE_MOUNT_LEASE_LOCK.lock();
    let mut leases = load_native_mount_leases();
    leases.retain(|lease| lease.mount != info.mount && lease.route != info.route);
    leases.push(info.clone());
    save_native_mount_leases(&leases)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn forget_native_mount(mount: &str) -> Result<(), String> {
    let _guard = NATIVE_MOUNT_LEASE_LOCK.lock();
    let mut leases = load_native_mount_leases();
    leases.retain(|lease| lease.mount != mount);
    save_native_mount_leases(&leases)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn remember_native_mount(_info: &NativeDriveInfo) -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn forget_native_mount(_mount: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn remove_known_native_mount(mount: &str) -> Result<(), String> {
    if !load_native_mount_leases()
        .iter()
        .any(|lease| lease.mount == mount)
    {
        return Ok(());
    }
    unmount_native(mount).await?;
    forget_native_mount(mount)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
async fn remove_known_native_mount(_mount: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn mount_native(mount: &str, url: &str, _label: &str) -> Result<(), String> {
    let output = crate::child_process::command("net.exe")
        .args(["use", mount, url, "/persistent:no"])
        .output()
        .await
        .map_err(|error| format!("couldn't launch Windows drive mapping: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            detail
        };
        Err(format!("Windows couldn't map {mount}: {detail}"))
    }
}

#[cfg(target_os = "macos")]
async fn mount_native(mount: &str, url: &str, label: &str) -> Result<(), String> {
    let output = crate::child_process::command("/sbin/mount_webdav")
        .args(["-S", "-v", label, url, mount])
        .output()
        .await
        .map_err(|error| format!("couldn't launch macOS drive mounting: {error}"))?;
    command_result(
        output,
        format!("macOS couldn't mount the remote drive at {mount}"),
    )
}

#[cfg(target_os = "linux")]
async fn mount_native(mount: &str, url: &str, _label: &str) -> Result<(), String> {
    let helper = ["/sbin/mount.davfs", "/usr/sbin/mount.davfs"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
        .unwrap_or("mount.davfs");
    let output = crate::child_process::command(helper)
        .args(["-o", "rw,noexec,nosuid,nodev", url, mount])
        .output()
        .await
        .map_err(|error| {
            format!(
                "couldn't launch Linux drive mounting: {error}. Install the davfs2 package first"
            )
        })?;
    command_result(
        output,
        format!(
            "Linux couldn't mount the remote drive at {mount}. Install davfs2; a non-root user also needs permission to mount that path"
        ),
    )
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
async fn mount_native(_mount: &str, _url: &str, _label: &str) -> Result<(), String> {
    Err("native drive mounting is not available on this operating system".into())
}

#[cfg(windows)]
async fn unmount_native(mount: &str) -> Result<(), String> {
    let output = crate::child_process::command("net.exe")
        .args(["use", mount, "/delete", "/y"])
        .output()
        .await
        .map_err(|error| format!("couldn't launch Windows drive unmapping: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn unmount_native(mount: &str) -> Result<(), String> {
    if native_mount_is_active(mount).await? {
        let output = crate::child_process::command("/sbin/umount")
            .arg(mount)
            .output()
            .await
            .map_err(|error| format!("couldn't launch drive unmounting: {error}"))?;
        command_result(output, format!("couldn't unmount {mount}"))?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
async fn unmount_native(_mount: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn label_native(mount: &str, label: &str, route: &str, port: u16) -> Result<(), String> {
    let letter = mount.trim_end_matches(':');
    let root = drive_registry_root()?;
    let marker = format!(r"{root}\Software\AllMyStuff\MappedDrives\{letter}");
    let drive_icon_key = format!(
        r"{root}\Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{letter}\DefaultLabel"
    );
    // Explorer does not use DriveIcons for a WebDAV network drive. It keys
    // that mount by its UNC transport path and reads `_LabelFromReg` from
    // MountPoints2. Keep DriveIcons too (it covers older shells), but this is
    // the value that changes "DavWWWRoot (\\localhost@12345)" into the name
    // the user chose in AllMyStuff.
    let mount_point_key = format!(
        r"{root}\Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##localhost@{port}#DavWWWRoot"
    );
    let marked = crate::child_process::command("reg.exe")
        .args(["add", &marker, "/ve", "/t", "REG_SZ", "/d", route, "/f"])
        .output()
        .await
        .map_err(|error| format!("couldn't record the AllMyStuff drive lease: {error}"))?;
    if !marked.status.success() {
        return Err(String::from_utf8_lossy(&marked.stderr).trim().to_string());
    }
    let port_string = port.to_string();
    let port_marked = crate::child_process::command("reg.exe")
        .args([
            "add",
            &marker,
            "/v",
            "Port",
            "/t",
            "REG_DWORD",
            "/d",
            &port_string,
            "/f",
        ])
        .output()
        .await
        .map_err(|error| format!("couldn't record the AllMyStuff drive port: {error}"))?;
    if !port_marked.status.success() {
        return Err(String::from_utf8_lossy(&port_marked.stderr)
            .trim()
            .to_string());
    }
    let output = crate::child_process::command("reg.exe")
        .args([
            "add",
            &drive_icon_key,
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            label,
            "/f",
        ])
        .output()
        .await
        .map_err(|error| format!("couldn't launch the Explorer label update: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let mount_label = crate::child_process::command("reg.exe")
        .args([
            "add",
            &mount_point_key,
            "/v",
            "_LabelFromReg",
            "/t",
            "REG_SZ",
            "/d",
            label,
            "/f",
        ])
        .output()
        .await
        .map_err(|error| format!("couldn't set the Explorer network-drive label: {error}"))?;
    if !mount_label.status.success() {
        return Err(String::from_utf8_lossy(&mount_label.stderr)
            .trim()
            .to_string());
    }
    refresh_explorer_drive_labels().await;
    Ok(())
}

#[cfg(not(windows))]
async fn label_native(_mount: &str, _label: &str, _route: &str, _port: u16) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn clear_native_label(mount: &str, port: Option<u16>) -> Result<(), String> {
    let letter = mount.trim_end_matches(':');
    let root = drive_registry_root()?;
    let marker = format!(r"{root}\Software\AllMyStuff\MappedDrives\{letter}");
    let drive_icon_key = format!(
        r"{root}\Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{letter}\DefaultLabel"
    );
    // `/persistent:no` should avoid this key, but a failed or interrupted
    // WebDAV redirector can still leave one behind. This function is called
    // only for a letter carrying our private lease marker.
    let network_key = format!(r"{root}\Network\{letter}");
    let _ = crate::child_process::command("reg.exe")
        .args(["delete", &network_key, "/f"])
        .output()
        .await;
    // A missing key is already the desired state, so deletion is best-effort.
    let _ = crate::child_process::command("reg.exe")
        .args(["delete", &drive_icon_key, "/f"])
        .output()
        .await;
    if let Some(port) = port {
        let mount_point_key = format!(
            r"{root}\Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##localhost@{port}#DavWWWRoot"
        );
        // Leave Explorer's mount-history key intact; it owns that history.
        // Remove only the display value AllMyStuff authored.
        let _ = crate::child_process::command("reg.exe")
            .args(["delete", &mount_point_key, "/v", "_LabelFromReg", "/f"])
            .output()
            .await;
    }
    let _ = crate::child_process::command("reg.exe")
        .args(["delete", &marker, "/f"])
        .output()
        .await;
    refresh_explorer_drive_labels().await;
    Ok(())
}

#[cfg(not(windows))]
async fn clear_native_label(_mount: &str, _port: Option<u16>) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn refresh_explorer_drive_labels() {
    // Ask Explorer to re-read DriveIcons now; otherwise an already-open This
    // PC window can retain the transport name until its next manual refresh.
    let _ = crate::child_process::command("ie4uinit.exe")
        .arg("-show")
        .output()
        .await;
}

#[cfg(windows)]
async fn cleanup_stale_native_mounts() {
    let Ok(root) = drive_registry_root() else {
        tracing::warn!("couldn't select the signed-in user's drive registry hive");
        return;
    };
    let base = format!(r"{root}\Software\AllMyStuff\MappedDrives");
    let Ok(output) = crate::child_process::command("reg.exe")
        .args(["query", &base])
        .output()
        .await
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let letters = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().rsplit('\\').next())
        .filter(|part| part.len() == 1 && part.as_bytes()[0].is_ascii_alphabetic())
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>();
    for letter in letters {
        let mount = format!("{letter}:");
        let marker = format!(r"{base}\{letter}");
        let port = crate::child_process::command("reg.exe")
            .args(["query", &marker, "/v", "Port"])
            .output()
            .await
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| parse_registry_dword(&output.stdout));
        tracing::info!("cleaning stale AllMyStuff drive mapping {mount}");
        let _ = unmount_native(&mount).await;
        let _ = clear_native_label(&mount, port.and_then(|p| u16::try_from(p).ok())).await;
    }
}

#[cfg(windows)]
fn parse_registry_dword(bytes: &[u8]) -> Option<u32> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .find_map(|part| part.strip_prefix("0x"))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
}

/// Registry root for per-user drive state. The Windows service launches the
/// node in the interactive session but retains its LocalSystem token, so HKCU
/// is the service account rather than the Explorer user's hive. The service
/// already supplies that user's SID for IPC ACLs; use the same SID explicitly
/// for drive leases and Explorer labels.
#[cfg(any(windows, test))]
fn registry_root_for_client(client_sid: Option<&str>) -> Result<String, String> {
    let Some(sid) = client_sid else {
        return Ok("HKCU".into());
    };
    let valid = sid.starts_with("S-1-")
        && sid
            .split('-')
            .skip(2)
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !valid {
        return Err("refusing invalid ALLMYSTUFF_CLIENT_SID value".into());
    }
    Ok(format!(r"HKU\{sid}"))
}

#[cfg(windows)]
fn drive_registry_root() -> Result<String, String> {
    let sid = std::env::var("ALLMYSTUFF_CLIENT_SID").ok();
    registry_root_for_client(sid.as_deref())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn cleanup_stale_native_mounts() {
    let leases = load_native_mount_leases();
    if leases.is_empty() {
        return;
    }
    let mut retained = Vec::new();
    for lease in leases {
        tracing::info!("cleaning stale AllMyStuff drive mount {}", lease.mount);
        if let Err(error) = unmount_native(&lease.mount).await {
            tracing::warn!(
                "couldn't clean stale native drive mount {}: {error}",
                lease.mount
            );
            retained.push(lease);
        }
    }
    if let Err(error) = save_native_mount_leases(&retained) {
        tracing::warn!("couldn't update native drive lease cleanup: {error}");
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
async fn cleanup_stale_native_mounts() {}

#[derive(Clone)]
pub struct RemoteDavFs {
    mesh: Arc<Mesh>,
    route: String,
}

impl RemoteDavFs {
    pub fn new(mesh: Arc<Mesh>, route: impl Into<String>) -> Self {
        Self {
            mesh,
            route: route.into(),
        }
    }

    fn path(path: &DavPath) -> String {
        path.as_pathbuf().to_string_lossy().replace('\\', "/")
    }

    async fn request(&self, make: impl FnOnce(u64) -> FileEvent) -> FsResult<Vec<FileEvent>> {
        let req = self.mesh.next_file_request_id();
        self.mesh
            .drive_file_request(&self.route, make(req))
            .await
            .map_err(|reason| map_remote_error(&reason))
    }

    async fn stat(&self, path: String) -> FsResult<RemoteMeta> {
        let events = self.request(|req| FileEvent::Stat { req, path }).await?;
        match events.into_iter().next() {
            Some(FileEvent::Metadata { entry, .. }) => Ok(RemoteMeta(entry)),
            Some(FileEvent::Err { reason, .. }) => Err(map_remote_error(&reason)),
            _ => Err(FsError::GeneralFailure),
        }
    }

    async fn mutate(&self, make: impl FnOnce(u64) -> FileEvent) -> FsResult<()> {
        match self.request(make).await?.into_iter().next() {
            Some(FileEvent::Ok { .. }) => Ok(()),
            Some(FileEvent::Err { reason, .. }) => Err(map_remote_error(&reason)),
            _ => Err(FsError::GeneralFailure),
        }
    }
}

impl DavFileSystem for RemoteDavFs {
    fn get_quota(&self) -> FsFuture<'_, (u64, Option<u64>)> {
        async move {
            let events = self.request(|req| FileEvent::Quota { req }).await?;
            match events.into_iter().next() {
                Some(FileEvent::QuotaInfo { used, total, .. }) => Ok((used, Some(total))),
                Some(FileEvent::Err { reason, .. }) => Err(map_remote_error(&reason)),
                _ => Err(FsError::GeneralFailure),
            }
        }
        .boxed()
    }

    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        let path = Self::path(path);
        async move {
            let current = self.stat(path.clone()).await;
            if options.create_new && current.is_ok() {
                return Err(FsError::Exists);
            }
            let size = match current {
                Ok(meta) if meta.is_dir() => return Err(FsError::Forbidden),
                Ok(meta) => meta.len(),
                Err(FsError::NotFound) if options.create || options.create_new => {
                    self.mutate(|req| FileEvent::WriteRange {
                        req,
                        path: path.clone(),
                        offset: 0,
                        data: Vec::new(),
                        truncate: true,
                    })
                    .await?;
                    0
                }
                Err(error) => return Err(error),
            };
            let size = if options.truncate {
                self.mutate(|req| FileEvent::WriteRange {
                    req,
                    path: path.clone(),
                    offset: 0,
                    data: Vec::new(),
                    truncate: true,
                })
                .await?;
                0
            } else {
                size
            };
            Ok(Box::new(RemoteFile {
                fs: self.clone(),
                path,
                pos: if options.append { size } else { 0 },
                size,
                append: options.append,
            }) as Box<dyn DavFile>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        let path = Self::path(path);
        async move {
            let events = self
                .request(|req| FileEvent::List {
                    req,
                    path,
                    cursor: None,
                    limit: None,
                })
                .await?;
            match events.into_iter().next() {
                Some(FileEvent::Entries { entries, .. }) => {
                    let entries: Vec<Box<dyn DavDirEntry>> = entries
                        .into_iter()
                        .map(|entry| Box::new(RemoteDirEntry(entry)) as Box<dyn DavDirEntry>)
                        .collect();
                    Ok(Box::pin(futures_util::stream::iter(entries).map(Ok))
                        as FsStream<Box<dyn DavDirEntry>>)
                }
                Some(FileEvent::Err { reason, .. }) => Err(map_remote_error(&reason)),
                _ => Err(FsError::GeneralFailure),
            }
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        let path = Self::path(path);
        async move { Ok(Box::new(self.stat(path).await?) as Box<dyn DavMetaData>) }.boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        let path = Self::path(path);
        async move { self.mutate(|req| FileEvent::Mkdir { req, path }).await }.boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        let path = Self::path(path);
        async move { self.mutate(|req| FileEvent::Delete { req, path }).await }.boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.remove_dir(path)
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        let from = Self::path(from);
        let to = Self::path(to);
        async move { self.mutate(|req| FileEvent::Rename { req, from, to }).await }.boxed()
    }
}

#[derive(Clone, Debug)]
struct RemoteMeta(FileEntry);

impl DavMetaData for RemoteMeta {
    fn len(&self) -> u64 {
        self.0.size
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(UNIX_EPOCH + Duration::from_secs(self.0.modified.unwrap_or(0)))
    }

    fn is_dir(&self) -> bool {
        self.0.dir
    }

    fn is_symlink(&self) -> bool {
        self.0.symlink
    }
}

struct RemoteDirEntry(FileEntry);

impl DavDirEntry for RemoteDirEntry {
    fn name(&self) -> Vec<u8> {
        self.0.name.as_bytes().to_vec()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        future::ready(Ok(
            Box::new(RemoteMeta(self.0.clone())) as Box<dyn DavMetaData>
        ))
        .boxed()
    }
}

struct RemoteFile {
    fs: RemoteDavFs,
    path: String,
    pos: u64,
    size: u64,
    append: bool,
}

impl fmt::Debug for RemoteFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteFile")
            .field("path", &self.path)
            .field("pos", &self.pos)
            .finish()
    }
}

impl DavFile for RemoteFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            let meta = self.fs.stat(self.path.clone()).await?;
            self.size = meta.len();
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        let bytes = buf.copy_to_bytes(buf.remaining());
        self.write_bytes(bytes)
    }

    fn write_bytes(&'_ mut self, bytes: Bytes) -> FsFuture<'_, ()> {
        async move {
            if self.append {
                self.pos = self.size;
            }
            let offset = self.pos;
            let len = bytes.len() as u64;
            self.fs
                .mutate(|req| FileEvent::WriteRange {
                    req,
                    path: self.path.clone(),
                    offset,
                    data: bytes.to_vec(),
                    truncate: false,
                })
                .await?;
            self.pos += len;
            self.size = self.size.max(self.pos);
            Ok(())
        }
        .boxed()
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            let events = self
                .fs
                .request(|req| FileEvent::ReadRange {
                    req,
                    path: self.path.clone(),
                    offset: self.pos,
                    len: count as u64,
                })
                .await?;
            let mut out = Vec::new();
            for event in events {
                match event {
                    FileEvent::Chunk { data, total, .. } => {
                        self.size = total;
                        out.extend_from_slice(&data);
                    }
                    FileEvent::Err { reason, .. } => return Err(map_remote_error(&reason)),
                    _ => return Err(FsError::GeneralFailure),
                }
            }
            self.pos += out.len() as u64;
            Ok(Bytes::from(out))
        }
        .boxed()
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let next = match pos {
                SeekFrom::Start(offset) => offset as i128,
                SeekFrom::Current(offset) => self.pos as i128 + offset as i128,
                SeekFrom::End(offset) => self.size as i128 + offset as i128,
            };
            if next < 0 || next > u64::MAX as i128 {
                return Err(FsError::GeneralFailure);
            }
            self.pos = next as u64;
            Ok(self.pos)
        }
        .boxed()
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        future::ready(Ok(())).boxed()
    }
}

fn map_remote_error(reason: &str) -> FsError {
    tracing::warn!("native drive operation failed: {reason}");
    let reason = reason.to_ascii_lowercase();
    if reason.contains("not found") || reason.contains("cannot find") || reason.contains("no such")
    {
        FsError::NotFound
    } else if reason.contains("already") || reason.contains("exists") {
        FsError::Exists
    } else if reason.contains("denied")
        || reason.contains("permission")
        || reason.contains("leaves the mapped")
    {
        FsError::Forbidden
    } else {
        FsError::GeneralFailure
    }
}

#[cfg(test)]
mod registry_tests {
    use super::{add_quota_to_empty_propfind, registry_root_for_client};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty, Full};
    use hyper::Request;

    #[test]
    fn service_drive_state_targets_the_interactive_user_hive() {
        assert_eq!(
            registry_root_for_client(Some("S-1-5-21-1000-2000-3000-1001")).unwrap(),
            r"HKU\S-1-5-21-1000-2000-3000-1001"
        );
        assert_eq!(registry_root_for_client(None).unwrap(), "HKCU");
    }

    #[test]
    fn service_client_sid_cannot_inject_a_registry_path() {
        assert!(registry_root_for_client(Some(r"S-1-5-21\Software")).is_err());
        assert!(registry_root_for_client(Some("not-a-sid")).is_err());
    }

    #[tokio::test]
    async fn empty_propfind_explicitly_requests_drive_capacity() {
        let request = Request::builder()
            .method("PROPFIND")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let request = add_quota_to_empty_propfind(request);

        let body = request.into_body().collect().await.unwrap().to_bytes();
        let xml = String::from_utf8_lossy(&body);
        assert!(xml.contains("quota-available-bytes"));
        assert!(xml.contains("quota-used-bytes"));
    }

    #[tokio::test]
    async fn explicit_propfind_body_is_preserved() {
        let request = Request::builder()
            .method("PROPFIND")
            .body(Full::new(Bytes::from_static(b"<explicit/>")))
            .unwrap();
        let request = add_quota_to_empty_propfind(request);

        let body = request.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"<explicit/>");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{normalize_requested_mount, parse_registry_dword};

    #[test]
    fn parses_allmystuff_drive_marker_port() {
        let output = b"    Port    REG_DWORD    0xf05d\r\n";
        assert_eq!(parse_registry_dword(output), Some(61_533));
    }

    #[test]
    fn requested_drive_letters_are_canonical_and_strict() {
        assert_eq!(
            normalize_requested_mount(" x:\\").unwrap(),
            Some("X:".into())
        );
        assert_eq!(normalize_requested_mount("").unwrap(), None);
        assert!(normalize_requested_mount("XX:").is_err());
        assert!(normalize_requested_mount("1:").is_err());
    }
}
