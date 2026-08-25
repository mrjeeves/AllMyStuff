//! Mesh-native file sessions — the backend of "Open Files".
//!
//! Two halves, one struct (the `TerminalHost` shape):
//!
//!  * **Host** (the machine whose disk is browsed): [`FilesPlane::handle`]
//!    executes one viewer request (list / read / mkdir / rename / delete)
//!    against the local filesystem on its own blocking thread and streams
//!    the response events back over a bounded channel, so a big download
//!    is throttled by the mesh send, never ballooning memory. Uploads
//!    ([`write_piece`]) are the one op handled inline: each piece must
//!    land in arrival order, and a piece is one small append.
//!  * **Viewer** (the machine looking at it): inbound response frames are
//!    buffered per route and pulled by the files window with the same
//!    poke-then-pull watcher pattern the terminal uses ([`ByteQueues`]).
//!
//! No credentials and no sandbox below the user: the mesh already proved
//! who the peer is, the caller gates everything on the owner/fleet rule
//! (the same gate as the terminal — which hands out a whole shell), and
//! ops run as this user with this user's permissions.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use allmystuff_session::{FileEntry, FileEvent};
use parking_lot::Mutex;

use crate::byte_queues::ByteQueues;

/// Raw bytes per `Chunk`/`Write` piece: base64 (×4/3) plus the JSON
/// envelope stays under the daemon channel's ~64 KiB message ceiling.
pub const CHUNK_BYTES: usize = 40 * 1024;
/// Response events in flight per op before its thread blocks — bounded so
/// a slow link applies backpressure to the disk read, not to memory.
const OP_QUEUE: usize = 8;
/// Viewer-side buffer cap. Generous — a preview can be megabytes — but
/// finite, so a wedged window can't balloon; beyond it the oldest chunks
/// go (the files window caps previews well below this anyway).
const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
const LIST_PAGE_DEFAULT: usize = 256;
const LIST_PAGE_MAX: usize = 512;
const LIST_CURSOR_MAX: usize = 64;
const LIST_CURSOR_TTL: Duration = Duration::from_secs(5 * 60);

struct RemoteListCursor {
    requested_path: String,
    shown_path: String,
    home: String,
    reader: std::fs::ReadDir,
    pending: Option<std::fs::DirEntry>,
    touched: Instant,
}

pub struct FilesPlane {
    /// Viewer half: response frames per route, drained by the files
    /// window (the shared poke-then-pull queue plumbing).
    queues: ByteQueues,
    /// Host half: one cancel flag per route, checked between chunks by
    /// in-flight ops — `stop` flips it so a teardown ends a download
    /// mid-stream instead of pumping bytes at a gone peer.
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Live native iterators for upgraded peers: opaque, route-scoped,
    /// TTL'd, and globally capped.
    list_cursors: Arc<Mutex<HashMap<(String, String), RemoteListCursor>>>,
    /// Active Storage routes are explicit volume mappings. Their route id is
    /// bound to the host's real mount root when the route activates; requests
    /// never trust a peer-supplied filesystem path as that root.
    roots: Mutex<HashMap<String, PathBuf>>,
    /// Native filesystem bridges consume request replies directly instead
    /// of routing them through a GUI byte queue.
    waiters: Mutex<HashMap<(String, u64), tokio::sync::mpsc::UnboundedSender<FileEvent>>>,
}

impl Default for FilesPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesPlane {
    pub fn new() -> Self {
        FilesPlane {
            queues: ByteQueues::new(MAX_QUEUED_BYTES),
            cancels: Mutex::new(HashMap::new()),
            list_cursors: Arc::new(Mutex::new(HashMap::new())),
            roots: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
        }
    }

    // ---- host side ----------------------------------------------------

    /// Execute one viewer request against this machine's filesystem,
    /// streaming response events (most ops yield exactly one; `Read`
    /// yields a chunk stream). The blocking fs work runs on its own
    /// thread; dropping the receiver aborts the op at its next send.
    /// `Write` pieces don't come here — see [`write_piece`].
    pub fn handle(
        &self,
        route_id: &str,
        event: FileEvent,
    ) -> tokio::sync::mpsc::Receiver<FileEvent> {
        self.handle_in_root(route_id, event, None)
    }

