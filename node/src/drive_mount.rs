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
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::net::TcpListener;

use dav_server::{fakels::FakeLs, DavHandler};

use crate::mesh::Mesh;

#[derive(Debug, Clone, Serialize)]
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
        let mount = choose_mount(&requested_mount, &self.list()).await?;
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
                                async move { Ok::<_, Infallible>(handler.handle(request).await) }
                            }),
                        )
                        .await
                    {
                        tracing::debug!("native drive WebDAV connection ended: {error}");
                    }
                });
            }
        });
        let url = format!("http://localhost:{port}/");
        if let Err(error) = mount_native(&mount, &url).await {
            server_task.abort();
            return Err(error);
        }
        if let Err(error) = label_native(&mount, &label).await {
            tracing::warn!("couldn't label native drive {mount} as {label}: {error}");
        }
        let info = NativeDriveInfo {
            route: route.clone(),
            label,
            mount,
            port,
        };
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
            if let Err(error) = unmount_native(&active.info.mount).await {
                tracing::warn!(
                    "couldn't remove native drive {} for {}: {error}",
                    active.info.mount,
                    active.info.route
                );
            }
            if let Err(error) = clear_native_label(&active.info.mount).await {
                tracing::warn!(
                    "couldn't clear native drive label for {}: {error}",
                    active.info.mount
                );
            }
        });
    }
}

async fn choose_mount(requested: &str, active: &[NativeDriveInfo]) -> Result<String, String> {
    #[cfg(windows)]
    {
        let remembered = remembered_network_mounts().await?;
        let requested = requested.trim().trim_end_matches(['\\', '/']);
        if !requested.is_empty() {
            let mut chars = requested.chars();
            let Some(letter) = chars.next().map(|letter| letter.to_ascii_uppercase()) else {
                return Err("choose a drive letter".into());
            };
            if !letter.is_ascii_alphabetic()
                || !matches!(chars.next(), Some(':'))
                || chars.next().is_some()
            {
                return Err("drive letter must look like X:".into());
            }
            let mount = format!("{letter}:");
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
    #[cfg(not(windows))]
    {
        let _ = (requested, active);
        Err("native drive mounting is currently available on Windows".into())
    }
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
    let output = tokio::process::Command::new("net.exe")
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

#[cfg(windows)]
async fn mount_native(mount: &str, url: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("net.exe")
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

#[cfg(not(windows))]
async fn mount_native(_mount: &str, _url: &str) -> Result<(), String> {
    Err("native drive mounting is currently available on Windows".into())
}

#[cfg(windows)]
async fn unmount_native(mount: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("net.exe")
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

#[cfg(not(windows))]
async fn unmount_native(_mount: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn label_native(mount: &str, label: &str) -> Result<(), String> {
    let letter = mount.trim_end_matches(':');
    let key = format!(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{letter}\DefaultLabel"
    );
    let output = tokio::process::Command::new("reg.exe")
        .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", label, "/f"])
        .output()
        .await
        .map_err(|error| format!("couldn't launch the Explorer label update: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    refresh_explorer_drive_labels().await;
    Ok(())
}

#[cfg(not(windows))]
async fn label_native(_mount: &str, _label: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn clear_native_label(mount: &str) -> Result<(), String> {
    let letter = mount.trim_end_matches(':');
    let key = format!(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{letter}\DefaultLabel"
    );
    // A missing key is already the desired state, so deletion is best-effort.
    let _ = tokio::process::Command::new("reg.exe")
        .args(["delete", &key, "/f"])
        .output()
        .await;
    refresh_explorer_drive_labels().await;
    Ok(())
}

#[cfg(not(windows))]
async fn clear_native_label(_mount: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn refresh_explorer_drive_labels() {
    // Ask Explorer to re-read DriveIcons now; otherwise an already-open This
    // PC window can retain the transport name until its next manual refresh.
    let _ = tokio::process::Command::new("ie4uinit.exe")
        .arg("-show")
        .output()
        .await;
}

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
            let events = self.request(|req| FileEvent::List { req, path }).await?;
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