    /// Execute a request inside one explicitly mapped volume. `root = None`
    /// keeps the owner/fleet whole-machine Files behaviour; a mapped drive
    /// always supplies a root and all viewer paths are treated as virtual
    /// paths below it. Symlinks and `..` cannot escape the volume.
    pub fn handle_in_root(
        &self,
        route_id: &str,
        event: FileEvent,
        root: Option<PathBuf>,
    ) -> tokio::sync::mpsc::Receiver<FileEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel::<FileEvent>(OP_QUEUE);
        let cancel = self.cancel_flag(route_id);
        let rid = route_id.to_string();
        let list_cursors = self.list_cursors.clone();
        let _ = std::thread::Builder::new()
            .name(format!("amst-files-op {rid}"))
            .spawn(move || {
                if let Some(reply) =
                    run_op(event, &tx, &cancel, root.as_deref(), &rid, &list_cursors)
                {
                    let _ = tx.blocking_send(reply);
                }
            });
        rx
    }

    fn cancel_flag(&self, route_id: &str) -> Arc<AtomicBool> {
        self.cancels
            .lock()
            .entry(route_id.to_string())
            .or_default()
            .clone()
    }

    /// Tear down whatever this route had here — in-flight host ops
    /// (cancelled at their next chunk) and/or the viewer buffer.
    /// Idempotent; safe on either side.
    pub fn stop(&self, route_id: &str) {
        if let Some(flag) = self.cancels.lock().remove(route_id) {
            flag.store(true, Ordering::Relaxed);
        }
        self.queues.remove(route_id);
        self.roots.lock().remove(route_id);
        self.list_cursors
            .lock()
            .retain(|(route, _), _| route != route_id);
        self.waiters
            .lock()
            .retain(|(route, _), _| route != route_id);
    }

    pub fn map_root(&self, route_id: &str, root: PathBuf) {
        self.roots.lock().insert(route_id.to_string(), root);
    }

    pub fn mapped_root(&self, route_id: &str) -> Option<PathBuf> {
        self.roots.lock().get(route_id).cloned()
    }

    // ---- viewer side ----------------------------------------------------

    /// Make sure a response buffer exists for `route_id` *before* the
    /// window subscribes — called when the route goes active, so a reply
    /// that races the window boot is kept, not dropped.
    pub fn ensure_queue(&self, route_id: &str) {
        self.queues.ensure(route_id);
    }

    pub fn watch(&self, route_id: &str) -> u64 {
        self.queues.watch(route_id)
    }

    pub fn unwatch(&self, route_id: &str, token: u64) {
        self.queues.unwatch(route_id, token);
    }

    pub fn poll(&self, route_id: &str) -> Vec<u8> {
        self.queues.poll(route_id)
    }

    /// Buffer one inbound response frame (as its JSON bytes) for the
    /// watching window. Returns `true` when the queue went empty →
    /// non-empty — the caller's cue to poke the front-end.
    pub fn enqueue(&self, route_id: &str, bytes: Vec<u8>) -> bool {
        self.queues.enqueue(route_id, bytes)
    }

    pub fn begin_rpc(
        &self,
        route_id: &str,
        req: u64,
    ) -> tokio::sync::mpsc::UnboundedReceiver<FileEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.waiters.lock().insert((route_id.to_string(), req), tx);
        rx
    }

    pub fn cancel_rpc(&self, route_id: &str, req: u64) {
        self.waiters.lock().remove(&(route_id.to_string(), req));
    }

    /// Deliver a response to a native bridge waiter. Returns true when the
    /// event belonged to one, so the GUI queue must not also consume it.
    pub fn deliver_rpc(&self, route_id: &str, event: &FileEvent) -> bool {
        let req = event.req();
        if req == 0 {
            return false;
        }
        let key = (route_id.to_string(), req);
        let terminal = matches!(
            event,
            FileEvent::Entries { .. }
                | FileEvent::VolumeList { .. }
                | FileEvent::QuotaInfo { .. }
                | FileEvent::Metadata { .. }
                | FileEvent::Ok { .. }
                | FileEvent::Err { .. }
                | FileEvent::Chunk { eof: true, .. }
        );
        let mut waiters = self.waiters.lock();
        let Some(waiter) = waiters.get(&key).cloned() else {
            return false;
        };
        if terminal {
            waiters.remove(&key);
        }
        drop(waiters);
        let _ = waiter.send(event.clone());
        true
    }
}

/// Run one request, sending streamed events through `tx`; the returned
/// event (if any) is the final reply. Pure fs + channel work — runs on
/// the op thread.
fn run_op(
    event: FileEvent,
    tx: &tokio::sync::mpsc::Sender<FileEvent>,
    cancel: &AtomicBool,
    root: Option<&Path>,
    route_id: &str,
    list_cursors: &Mutex<HashMap<(String, String), RemoteListCursor>>,
) -> Option<FileEvent> {
    match event {
        FileEvent::Quota { req } => Some(match root {
            Some(root) => match filesystem_quota(root) {
                Ok((used, total)) => FileEvent::QuotaInfo { req, used, total },
                Err(reason) => FileEvent::Err { req, reason },
            },
            None => FileEvent::Err {
                req,
                reason: "quota is only available on a scoped drive route".into(),
            },
        }),
        FileEvent::Volumes { req } => {
            let inv = allmystuff_inventory::scan();
            Some(FileEvent::VolumeList {
                req,
                volumes: inv
                    .storage
                    .into_iter()
                    .filter_map(|volume| {
                        volume
                            .mount_point
                            .map(|path| allmystuff_session::FileVolume {
                                name: volume.name,
                                path,
                                size: volume.total_bytes,
                                removable: volume.removable,
                            })
                    })
                    .collect(),
            })
        }
        FileEvent::List {
            req,
            path,
            cursor,
            limit,
        } => Some(match limit {
            Some(limit) => match list_dir_page(
                &path,
                root,
                route_id,
                cursor.as_deref(),
                if limit == 0 {
                    LIST_PAGE_DEFAULT
                } else {
                    usize::from(limit).min(LIST_PAGE_MAX)
                },
                list_cursors,
                cancel,
            ) {
                Ok((path, home, entries, next_cursor)) => FileEvent::Entries {
                    req,
                    path,
                    home,
                    entries,
                    next_cursor,
                },
                Err(reason) => FileEvent::Err { req, reason },
            },
            None => match list_dir(&path, root) {
                Ok((path, entries)) => FileEvent::Entries {
                    req,
                    path,
                    home: if root.is_some() {
                        "/".into()
                    } else {
                        home_dir_string()
                    },
                    entries,
                    next_cursor: None,
                },
                Err(reason) => FileEvent::Err { req, reason },
            },
        }),
        FileEvent::Read { req, path } => match stream_read(req, &path, tx, cancel, root) {
            Ok(()) => None, // the chunk stream (ending in eof) is the reply
            Err(reason) => Some(FileEvent::Err { req, reason }),
        },
        FileEvent::Stat { req, path } => Some(match stat_path(&path, root) {
            Ok(entry) => FileEvent::Metadata { req, entry },
            Err(reason) => FileEvent::Err { req, reason },
        }),
        FileEvent::ReadRange {
            req,
            path,
            offset,
            len,
        } => match stream_read_range(req, &path, offset, Some(len), tx, cancel, root) {
            Ok(()) => None,
            Err(reason) => Some(FileEvent::Err { req, reason }),
        },
        FileEvent::Mkdir { req, path } => Some(reply(
            req,
            (|| {
                let p = resolve_for(&path, root)?;
                std::fs::create_dir_all(p).map_err(|e| e.to_string())
            })(),
        )),
        FileEvent::Rename { req, from, to } => {
            let r = (|| {
                if root.is_some() && (scoped_is_root(&from) || scoped_is_root(&to)) {
                    return Err("the mapped drive root can't be renamed".to_string());
                }
                let src = resolve_for(&from, root)?;
                let dst = resolve_for(&to, root)?;
                if dst.exists() {
                    Err("something already has that name".to_string())
                } else {
                    std::fs::rename(src, dst).map_err(|e| e.to_string())
                }
            })();
            Some(reply(req, r))
        }
        FileEvent::Delete { req, path } => {
            if root.is_some() && scoped_is_root(&path) {
                return Some(FileEvent::Err {
                    req,
                    reason: "the mapped drive root can't be deleted".into(),
                });
            }
            let p = match resolve_for(&path, root) {
                Ok(p) => p,
                Err(reason) => return Some(FileEvent::Err { req, reason }),
            };
            // Never follow a symlink into deleting what it points at —
            // remove the link itself.
            let r = match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p),
                Ok(_) => std::fs::remove_file(&p),
                Err(e) => Err(e),
            }
            .map_err(|e| e.to_string());
            Some(reply(req, r))
        }
        // Write pieces are handled inline by `write_piece`; response
        // kinds landing here are a confused peer — answer nothing.
        other => {
            tracing::debug!("files op ignoring event: {other:?}");
            None
        }
    }
}

fn filesystem_quota(path: &Path) -> Result<(u64, u64), String> {
    let total = fs2::total_space(path).map_err(|error| error.to_string())?;
    let available = fs2::available_space(path).map_err(|error| error.to_string())?;
    Ok((total.saturating_sub(available.min(total)), total))
}

fn reply(req: u64, r: Result<(), String>) -> FileEvent {
    match r {
        Ok(()) => FileEvent::Ok { req },
        Err(reason) => FileEvent::Err { req, reason },
    }
}

/// Apply one upload piece. Handled inline (not on an op thread) because
/// pieces of one upload must land in arrival order — and one piece is one
/// small append, comparable to the JSON work already done in line. The
/// viewer sends pieces sequentially, so at most one is ever in flight.
/// Returns the reply to send, if any (`Ok` only once the `eof` piece is
/// on disk; errors always answer).
pub fn write_piece(event: &FileEvent) -> Option<FileEvent> {
    write_piece_in_root(event, None)
}

/// The scoped twin of [`write_piece`] used by an explicit drive mapping.
pub fn write_piece_in_root(event: &FileEvent, root: Option<&Path>) -> Option<FileEvent> {
    let FileEvent::Write {
        req,
        path,
        data,
        append,
        eof,
    } = event
    else {
        return None;
    };
    let p = match resolve_for(path, root) {
        Ok(p) => p,
        Err(reason) => return Some(FileEvent::Err { req: *req, reason }),
    };
    let r = (|| -> std::io::Result<()> {
        let mut f = if *append {
            std::fs::OpenOptions::new().append(true).open(&p)?
        } else {
            std::fs::File::create(&p)?
        };
        f.write_all(data)?;
        f.flush()
    })();
    match r {
        Ok(()) if *eof => Some(FileEvent::Ok { req: *req }),
        Ok(()) => None,
        Err(e) => Some(FileEvent::Err {
            req: *req,
            reason: e.to_string(),
        }),
    }
}

/// Apply one random-access write used by the native filesystem bridge.
pub fn write_range_in_root(event: &FileEvent, root: Option<&Path>) -> Option<FileEvent> {
    let FileEvent::WriteRange {
        req,
        path,
        offset,
        data,
        truncate,
    } = event
    else {
        return None;
    };
    let p = match resolve_for(path, root) {
        Ok(p) => p,
        Err(reason) => return Some(FileEvent::Err { req: *req, reason }),
    };
    let r = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        let mut file = options.open(&p)?;
        if *truncate {
            file.set_len(*offset)?;
        }
        file.seek(std::io::SeekFrom::Start(*offset))?;
        file.write_all(data)?;
        file.flush()
    })();
    Some(reply(*req, r.map_err(|e| e.to_string())))
}

/// Resolve a viewer path to a host path: `""`/`"~"` (and `~/…`) mean this
/// user's home; relative paths hang off home too; absolute paths stand.
fn resolve(path: &str) -> PathBuf {
    let home = user_home();
    if path.is_empty() || path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        home.join(p)
    }
}

fn resolve_for(path: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    match root {
        None => Ok(resolve(path)),
        Some(root) => resolve_scoped(root, path),
    }
}

/// Resolve a viewer's virtual path below `root`. Mapped-drive paths always use
/// `/` in the UI regardless of the host OS. Parent traversal is rejected, and
/// the nearest existing ancestor is canonicalized so a symlink cannot turn a
/// harmless-looking child into an escape from the mapped volume.
fn resolve_scoped(root: &Path, logical: &str) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("mapped drive is unavailable: {e}"))?;
    let mut out = root.clone();
    let clean = logical
        .strip_prefix('~')
        .unwrap_or(logical)
        .trim_start_matches(['/', '\\']);
    for part in Path::new(clean).components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("that path leaves the mapped drive".into());
            }
        }
    }

    let mut ancestor = out.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| "that path leaves the mapped drive".to_string())?;
    }
    let real = ancestor
        .canonicalize()
        .map_err(|e| format!("couldn't resolve that path: {e}"))?;
    if !real.starts_with(&root) {
        return Err("that path leaves the mapped drive".into());
    }
    if out.exists() {
        let real_out = out
            .canonicalize()
            .map_err(|e| format!("couldn't resolve that path: {e}"))?;
        if !real_out.starts_with(&root) {
            return Err("that path leaves the mapped drive".into());
        }
    }
    Ok(out)
}

fn scoped_display_path(path: &str) -> Result<String, String> {
    let mut names = Vec::new();
    let clean = path
        .strip_prefix('~')
        .unwrap_or(path)
        .trim_start_matches(['/', '\\']);
    for part in Path::new(clean).components() {
        match part {
            Component::Normal(name) => names.push(name.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("that path leaves the mapped drive".into());
            }
        }
    }
    Ok(if names.is_empty() {
        "/".into()
    } else {
        format!("/{}", names.join("/"))
    })
}

fn scoped_is_root(path: &str) -> bool {
    matches!(path.trim(), "" | "/" | "\\" | "~" | "~/" | "~\\")
}

fn home_dir_string() -> String {
    user_home().to_string_lossy().into_owned()
}

fn user_home() -> PathBuf {
    std::env::var_os("ALLMYSTUFF_USER_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn native_file_id(path: &Path, meta: &std::fs::Metadata, symlink: bool) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let _ = (path, symlink);
        return Some(format!("unix:{:x}:{:x}", meta.dev(), meta.ino()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let _ = meta;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS
                    | if symlink {
                        FILE_FLAG_OPEN_REPARSE_POINT
                    } else {
                        0
                    },
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
        unsafe { CloseHandle(handle) };
        if !ok {
            return None;
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        return Some(format!("windows:{:x}:{index:x}", info.dwVolumeSerialNumber));
    }
    #[allow(unreachable_code)]
    None
}

fn native_hidden(name: &str, meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        let _ = name;
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        return meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0;
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        name.starts_with('.')
    }
}

fn listed_file_entry(entry: std::fs::DirEntry) -> FileEntry {
    let name = entry.file_name().to_string_lossy().into_owned();
    let path = entry.path();
    let identity_meta = std::fs::symlink_metadata(&path).ok();
    let symlink = identity_meta.as_ref().is_some_and(|meta| meta.is_symlink());
    let meta = std::fs::metadata(&path).ok();
    let dir = meta.as_ref().is_some_and(|meta| meta.is_dir());
    FileEntry {
        native_id: identity_meta
            .as_ref()
            .and_then(|meta| native_file_id(&path, meta, symlink)),
        hidden: identity_meta
            .as_ref()
            .is_some_and(|meta| native_hidden(&name, meta)),
        name,
        dir,
        size: if dir {
            0
        } else {
            meta.as_ref().map(|meta| meta.len()).unwrap_or(0)
        },
        modified: meta
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
        symlink,
    }
}

fn next_dir_entry(
    reader: &mut std::fs::ReadDir,
    cancel: &AtomicBool,
) -> Result<Option<std::fs::DirEntry>, String> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("file route closed".into());
        }
        match reader.next() {
            Some(Ok(entry)) => return Ok(Some(entry)),
            Some(Err(_)) => continue,
            None => return Ok(None),
        }
    }
}

fn new_list_cursor_token() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| format!("mint list cursor: {error}"))?;
    let mut token = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

fn list_dir_page(
    path: &str,
    root: Option<&Path>,
    route_id: &str,
    cursor: Option<&str>,
    limit: usize,
    cursors: &Mutex<HashMap<(String, String), RemoteListCursor>>,
    cancel: &AtomicBool,
) -> Result<(String, String, Vec<FileEntry>, Option<String>), String> {
    let now = Instant::now();
    let existing_token = cursor.map(str::to_owned);
    let mut state = if let Some(token) = cursor {
        if token.len() > 128 || token.is_empty() {
            return Err("invalid directory cursor".into());
        }
        let mut cursors = cursors.lock();
        cursors.retain(|_, state| now.duration_since(state.touched) <= LIST_CURSOR_TTL);
        let key = (route_id.to_string(), token.to_string());
        let state = cursors
            .remove(&key)
            .ok_or("that directory page expired; refresh the folder")?;
        if state.requested_path != path {
            return Err("directory cursor does not match this path".into());
        }
        state
    } else {
        {
            let mut cursors = cursors.lock();
            cursors.retain(|_, state| now.duration_since(state.touched) <= LIST_CURSOR_TTL);
            if cursors.len() >= LIST_CURSOR_MAX {
                return Err("too many directory pages are open; finish or refresh one".into());
            }
        }
        let dir = resolve_for(path, root)?;
        let reader = std::fs::read_dir(&dir).map_err(|error| error.to_string())?;
        let shown_path = match root {
            Some(_) => scoped_display_path(path)?,
            None => dir.to_string_lossy().into_owned(),
        };
        RemoteListCursor {
            requested_path: path.to_string(),
            shown_path,
            home: if root.is_some() {
                "/".into()
            } else {
                home_dir_string()
            },
            reader,
            pending: None,
            touched: now,
        }
    };

    let mut entries = Vec::with_capacity(limit);
    if let Some(entry) = state.pending.take() {
        entries.push(listed_file_entry(entry));
    }
    while entries.len() < limit {
        let Some(entry) = next_dir_entry(&mut state.reader, cancel)? else {
            break;
        };
        entries.push(listed_file_entry(entry));
    }
    state.pending = next_dir_entry(&mut state.reader, cancel)?;
    let shown_path = state.shown_path.clone();
    let home = state.home.clone();
    let next_cursor = if state.pending.is_some() {
        let token = match existing_token {
            Some(token) => token,
            None => new_list_cursor_token()?,
        };
        state.touched = now;
        let mut cursors = cursors.lock();
        cursors.retain(|_, state| now.duration_since(state.touched) <= LIST_CURSOR_TTL);
        if cursors.len() >= LIST_CURSOR_MAX {
            return Err("too many directory pages are open; refresh this folder".into());
        }
        cursors.insert((route_id.to_string(), token.clone()), state);
        Some(token)
    } else {
        None
    };

    Ok((shown_path, home, entries, next_cursor))
}

fn list_dir(path: &str, root: Option<&Path>) -> Result<(String, Vec<FileEntry>), String> {
    let dir = resolve_for(path, root)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let p = entry.path();
        let identity_meta = std::fs::symlink_metadata(&p).ok();
        let symlink = identity_meta.as_ref().is_some_and(|m| m.is_symlink());
        // Follow links for dir-ness/size so a symlinked folder is
        // navigable; a broken link reads as a 0-byte file.
        let meta = std::fs::metadata(&p).ok();
        let dir_flag = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        entries.push(FileEntry {
            native_id: identity_meta
                .as_ref()
                .and_then(|meta| native_file_id(&p, meta, symlink)),
            hidden: identity_meta
                .as_ref()
                .is_some_and(|meta| native_hidden(&name, meta)),
            name,
            dir: dir_flag,
            size: if dir_flag {
                0
            } else {
                meta.as_ref().map(|m| m.len()).unwrap_or(0)
            },
            modified: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            symlink,
        });
    }
    let shown = match root {
        Some(_) => scoped_display_path(path)?,
        None => dir.to_string_lossy().into_owned(),
    };
    Ok((shown, entries))
}

fn stat_path(path: &str, root: Option<&Path>) -> Result<FileEntry, String> {
    let p = resolve_for(path, root)?;
    let symlink_meta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
    let symlink = symlink_meta.is_symlink();
    let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
    let dir = meta.is_dir();
    let name = if root.is_some() && scoped_is_root(path) {
        String::new()
    } else {
        p.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(FileEntry {
        native_id: native_file_id(&p, &symlink_meta, symlink),
        hidden: native_hidden(&name, &symlink_meta),
        name,
        dir,
        size: if dir { 0 } else { meta.len() },
        modified: meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
        symlink,
    })
}

/// Stream one file as `Chunk` events (the last marked `eof`), checking
/// the route's cancel flag between pieces. `blocking_send` is the flow
/// control: a slow mesh send fills the channel and parks the read here.
fn stream_read(
    req: u64,
    path: &str,
    tx: &tokio::sync::mpsc::Sender<FileEvent>,
    cancel: &AtomicBool,
    root: Option<&Path>,
) -> Result<(), String> {
    stream_read_range(req, path, 0, None, tx, cancel, root)
}

fn stream_read_range(
    req: u64,
    path: &str,
    offset: u64,
    len: Option<u64>,
    tx: &tokio::sync::mpsc::Sender<FileEvent>,
    cancel: &AtomicBool,
    root: Option<&Path>,
) -> Result<(), String> {
    let p = resolve_for(path, root)?;
    let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("that's a folder".into());
    }
    let total = meta.len();
    let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
    f.seek(std::io::SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut sent: u64 = 0;
    let wanted = len.unwrap_or_else(|| total.saturating_sub(offset));
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let remaining = wanted.saturating_sub(sent);
        let take = remaining.min(buf.len() as u64) as usize;
        let n = if take == 0 {
            0
        } else {
            f.read(&mut buf[..take]).map_err(|e| e.to_string())?
        };
        let eof = n == 0 || sent + n as u64 >= wanted || offset + sent + n as u64 >= total;
        let chunk = FileEvent::Chunk {
            req,
            data: buf[..n].to_vec(),
            total,
            eof,
        };
        sent += n as u64;
        if tx.blocking_send(chunk).is_err() {
            // Receiver gone — the pump (and likely the route) ended.
            return Ok(());
        }
        if eof {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn drain(mut rx: tokio::sync::mpsc::Receiver<FileEvent>) -> Vec<FileEvent> {
        let mut out = Vec::new();
        // Ops run on their own thread; blocking_recv waits for each event.
        while let Some(ev) = rx.blocking_recv() {
            out.push(ev);
        }
        out
    }

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "amst-files-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn list_read_roundtrip() {
        let dir = tempdir("list");
        std::fs::write(dir.join("hello.txt"), b"hello files").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();

        let plane = FilesPlane::new();
        let events = drain(plane.handle(
            "r1",
            FileEvent::List {
                req: 1,
                path: dir.to_string_lossy().into_owned(),
                cursor: None,
                limit: None,
            },
        ));
        let [FileEvent::Entries {
            req: 1, entries, ..
        }] = events.as_slice()
        else {
            panic!("expected one Entries, got {events:?}");
        };
        let file = entries.iter().find(|e| e.name == "hello.txt").unwrap();
        assert!(!file.dir);
        assert_eq!(file.size, 11);
        assert!(entries.iter().any(|e| e.name == "sub" && e.dir));

        let events = drain(plane.handle(
            "r1",
            FileEvent::Read {
                req: 2,
                path: dir.join("hello.txt").to_string_lossy().into_owned(),
            },
        ));
        let mut bytes = Vec::new();
        for ev in &events {
            let FileEvent::Chunk { data, total, .. } = ev else {
                panic!("expected chunks, got {ev:?}");
            };
            assert_eq!(*total, 11);
            bytes.extend_from_slice(data);
        }
        assert_eq!(bytes, b"hello files");
        assert!(matches!(
            events.last(),
            Some(FileEvent::Chunk { eof: true, .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upgraded_directory_lists_are_bounded_and_continuable() {
        let dir = tempdir("paged-list");
        for index in 0..5 {
            std::fs::write(dir.join(format!("file-{index}.txt")), [index]).unwrap();
        }
        let shown = dir.to_string_lossy().into_owned();
        let plane = FilesPlane::new();
        let mut cursor = None;
        let mut names = std::collections::HashSet::new();
        for req in 1..=3 {
            let events = drain(plane.handle(
                "paged-route",
                FileEvent::List {
                    req,
                    path: shown.clone(),
                    cursor,
                    limit: Some(2),
                },
            ));
            let [FileEvent::Entries {
                entries,
                next_cursor,
                ..
            }] = events.as_slice()
            else {
                panic!("expected one paged Entries reply, got {events:?}");
            };
            assert!(entries.len() <= 2);
            names.extend(entries.iter().map(|entry| entry.name.clone()));
            cursor = next_cursor.clone();
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(names.len(), 5);
        assert!(cursor.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_streams_big_files_in_capped_chunks() {
        let dir = tempdir("big");
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("big.bin"), &body).unwrap();

        let plane = FilesPlane::new();
        let events = drain(plane.handle(
            "r1",
            FileEvent::Read {
                req: 1,
                path: dir.join("big.bin").to_string_lossy().into_owned(),
            },
        ));
        assert!(events.len() > 1, "split into several chunks");
        let mut bytes = Vec::new();
        for ev in &events {
            let FileEvent::Chunk { data, .. } = ev else {
                panic!("expected chunks");
            };
            assert!(data.len() <= CHUNK_BYTES);
            bytes.extend_from_slice(data);
        }
        assert_eq!(bytes, body, "byte-exact across chunks");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_pieces_assemble_in_order_and_reply_on_eof() {
        let dir = tempdir("write");
        let path = dir.join("up.bin").to_string_lossy().into_owned();

        // First piece creates; later pieces append; only eof answers.
        let first = write_piece(&FileEvent::Write {
            req: 7,
            path: path.clone(),
            data: b"hello ".to_vec(),
            append: false,
            eof: false,
        });
        assert_eq!(first, None, "mid-upload pieces are silent");
        let last = write_piece(&FileEvent::Write {
            req: 7,
            path: path.clone(),
            data: b"upload".to_vec(),
            append: true,
            eof: true,
        });
        assert_eq!(last, Some(FileEvent::Ok { req: 7 }));
        assert_eq!(std::fs::read(resolve(&path)).unwrap(), b"hello upload");

        // A failing piece answers Err whatever its position.
        let bad = write_piece(&FileEvent::Write {
            req: 8,
            path: dir.join("no/such/dir/x").to_string_lossy().into_owned(),
            data: vec![1],
            append: false,
            eof: false,
        });
        assert!(matches!(bad, Some(FileEvent::Err { req: 8, .. })));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mkdir_rename_delete_roundtrip() {
        let dir = tempdir("manage");
        let plane = FilesPlane::new();

        let sub = dir.join("made/here").to_string_lossy().into_owned();
        let events = drain(plane.handle(
            "r1",
            FileEvent::Mkdir {
                req: 1,
                path: sub.clone(),
            },
        ));
        assert_eq!(events, vec![FileEvent::Ok { req: 1 }]);
        assert!(resolve(&sub).is_dir());

        let renamed = dir.join("made/there").to_string_lossy().into_owned();
        let events = drain(plane.handle(
            "r1",
            FileEvent::Rename {
                req: 2,
                from: sub.clone(),
                to: renamed.clone(),
            },
        ));
        assert_eq!(events, vec![FileEvent::Ok { req: 2 }]);
        assert!(!resolve(&sub).exists());
        assert!(resolve(&renamed).is_dir());

        // Rename refuses to clobber something that already exists.
        std::fs::create_dir_all(dir.join("occupied")).unwrap();
        let events = drain(plane.handle(
            "r1",
            FileEvent::Rename {
                req: 3,
                from: renamed.clone(),
                to: dir.join("occupied").to_string_lossy().into_owned(),
            },
        ));
        assert!(matches!(events.as_slice(), [FileEvent::Err { req: 3, .. }]));

        let events = drain(plane.handle(
            "r1",
            FileEvent::Delete {
                req: 4,
                path: dir.join("made").to_string_lossy().into_owned(),
            },
        ));
        assert_eq!(events, vec![FileEvent::Ok { req: 4 }]);
        assert!(!dir.join("made").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn errors_carry_the_reason_not_a_panic() {
        let plane = FilesPlane::new();
        let dir = tempdir("errs");
        let missing = dir.join("nope").to_string_lossy().into_owned();
        for (req, ev) in [
            (
                1,
                FileEvent::List {
                    req: 1,
                    path: missing.clone(),
                    cursor: None,
                    limit: None,
                },
            ),
            (
                2,
                FileEvent::Read {
                    req: 2,
                    path: missing.clone(),
                },
            ),
            (
                3,
                FileEvent::Delete {
                    req: 3,
                    path: missing.clone(),
                },
            ),
        ] {
            let events = drain(plane.handle("r1", ev));
            assert!(
                matches!(events.as_slice(), [FileEvent::Err { req: r, .. }] if *r == req),
                "req {req}: {events:?}"
            );
        }
        // Reading a directory is refused in the viewer's own terms.
        let events = drain(plane.handle(
            "r1",
            FileEvent::Read {
                req: 4,
                path: dir.to_string_lossy().into_owned(),
            },
        ));
        assert!(
            matches!(events.as_slice(), [FileEvent::Err { req: 4, reason }] if reason.contains("folder"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_cancels_a_read_mid_stream() {
        let dir = tempdir("cancel");
        // Big enough that the bounded channel parks the reader.
        let body = vec![7u8; CHUNK_BYTES * (OP_QUEUE + 4)];
        std::fs::write(dir.join("big.bin"), &body).unwrap();

        let plane = FilesPlane::new();
        let mut rx = plane.handle(
            "r1",
            FileEvent::Read {
                req: 1,
                path: dir.join("big.bin").to_string_lossy().into_owned(),
            },
        );
        // Take one chunk, then stop the route; the op must end (with a
        // cancelled error or silence), never stream the whole file.
        let first = rx.blocking_recv().expect("first chunk");
        assert!(matches!(first, FileEvent::Chunk { .. }));
        plane.stop("r1");
        let mut chunks = 1;
        while let Some(ev) = rx.blocking_recv() {
            if matches!(ev, FileEvent::Chunk { .. }) {
                chunks += 1;
            }
        }
        assert!(
            chunks <= OP_QUEUE + 2,
            "stopped read kept streaming: {chunks} chunks"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn paths_resolve_against_home() {
        let home = user_home();
        assert_eq!(resolve(""), home);
        assert_eq!(resolve("~"), home);
        assert_eq!(resolve("~/docs"), home.join("docs"));
        assert_eq!(resolve("plain"), home.join("plain"));
        let abs = if cfg!(windows) { "C:\\x" } else { "/x" };
        assert_eq!(resolve(abs), Path::new(abs));
    }

    #[test]
    fn mapped_drive_is_rooted_and_uses_virtual_paths() {
        let root = tempdir("mapped");
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/note.txt"), b"mapped").unwrap();
        let plane = FilesPlane::new();

        let events = drain(plane.handle_in_root(
            "mapped-1",
            FileEvent::List {
                req: 1,
                path: "~".into(),
                cursor: None,
                limit: None,
            },
            Some(root.clone()),
        ));
        assert!(matches!(
            events.as_slice(),
            [FileEvent::Entries { path, home, entries, .. }]
                if path == "/" && home == "/" && entries.iter().any(|e| e.name == "docs")
        ));

        let events = drain(plane.handle_in_root(
            "mapped-1",
            FileEvent::Quota { req: 4 },
            Some(root.clone()),
        ));
        assert!(matches!(
            events.as_slice(),
            [FileEvent::QuotaInfo {
                req: 4, used, total
            }] if used <= total && *total > 0
        ));

        let events = drain(plane.handle_in_root(
            "mapped-1",
            FileEvent::Read {
                req: 2,
                path: "/docs/note.txt".into(),
            },
            Some(root.clone()),
        ));
        assert!(
            matches!(events.as_slice(), [FileEvent::Chunk { data, eof: true, .. }] if data == b"mapped")
        );

        let events = drain(plane.handle_in_root(
            "mapped-1",
            FileEvent::Delete {
                req: 3,
                path: "/".into(),
            },
            Some(root.clone()),
        ));
        assert!(matches!(
            events.as_slice(),
            [FileEvent::Err { req: 3, reason }] if reason.contains("root can't be deleted")
        ));
        assert!(
            root.exists(),
            "a mapped root must never delete the drive itself"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mapped_drive_rejects_parent_escape() {
        let root = tempdir("mapped-escape");
        let plane = FilesPlane::new();
        let events = drain(plane.handle_in_root(
            "mapped-2",
            FileEvent::List {
                req: 9,
                path: "/../".into(),
                cursor: None,
                limit: None,
            },
            Some(root.clone()),
        ));
        assert!(matches!(
            events.as_slice(),
            [FileEvent::Err { req: 9, reason }] if reason.contains("leaves the mapped drive")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
