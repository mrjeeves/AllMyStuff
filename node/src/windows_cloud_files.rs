//! Native Windows Cloud Files provider shared by Fleetfiles and scoped drives.
//!
//! Windows calls into this module for directory population and aligned byte
//! ranges. Every network request remains on the authorized mesh file route.
//! Local shared-drive mutations are journaled before upload and recovered at
//! reconnect; a drive letter is only an alias for the durable local sync root.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use allmystuff_session::{FileEntry, FileEvent};
use notify::{EventKind, RecursiveMode, Watcher};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE;
use windows_sys::Win32::Storage::CloudFilters::*;
use windows_sys::Win32::Storage::FileSystem::*;

use crate::mesh::Mesh;

const RANGE_BYTES: u64 = 1024 * 1024;
const WORK_QUEUE: usize = 128;
const CF_ALIGNMENT: u64 = 4096;

pub(crate) fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn check(result: i32, action: &str) -> Result<(), String> {
    if result < 0 {
        Err(format!("{action}: Windows error 0x{:08x}", result as u32))
    } else {
        Ok(())
    }
}

fn relative_path(value: &str) -> Result<String, String> {
    let value = value.replace('\\', "/");
    if value.contains('\0')
        || value.contains(':')
        || value.starts_with('/')
        || value.split('/').any(|part| part == ".." || part == ".")
        || value.split('/').any(|part| part.ends_with([' ', '.']))
        || value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err("a Cloud Files path escaped its sync root".into());
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn child_path(parent: &str, name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err("invalid remote filename".into());
    }
    relative_path(&if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    })
}

fn internal(path: &str) -> bool {
    path.split('/').any(|part| part.starts_with(".allmystuff-"))
}

fn metadata(entry: &FileEntry) -> Result<CF_FS_METADATA, String> {
    let time = entry
        .modified
        .unwrap_or(0)
        .saturating_mul(10_000_000)
        .saturating_add(116_444_736_000_000_000)
        .min(i64::MAX as u64) as i64;
    Ok(CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: time,
            LastAccessTime: time,
            LastWriteTime: time,
            ChangeTime: time,
            FileAttributes: if entry.dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            } | if entry.hidden {
                FILE_ATTRIBUTE_HIDDEN
            } else {
                0
            },
        },
        FileSize: i64::try_from(entry.size)
            .map_err(|_| "remote file exceeds Windows' signed 64-bit file range")?,
    })
}

fn attr_file(path: &Path) -> std::io::Result<File> {
    metadata_file(path, FILE_READ_ATTRIBUTES)
}

fn metadata_file(path: &Path, access: u32) -> std::io::Result<File> {
    OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

fn on_disk_exists(path: &Path) -> bool {
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            wide(path).as_ptr(),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_ON_DISK_ENTRIES_ONLY,
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return false;
    }
    unsafe {
        FindClose(handle);
    }
    true
}

pub(crate) fn is_in_sync(path: &Path) -> bool {
    let Ok(file) = attr_file(path) else {
        return false;
    };
    let mut buffer = vec![0_u64; 1024];
    let result = unsafe {
        CfGetPlaceholderInfo(
            file.as_raw_handle() as _,
            CF_PLACEHOLDER_INFO_BASIC,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            std::ptr::null_mut(),
        )
    };
    result >= 0
        && unsafe {
            (*(buffer.as_ptr().cast::<CF_PLACEHOLDER_BASIC_INFO>())).InSyncState
                == CF_IN_SYNC_STATE_IN_SYNC
        }
}

fn mark_synced(path: &Path, identity: &str) -> Result<(), String> {
    let file =
        metadata_file(path, FILE_READ_ATTRIBUTES | WRITE_DAC).map_err(|error| error.to_string())?;
    mark_synced_handle(&file, identity)
}

fn mark_synced_handle(file: &File, identity: &str) -> Result<(), String> {
    // Callers request WRITE_DAC when opening this exact file. Conversion
    // requires it, and retaining the handle preserves identity across renames.
    if !is_placeholder_handle(file) {
        check(
            unsafe {
                CfConvertToPlaceholder(
                    file.as_raw_handle() as _,
                    identity.as_ptr().cast(),
                    identity.len() as u32,
                    CF_CONVERT_FLAG_MARK_IN_SYNC
                        | if file.metadata().map_err(|error| error.to_string())?.is_dir() {
                            CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION
                        } else {
                            0
                        },
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            "convert uploaded file",
        )?;
    } else {
        check(
            unsafe {
                CfSetInSyncState(
                    file.as_raw_handle() as _,
                    CF_IN_SYNC_STATE_IN_SYNC,
                    CF_SET_IN_SYNC_FLAG_NONE,
                    std::ptr::null_mut(),
                )
            },
            "mark uploaded file in sync",
        )?;
    }
    Ok(())
}

fn is_placeholder(path: &Path) -> bool {
    attr_file(path).is_ok_and(|file| is_placeholder_handle(&file))
}

fn is_placeholder_handle(file: &File) -> bool {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    (unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    }) != 0
        && unsafe { CfGetPlaceholderStateFromAttributeTag(info.FileAttributes, info.ReparseTag) }
            & CF_PLACEHOLDER_STATE_PLACEHOLDER
            != 0
}

#[derive(Clone, Copy)]
struct Request {
    connection: i64,
    transfer: i64,
    request: i64,
}
impl Request {
    fn info(self, kind: i32) -> CF_OPERATION_INFO {
        CF_OPERATION_INFO {
            StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
            Type: kind,
            ConnectionKey: self.connection,
            TransferKey: self.transfer,
            RequestKey: self.request,
            ..Default::default()
        }
    }
    fn data(self, offset: u64, bytes: &[u8], length: u64, status: i32) -> Result<(), String> {
        let data = CF_OPERATION_PARAMETERS_0_0 {
            CompletionStatus: status,
            Buffer: if bytes.is_empty() {
                std::ptr::null()
            } else {
                bytes.as_ptr().cast()
            },
            Offset: offset as i64,
            Length: length as i64,
            ..Default::default()
        };
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + std::mem::size_of_val(&data)) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 { TransferData: data },
        };
        check(
            unsafe { CfExecute(&self.info(CF_OPERATION_TYPE_TRANSFER_DATA), &mut params) },
            "transfer Cloud Files range",
        )
    }
    fn placeholders(
        self,
        entries: &mut [CF_PLACEHOLDER_CREATE_INFO],
        total: i64,
        status: i32,
        complete: bool,
    ) -> Result<(), String> {
        let data = CF_OPERATION_PARAMETERS_0_4 {
            Flags: if complete {
                CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION
            } else {
                0
            },
            CompletionStatus: status,
            PlaceholderTotalCount: total,
            PlaceholderArray: entries.as_mut_ptr(),
            PlaceholderCount: entries.len() as u32,
            ..Default::default()
        };
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous)
                + std::mem::size_of_val(&data)) as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                TransferPlaceholders: data,
            },
        };
        check(
            unsafe {
                CfExecute(
                    &self.info(CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS),
                    &mut params,
                )
            },
            "populate Cloud Files directory",
        )
    }
}

enum Work {
    Fetch(Request, String, u64, u64, u64),
    Populate(Request, String),
    Changed,
    Refresh(String),
    RefreshAll,
}

#[derive(Clone, Serialize, Deserialize)]
enum Mutation {
    Upload(String),
    Rename(String, String),
    Delete(String),
}

pub(crate) fn has_pending_drive_edits(root: &Path) -> Result<bool, String> {
    use sha2::{Digest, Sha256};
    let identity = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
    let database = allmystuff_protocol::myownmesh_state_dir()
        .ok_or("AllMyStuff state is unavailable")?
        .join("cloud-provider")
        .join(format!("{identity}.sqlite3"));
    if !database.exists() {
        return Ok(false);
    }
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| error.to_string())?;
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pending) OR EXISTS(SELECT 1 FROM uploads)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

struct Journal(Mutex<Connection>);
impl Journal {
    fn open(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|error| error.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS entries(path TEXT PRIMARY KEY, parent TEXT NOT NULL, metadata TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS directories(path TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS pending(id INTEGER PRIMARY KEY AUTOINCREMENT, mutation TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS uploads(path TEXT PRIMARY KEY, metadata TEXT NOT NULL, bytes INTEGER NOT NULL, modified INTEGER NOT NULL);")
            .map_err(|error| error.to_string())?;
        Ok(Self(Mutex::new(db)))
    }
    fn record(&self, path: &str, entry: &FileEntry) -> Result<(), String> {
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        self.0.lock().execute("INSERT INTO entries VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET metadata=excluded.metadata",
            params![path, parent, serde_json::to_string(entry).map_err(|error| error.to_string())?])
            .map(|_| ()).map_err(|error| error.to_string())
    }
    fn get(&self, path: &str) -> Option<FileEntry> {
        self.0
            .lock()
            .query_row(
                "SELECT metadata FROM entries WHERE path=?1",
                [path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok())
    }
    fn children(&self, path: &str) -> Result<Vec<String>, String> {
        let db = self.0.lock();
        let mut query = db
            .prepare("SELECT path FROM entries WHERE parent=?1 ORDER BY path")
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map([path], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }
    fn directories(&self) -> Result<Vec<String>, String> {
        let db = self.0.lock();
        let mut query = db
            .prepare("SELECT path FROM directories ORDER BY path")
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map([], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        let mut paths = rows
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| error.to_string())?;
        // A fresh journal has no previously populated directory. Always seed
        // the namespace root so initial registration exposes its real entries.
        if !paths.iter().any(|path| path.is_empty()) {
            paths.insert(0, String::new());
        }
        Ok(paths)
    }
    fn populated(&self, path: &str) -> Result<(), String> {
        self.0
            .lock()
            .execute("INSERT OR IGNORE INTO directories VALUES(?1)", [path])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    fn enqueue(&self, mutation: &Mutation) -> Result<(), String> {
        let value = serde_json::to_string(mutation).map_err(|error| error.to_string())?;
        let mut db = self.0.lock();
        let transaction = db.transaction().map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM pending WHERE mutation=?1", [&value])
            .map_err(|error| error.to_string())?;
        transaction
            .execute("INSERT INTO pending(mutation) VALUES(?1)", [&value])
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }
    fn pending(&self) -> Result<Option<(i64, Mutation)>, String> {
        let row = self
            .0
            .lock()
            .query_row(
                "SELECT id,mutation FROM pending ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        row.map(|(id, value)| {
            serde_json::from_str(&value)
                .map(|mutation| (id, mutation))
                .map_err(|error| error.to_string())
        })
        .transpose()
    }
    fn acknowledge(&self, id: i64) -> Result<(), String> {
        self.0
            .lock()
            .execute("DELETE FROM pending WHERE id=?1", [id])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    fn forget(&self, path: &str) -> Result<(), String> {
        self.0
            .lock()
            .execute("DELETE FROM entries WHERE path=?1", [path])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    fn receipt(&self, path: &str) -> Result<Option<(FileEntry, u64, u64)>, String> {
        let row = self
            .0
            .lock()
            .query_row(
                "SELECT metadata,bytes,modified FROM uploads WHERE path=?1",
                [path],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        row.map(|(entry, bytes, modified)| {
            serde_json::from_str(&entry)
                .map(|entry| (entry, bytes, modified))
                .map_err(|error| error.to_string())
        })
        .transpose()
    }
    fn save_receipt(
        &self,
        path: &str,
        entry: &FileEntry,
        bytes: u64,
        modified: u64,
    ) -> Result<(), String> {
        self.0.lock().execute("INSERT INTO uploads VALUES(?1,?2,?3,?4) ON CONFLICT(path) DO UPDATE SET metadata=excluded.metadata,bytes=excluded.bytes,modified=excluded.modified",params![path,serde_json::to_string(entry).map_err(|error|error.to_string())?,bytes,modified]).map(|_|()).map_err(|error|error.to_string())
    }
    fn clear_receipt(&self, path: &str) -> Result<(), String> {
        self.0
            .lock()
            .execute("DELETE FROM uploads WHERE path=?1", [path])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let mut db = self.0.lock();
        let transaction = db.transaction().map_err(|error| error.to_string())?;
        let rows = {
            let mut query=transaction.prepare("SELECT path,metadata FROM entries WHERE path=?1 OR substr(path,1,length(?1)+1)=?1||'/'").map_err(|error|error.to_string())?;
            let rows = query
                .query_map([from], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        transaction
            .execute(
                "DELETE FROM entries WHERE path=?1 OR substr(path,1,length(?1)+1)=?1||'/'",
                [from],
            )
            .map_err(|error| error.to_string())?;
        for (path, metadata) in rows {
            let renamed = format!("{to}{}", &path[from.len()..]);
            let parent = renamed
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("");
            transaction
                .execute(
                    "INSERT OR REPLACE INTO entries VALUES(?1,?2,?3)",
                    params![renamed, parent, metadata],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute(
                "DELETE FROM directories WHERE path=?1 OR substr(path,1,length(?1)+1)=?1||'/'",
                [from],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }
}

struct Provider {
    mesh: Weak<Mesh>,
    route: Option<String>,
    root: PathBuf,
    journal: Journal,
    runtime: tokio::runtime::Handle,
    connection: AtomicI64,
    stopped: AtomicBool,
    tx: mpsc::Sender<Work>,
    watches: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    cancelled: Mutex<BTreeSet<(i64, i64)>>,
    populations: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl Provider {
    fn recover_local_changes(&self) -> Result<(), String> {
        // Enumerate only directory records already on disk. Recovery must
        // never hydrate the remote tree or mistake cloud reparse points for
        // symlinks. Native dirty state covers a crash before notification.
        let mut pending = vec![self.root.clone()];
        while let Some(directory) = pending.pop() {
            let mut data = WIN32_FIND_DATAW::default();
            let search = wide(directory.join("*"));
            let handle = unsafe {
                FindFirstFileExW(
                    search.as_ptr(),
                    FindExInfoBasic,
                    (&mut data as *mut WIN32_FIND_DATAW).cast(),
                    FindExSearchNameMatch,
                    std::ptr::null(),
                    FIND_FIRST_EX_ON_DISK_ENTRIES_ONLY,
                )
            };
            if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                continue;
            }
            loop {
                let length = data
                    .cFileName
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(data.cFileName.len());
                let name = String::from_utf16_lossy(&data.cFileName[..length]);
                if name != "." && name != ".." && !name.starts_with(".allmystuff-") {
                    let path = directory.join(&name);
                    let state = unsafe {
                        CfGetPlaceholderStateFromAttributeTag(
                            data.dwFileAttributes,
                            data.dwReserved0,
                        )
                    };
                    let reparse = data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                    if !reparse || state & CF_PLACEHOLDER_STATE_PLACEHOLDER != 0 {
                        let relative = self.relative(&path)?;
                        if !is_in_sync(&path) {
                            self.journal.enqueue(&Mutation::Upload(relative.clone()))?;
                        }
                        if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
                            && (!reparse || state & CF_PLACEHOLDER_STATE_PARTIAL == 0)
                        {
                            pending.push(path);
                        }
                    }
                }
                if unsafe { FindNextFileW(handle, &mut data) } == 0 {
                    break;
                }
            }
            unsafe {
                FindClose(handle);
            }
        }
        Ok(())
    }
    fn path(&self, relative: &str) -> Result<PathBuf, String> {
        Ok(self.root.join(relative_path(relative)?.replace('/', "\\")))
    }
    fn relative(&self, path: &Path) -> Result<String, String> {
        let path = path
            .strip_prefix(&self.root)
            .map_err(|_| "Windows returned a path outside the Cloud Files root")?;
        if path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("invalid Cloud Files relative path".into());
        }
        relative_path(&path.to_string_lossy())
    }
    fn mesh(&self) -> Result<Arc<Mesh>, String> {
        self.mesh
            .upgrade()
            .ok_or_else(|| "AllMyStuff is stopping".into())
    }
    async fn list(
        &self,
        path: &str,
        cursor: Option<String>,
    ) -> Result<(Vec<FileEntry>, Option<String>), String> {
        let mesh = self.mesh()?;
        if let Some(route) = &self.route {
            let events = mesh
                .drive_file_request(
                    route,
                    FileEvent::List {
                        req: mesh.next_file_request_id(),
                        path: path.into(),
                        cursor,
                        limit: Some(256),
                    },
                )
                .await?;
            for event in events {
                match event {
                    FileEvent::Entries {
                        entries,
                        next_cursor,
                        ..
                    } => return Ok((entries, next_cursor)),
                    FileEvent::Err { reason, .. } => return Err(reason),
                    _ => {}
                }
            }
            Err("the source did not return a directory page".into())
        } else {
            let page = mesh.cloud_fleetfiles_list(path, cursor.as_deref(), 256)?;
            Ok((
                page.entries
                    .into_iter()
                    .map(|entry| FileEntry {
                        name: entry.name,
                        native_id: Some(format!(
                            "{}:{}",
                            entry.version.counter, entry.version.actor
                        )),
                        hidden: false,
                        dir: entry.kind == "directory",
                        size: entry.size,
                        modified: u64::try_from(entry.modified).ok(),
                        symlink: false,
                    })
                    .collect(),
                page.next_cursor,
            ))
        }
    }
    async fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let mesh = self.mesh()?;
        if let Some(route) = &self.route {
            let events = mesh
                .drive_file_request(
                    route,
                    FileEvent::ReadRange {
                        req: mesh.next_file_request_id(),
                        path: path.into(),
                        offset,
                        len: length,
                    },
                )
                .await?;
            let mut bytes = Vec::new();
            for event in events {
                match event {
                    FileEvent::Chunk { data, .. } => bytes.extend(data),
                    FileEvent::Err { reason, .. } => return Err(reason),
                    _ => {}
                }
            }
            Ok(bytes)
        } else {
            mesh.cloud_fleetfiles_read(path, offset, length).await
        }
    }
    async fn mutate(&self, make: impl FnOnce(u64) -> FileEvent) -> Result<(), String> {
        let mesh = self.mesh()?;
        let route = self
            .route
            .as_ref()
            .ok_or("Fleetfiles mutations use the existing version ledger")?;
        for event in mesh
            .drive_file_request(route, make(mesh.next_file_request_id()))
            .await?
        {
            match event {
                FileEvent::Ok { .. } => return Ok(()),
                FileEvent::Err { reason, .. } => return Err(reason),
                _ => {}
            }
        }
        Err("the source did not acknowledge the file operation".into())
    }
    async fn source_entry(&self, path: &str) -> Result<Option<FileEntry>, String> {
        let mesh = self.mesh()?;
        let Some(route) = &self.route else {
            return Ok(None);
        };
        // One Stat request supplies both existence and conflict metadata.
        let events = mesh
            .drive_file_request(
                route,
                FileEvent::Stat {
                    req: mesh.next_file_request_id(),
                    path: path.to_string(),
                },
            )
            .await
            .map_err(|error| format!("stat {path}: {error}"))?;
        for event in events {
            match event {
                FileEvent::Metadata { entry, .. } => return Ok(Some(entry)),
                FileEvent::Err { reason, .. } => {
                    let message = reason.to_ascii_lowercase();
                    if message.contains("not found")
                        || message.contains("cannot find")
                        || message.contains("no such")
                        || message.contains("(os error 2)")
                        || message.contains("(os error 3)")
                    {
                        return Ok(None);
                    }
                    return Err(reason);
                }
                _ => {}
            }
        }
        Err(format!(
            "the source returned no metadata for {path}; local edits remain pending"
        ))
    }
    async fn check_source_version(&self, path: &str) -> Result<(), String> {
        let Some(entry) = self.source_entry(path).await? else {
            return Ok(());
        };
        if self.journal.get(path).is_some_and(|previous| {
            previous.dir == entry.dir
                && previous.size == entry.size
                && previous.modified == entry.modified
                && previous.native_id == entry.native_id
        }) {
            return Ok(());
        }
        Err(format!("{path} changed on the source while local edits were pending; both copies were preserved"))
    }
    async fn upload_source_version(&self, path: &str, backup: &str) -> Result<(), String> {
        if self.source_entry(path).await?.is_some() {
            return self.check_source_version(path).await;
        }
        // An absent target is expected only for a new file or after our own
        // interrupted replacement moved the previous body aside.
        match (self.journal.get(path), self.source_entry(backup).await?) {
            (None, None) => Ok(()),
            (Some(expected), Some(actual)) if same_source_body(&expected, &actual) => Ok(()),
            _ => Err(format!(
                "{path} disappeared or its replacement backup changed; local edits remain pending"
            )),
        }
    }
    async fn remove_upload_backup(&self, path: &str, backup: &str) -> Result<(), String> {
        if let Some(actual) = self.source_entry(backup).await? {
            if !self
                .journal
                .get(path)
                .is_some_and(|expected| same_source_body(&expected, &actual))
            {
                return Err(
                    "the previous source body could not be verified; its backup was preserved"
                        .into(),
                );
            }
            self.mutate(|req| FileEvent::Delete {
                req,
                path: backup.to_string(),
            })
            .await?;
        }
        Ok(())
    }
    async fn fetch(
        &self,
        request: Request,
        path: &str,
        offset: u64,
        length: u64,
        size: u64,
    ) -> Result<(), String> {
        let end = offset
            .checked_add(length)
            .ok_or("Cloud Files range overflow")?
            .min(size);
        let mut offset = offset / CF_ALIGNMENT * CF_ALIGNMENT;
        while offset < end {
            if self.stopped.load(Ordering::Acquire)
                || self
                    .cancelled
                    .lock()
                    .contains(&(request.transfer, request.request))
            {
                return Err("Cloud Files hydration was cancelled".into());
            }
            // Bounded read-ahead avoids a mesh round trip for every small
            // application read, while random seeks still fetch at most 1 MiB.
            let wanted = RANGE_BYTES.min(size - offset);
            let bytes = self.read(path, offset, wanted).await?;
            if bytes.len() as u64 != wanted {
                return Err("the source file changed or ended during hydration".into());
            }
            request.data(offset, &bytes, bytes.len() as u64, 0)?;
            offset += bytes.len() as u64;
        }
        Ok(())
    }
    async fn populate(
        self: &Arc<Self>,
        request: Option<Request>,
        path: &str,
    ) -> Result<(), String> {
        let lock = {
            let mut locks = self.populations.lock();
            if locks.len() > 128 {
                locks.retain(|_, lock| lock.strong_count() > 0);
            }
            if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(path.to_string(), Arc::downgrade(&lock));
                lock
            }
        };
        let _guard = lock.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        let mut transferred = 0_i64;
        loop {
            let (entries, next) = self.list(path, cursor).await?;
            let mut names = Vec::new();
            let mut identities = Vec::new();
            let mut fresh = Vec::new();
            for entry in entries {
                if entry.symlink {
                    continue;
                }
                let relative = child_path(path, &entry.name)?;
                if internal(&relative) {
                    continue;
                }
                seen.insert(relative.clone());
                let local = self.path(&relative)?;
                // Ordinary existence checks can recursively ask Cloud Files
                // to populate this same directory. Query on-disk names only.
                if on_disk_exists(&local) {
                    if entry.dir && !is_placeholder(&local) {
                        // Existing Fleetfiles directories predate native
                        // placeholders. Enable population without reading or
                        // changing any existing file bodies.
                        mark_synced(&local, &relative)?;
                    }
                    if let Some(previous) = self.journal.get(&relative) {
                        if !entry.dir
                            && (entry.size != previous.size
                                || entry.modified != previous.modified
                                || entry.native_id != previous.native_id)
                            && is_in_sync(&local)
                        {
                            update_placeholder(&local, &entry, &relative)?;
                        }
                    }
                    if is_in_sync(&local) {
                        self.journal.record(&relative, &entry)?;
                    }
                    continue;
                }
                if relative.len() > CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize {
                    return Err(
                        "a remote path exceeds the Windows placeholder identity budget".into(),
                    );
                }
                self.journal.record(&relative, &entry)?;
                names.push(wide(&entry.name));
                identities.push(relative.into_bytes());
                fresh.push(entry);
            }
            let mut placeholders = fresh
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    Ok(CF_PLACEHOLDER_CREATE_INFO {
                        RelativeFileName: names[index].as_ptr(),
                        FsMetadata: metadata(entry)?,
                        FileIdentity: identities[index].as_ptr().cast(),
                        FileIdentityLength: identities[index].len() as u32,
                        Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
                        ..Default::default()
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if let Some(request) = request {
                transferred += placeholders.len() as i64;
                request.placeholders(
                    &mut placeholders,
                    if next.is_none() {
                        transferred
                    } else {
                        i64::MAX
                    },
                    0,
                    next.is_none(),
                )?;
            } else if !placeholders.is_empty() {
                let base = wide(self.path(path)?);
                check(
                    unsafe {
                        CfCreatePlaceholders(
                            base.as_ptr(),
                            placeholders.as_mut_ptr(),
                            placeholders.len() as u32,
                            CF_CREATE_FLAG_NONE,
                            std::ptr::null_mut(),
                        )
                    },
                    "refresh Cloud Files placeholders",
                )?;
            }
            for placeholder in placeholders {
                check(placeholder.Result, "create Cloud Files placeholder")?;
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        self.journal.populated(path)?;
        // A remote delete must not erase local unsent edits. Only known,
        // clean placeholders may be removed by reconciliation.
        for absent in self.journal.children(path)? {
            if seen.contains(&absent) {
                continue;
            }
            let local = self.path(&absent)?;
            if is_in_sync(&local) {
                self.journal.forget(&absent)?;
                if local.is_dir() {
                    let _ = std::fs::remove_dir(&local);
                } else {
                    let _ = std::fs::remove_file(&local);
                }
            }
        }
        self.ensure_watch(path);
        Ok(())
    }
    fn ensure_watch(self: &Arc<Self>, path: &str) {
        let Some(route) = self.route.clone() else {
            return;
        };
        let mut watches = self.watches.lock();
        if watches.contains_key(path) || watches.len() >= 32 {
            return;
        }
        let weak = Arc::downgrade(self);
        let path = path.to_string();
        let watched_path = path.clone();
        let task = self.runtime.spawn(async move {
            let Some(provider) = weak.upgrade() else { return; };
            let Ok(mesh) = provider.mesh() else { return; };
            drop(provider);
            loop {
                let Ok((req,mut events)) = mesh.cloud_watch_directory(&route, &watched_path).await else { return; };
                let lease = tokio::time::sleep(Duration::from_secs(25 * 60));
                tokio::pin!(lease);
                loop {
                    tokio::select! {
                        event = events.recv() => match event {
                            Some(FileEvent::DirectoryChanged { .. }) => {
                                if let Some(provider) = weak.upgrade() { let _ = provider.tx.try_send(Work::Refresh(watched_path.clone())); } else { mesh.cloud_cancel_watch(&route, req); return; }
                            }
                            Some(FileEvent::Watching { .. }) => {}
                            _ => { mesh.cloud_cancel_watch(&route, req); return; }
                        },
                        _ = &mut lease => break,
                    }
                }
                mesh.cloud_cancel_watch(&route, req);
            }
        });
        watches.insert(path, task);
    }
    fn changed(&self, event: notify::Event) {
        if self.stopped.load(Ordering::Acquire) || matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let mutation = if matches!(
            event.kind,
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both
            ))
        ) && event.paths.len() == 2
        {
            self.relative(&event.paths[0]).and_then(|from| {
                self.relative(&event.paths[1])
                    .map(|to| Mutation::Rename(from, to))
            })
        } else {
            for path in &event.paths {
                let Ok(relative) = self.relative(path) else {
                    continue;
                };
                if relative.is_empty() || internal(&relative) || is_in_sync(path) {
                    continue;
                }
                let mutation = if path.exists() {
                    Mutation::Upload(relative)
                } else if self.journal.get(&relative).is_some() {
                    Mutation::Delete(relative)
                } else {
                    continue;
                };
                if let Err(error) = self.journal.enqueue(&mutation) {
                    tracing::error!(%error, "couldn't journal Windows file change");
                }
            }
            let _ = self.tx.try_send(Work::Changed);
            return;
        };
        if let Ok(mutation) = mutation {
            if let Err(error) = self.journal.enqueue(&mutation) {
                tracing::error!(%error, "couldn't journal Windows file rename");
            }
            let _ = self.tx.try_send(Work::Changed);
        }
    }
    async fn drain(&self) -> Result<(), String> {
        while let Some((id, mutation)) = self.journal.pending()? {
            if self.stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            match mutation {
                Mutation::Upload(path) => {
                    let local = self.path(&path)?;
                    if !local.exists() || is_in_sync(&local) {
                        self.journal.acknowledge(id)?;
                        continue;
                    }
                    if self.route.is_none() {
                        let hydrate_path = local.clone();
                        tokio::task::spawn_blocking(move || hydrate_local(&hydrate_path))
                            .await
                            .map_err(|error| error.to_string())??;
                        self.mesh()?.cloud_fleetfiles_changed(local.clone()).await?;
                        mark_synced(&local, &path)?;
                    } else if local.is_dir() {
                        if self.journal.get(&path).is_none() {
                            self.mutate(|req| FileEvent::Mkdir {
                                req,
                                path: path.clone(),
                            })
                            .await?;
                        }
                        mark_synced(&local, &path)?;
                    } else {
                        self.upload(&local, &path).await?;
                    }
                }
                Mutation::Rename(from, to) => {
                    if self.route.is_some() {
                        if self.journal.get(&from).is_some() {
                            if self.source_entry(&from).await?.is_some() {
                                self.check_source_version(&from).await?;
                                self.check_source_version(&to).await?;
                                self.mutate(|req| FileEvent::Rename {
                                    req,
                                    from: from.clone(),
                                    to: to.clone(),
                                })
                                .await?;
                            } else {
                                let expected =
                                    self.journal.get(&from).and_then(|entry| entry.native_id);
                                let actual = self
                                    .source_entry(&to)
                                    .await?
                                    .and_then(|entry| entry.native_id);
                                if expected.is_none() || expected != actual {
                                    return Err("the source rename could not be verified; local files remain pending".into());
                                }
                            }
                            self.journal.rename(&from, &to)?;
                        }
                        let local = self.path(&to)?;
                        if local.exists() && !is_in_sync(&local) {
                            self.journal.enqueue(&Mutation::Upload(to))?;
                        }
                        self.recover_local_changes()?;
                    } else {
                        self.mesh()?
                            .cloud_fleetfiles_namespace(from.clone(), Some(to.clone()))
                            .await?;
                        self.journal.rename(&from, &to)?;
                        self.recover_local_changes()?;
                    }
                }
                Mutation::Delete(path) => {
                    if self.route.is_some() {
                        if self.path(&path)?.exists() {
                            self.journal.acknowledge(id)?;
                            continue;
                        }
                        if self.source_entry(&path).await?.is_some() {
                            self.check_source_version(&path).await?;
                            self.mutate(|req| FileEvent::Delete {
                                req,
                                path: path.clone(),
                            })
                            .await?;
                        }
                        self.journal.forget(&path)?;
                    } else if !self.path(&path)?.exists() {
                        self.mesh()?
                            .cloud_fleetfiles_namespace(path.clone(), None)
                            .await?;
                        self.journal.forget(&path)?;
                    }
                }
            }
            self.journal.acknowledge(id)?;
        }
        Ok(())
    }
    async fn upload(&self, local: &Path, path: &str) -> Result<(), String> {
        let hydrate_path = local.to_path_buf();
        tokio::task::spawn_blocking(move || hydrate_local(&hydrate_path))
            .await
            .map_err(|error| error.to_string())??;
        let mut file = OpenOptions::new()
            .read(true)
            .access_mode(FILE_GENERIC_READ | WRITE_DAC)
            .share_mode(FILE_SHARE_READ)
            .open(local)
            .map_err(|error| error.to_string())?;
        let before = file.metadata().map_err(|error| error.to_string())?;
        use sha2::{Digest, Sha256};
        let stage_id = format!(
            "{:x}",
            Sha256::digest(format!("{}\0{path}", self.root.display()).as_bytes())
        );
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let stage = child_path(parent, &format!(".allmystuff-upload-{stage_id}.part"))?;
        let backup = child_path(parent, &format!(".allmystuff-upload-{stage_id}.previous"))?;
        // Recover a rename that committed just before its acknowledgement was
        // lost. Native source identity proves ownership of that exact body.
        if let Some((receipt, bytes, modified)) = self.journal.receipt(path)? {
            if let Some(actual) = self.source_entry(path).await? {
                if receipt.native_id.is_some()
                    && receipt.native_id == actual.native_id
                    && receipt.size == actual.size
                    && receipt.modified == actual.modified
                {
                    self.remove_upload_backup(path, &backup).await?;
                    self.journal.record(path, &actual)?;
                    self.journal.clear_receipt(path)?;
                    if bytes == before.len() && modified == before.last_write_time() {
                        mark_synced_handle(&file, path)?;
                        return Ok(());
                    }
                }
            }
        }
        self.upload_source_version(path, &backup).await?;
        // Interrupted streams never truncate the current remote body. The
        // existing protocol refuses overwrite-renames, so replacements retain
        // the previous body in a verified sibling until publication succeeds.
        self.mesh()?
            .cloud_upload_file(
                self.route
                    .as_deref()
                    .ok_or("a shared drive route is required")?,
                &stage,
                &mut file,
            )
            .await?;
        let staged = self
            .source_entry(&stage)
            .await?
            .ok_or("the source lost the completed staging body")?;
        if staged.size != before.len() {
            return Err("the source staging length differs from the uploaded body".into());
        }
        self.journal
            .save_receipt(path, &staged, before.len(), before.last_write_time())?;
        self.upload_source_version(path, &backup).await?;
        if self.source_entry(path).await?.is_some() {
            if self.source_entry(&backup).await?.is_some() {
                return Err(
                    "a previous source backup already exists; both bodies were preserved".into(),
                );
            }
            self.mutate(|req| FileEvent::Rename {
                req,
                from: path.to_string(),
                to: backup.clone(),
            })
            .await?;
        }
        let published = self
            .mutate(|req| FileEvent::Rename {
                req,
                from: stage,
                to: path.to_string(),
            })
            .await;
        if let Err(error) = published {
            // Restore the original name when the route is still available.
            // Otherwise the receipt and verified backup resume on reconnect.
            if matches!(self.source_entry(path).await, Ok(None))
                && self
                    .source_entry(&backup)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|actual| {
                        self.journal
                            .get(path)
                            .is_some_and(|expected| same_source_body(&expected, &actual))
                    })
            {
                let _ = self
                    .mutate(|req| FileEvent::Rename {
                        req,
                        from: backup,
                        to: path.to_string(),
                    })
                    .await;
            }
            return Err(error);
        }
        self.remove_upload_backup(path, &backup).await?;
        self.journal.record(path, &staged)?;
        self.journal.clear_receipt(path)?;
        let after = file.metadata().map_err(|error| error.to_string())?;
        if before.len() != after.len() || before.last_write_time() != after.last_write_time() {
            return Err("the local file changed before upload acknowledgement".into());
        }
        mark_synced_handle(&file, path)?;
        Ok(())
    }
}

fn same_source_body(expected: &FileEntry, actual: &FileEntry) -> bool {
    expected.native_id.is_some()
        && expected.native_id == actual.native_id
        && expected.dir == actual.dir
        && expected.size == actual.size
        && expected.modified == actual.modified
}

fn hydrate_local(path: &Path) -> Result<(), String> {
    if !is_placeholder(path) {
        return Ok(());
    }
    let file = attr_file(path).map_err(|error| error.to_string())?;
    check(
        unsafe {
            CfHydratePlaceholder(
                file.as_raw_handle() as _,
                0,
                -1,
                CF_HYDRATE_FLAG_NONE,
                std::ptr::null_mut(),
            )
        },
        "hydrate before upload",
    )
}

fn update_placeholder(path: &Path, entry: &FileEntry, identity: &str) -> Result<(), String> {
    let file = attr_file(path).map_err(|error| error.to_string())?;
    let metadata = metadata(entry)?;
    check(
        unsafe {
            CfUpdatePlaceholder(
                file.as_raw_handle() as _,
                &metadata,
                identity.as_ptr().cast(),
                identity.len() as u32,
                std::ptr::null(),
                0,
                CF_UPDATE_FLAG_VERIFY_IN_SYNC
                    | CF_UPDATE_FLAG_MARK_IN_SYNC
                    | CF_UPDATE_FLAG_DEHYDRATE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        "refresh remote placeholder",
    )
}

unsafe fn callback_path(provider: &Provider, info: &CF_CALLBACK_INFO) -> Result<String, String> {
    unsafe fn text(pointer: *const u16) -> String {
        if pointer.is_null() {
            return String::new();
        }
        let mut length = 0;
        while length < 32768 && *pointer.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length))
    }
    let normalized = text(info.NormalizedPath);
    let full = if normalized.get(1..2) == Some(":") {
        normalized
    } else {
        format!("{}{}", text(info.VolumeDosName), normalized)
    };
    let full = full.strip_prefix(r"\\?\").unwrap_or(&full);
    if let Ok(relative) = provider.relative(Path::new(full)) {
        return Ok(relative);
    }
    // DOS-device aliases may be reported using their drive letter rather
    // than the registered physical root. Kernel-supplied placeholder identity
    // remains relative to this connection and must pass the same path checks.
    if info.FileId == info.SyncRootFileId {
        return Ok(String::new());
    }
    if !info.FileIdentity.is_null()
        && info.FileIdentityLength > 0
        && info.FileIdentityLength <= CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH
    {
        let bytes = std::slice::from_raw_parts(
            info.FileIdentity.cast::<u8>(),
            info.FileIdentityLength as usize,
        );
        return relative_path(
            std::str::from_utf8(bytes).map_err(|_| "invalid native placeholder identity")?,
        );
    }
    Err(format!(
        "Windows returned an unrecognized native path: {full}"
    ))
}

unsafe fn callback_provider(info: *const CF_CALLBACK_INFO) -> Arc<Provider> {
    let pointer = (*info).CallbackContext.cast::<Provider>();
    // CfDisconnectSyncRoot waits for active callbacks before the owning Arc
    // is dropped. Every dispatched operation takes its own counted reference.
    Arc::increment_strong_count(pointer);
    Arc::from_raw(pointer)
}

unsafe extern "system" fn fetch_data(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let provider = callback_provider(info);
    let info = &*info;
    let request = Request {
        connection: info.ConnectionKey,
        transfer: info.TransferKey,
        request: info.RequestKey,
    };
    let data = (*params).Anonymous.FetchData;
    let offset = data.RequiredFileOffset.max(0) as u64;
    let size = info.FileSize.max(0) as u64;
    let length = if data.RequiredLength < 0 {
        size.saturating_sub(offset)
    } else {
        data.RequiredLength as u64
    };
    let path = callback_path(&provider, info);
    #[cfg(debug_assertions)]
    if size > 4 * 1024 * 1024 * 1024 {
        tracing::info!(root=%provider.root.display(),route=?provider.route,path=?path,offset,length,size,"native large-file hydration requested");
    }
    if let Err(error) = path.and_then(|path| {
        provider
            .tx
            .try_send(Work::Fetch(request, path, offset, length, size))
            .map_err(|error| error.to_string())
    }) {
        tracing::warn!(%error,"native Windows hydration could not be dispatched");
        let _ = request.data(offset, &[], length, STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE);
    }
}

unsafe extern "system" fn fetch_placeholders(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let provider = callback_provider(info);
    let info = &*info;
    let request = Request {
        connection: info.ConnectionKey,
        transfer: info.TransferKey,
        request: info.RequestKey,
    };
    #[cfg(debug_assertions)]
    tracing::debug!(root=%provider.root.display(),path=?callback_path(&provider,info),"native directory population requested");
    if let Err(error) = callback_path(&provider, info).and_then(|path| {
        provider
            .tx
            .try_send(Work::Populate(request, path))
            .map_err(|error| error.to_string())
    }) {
        tracing::warn!(%error,"native Windows population could not be dispatched");
        let _ = request.placeholders(&mut [], 0, STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE, true);
    }
}

unsafe extern "system" fn cancel_data(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let provider = callback_provider(info);
    provider
        .cancelled
        .lock()
        .insert(((*info).TransferKey, (*info).RequestKey));
}

pub(crate) struct CloudConnection {
    provider: Arc<Provider>,
    watcher: Option<notify::RecommendedWatcher>,
    worker: tokio::task::JoinHandle<()>,
    uploads: tokio::task::JoinHandle<()>,
}

impl CloudConnection {
    pub(crate) async fn connect(
        mesh: Arc<Mesh>,
        root: PathBuf,
        route: Option<String>,
        account: String,
        label: String,
    ) -> Result<Self, String> {
        let register_root = root.clone();
        tokio::task::spawn_blocking(move || {
            crate::windows_fleetfiles::register_root(register_root, account, label)
        })
        .await
        .map_err(|error| error.to_string())??;
        let state = allmystuff_protocol::myownmesh_state_dir()
            .ok_or("AllMyStuff state is unavailable")?
            .join("cloud-provider");
        std::fs::create_dir_all(&state).map_err(|error| error.to_string())?;
        use sha2::{Digest, Sha256};
        let identity = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
        let (tx, mut rx) = mpsc::channel(WORK_QUEUE);
        let provider = Arc::new(Provider {
            mesh: Arc::downgrade(&mesh),
            route,
            root,
            journal: Journal::open(&state.join(format!("{identity}.sqlite3")))?,
            runtime: tokio::runtime::Handle::current(),
            connection: AtomicI64::new(0),
            stopped: AtomicBool::new(false),
            tx,
            watches: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(BTreeSet::new()),
            populations: Mutex::new(HashMap::new()),
        });
        let table = [
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_FETCH_DATA,
                Callback: Some(fetch_data),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
                Callback: Some(cancel_data),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
                Callback: Some(fetch_placeholders),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NONE,
                Callback: None,
            },
        ];
        let mut key = 0;
        check(
            unsafe {
                CfConnectSyncRoot(
                    wide(&provider.root).as_ptr(),
                    table.as_ptr(),
                    Arc::as_ptr(&provider).cast(),
                    CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH
                        | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION,
                    &mut key,
                )
            },
            "connect native Windows provider",
        )?;
        provider.connection.store(key, Ordering::Release);
        let weak = Arc::downgrade(&provider);
        let (upload_tx, mut upload_rx) = mpsc::channel::<()>(1);
        let worker_upload = upload_tx.clone();
        let worker = tokio::spawn(async move {
            let gate = Arc::new(tokio::sync::Semaphore::new(4));
            while let Some(work) = rx.recv().await {
                let Some(provider) = weak.upgrade() else {
                    break;
                };
                if matches!(work, Work::Changed) {
                    let _ = worker_upload.try_send(());
                    continue;
                }
                let Ok(permit) = gate.clone().acquire_owned().await else {
                    break;
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = match work {
                        Work::Fetch(request, path, offset, length, size) => {
                            let result = provider.fetch(request, &path, offset, length, size).await;
                            if result.is_err() {
                                let _ = request.data(
                                    offset,
                                    &[],
                                    length,
                                    STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
                                );
                            }
                            provider
                                .cancelled
                                .lock()
                                .remove(&(request.transfer, request.request));
                            result
                        }
                        Work::Populate(request, path) => {
                            let result = provider.populate(Some(request), &path).await;
                            if result.is_err() {
                                let _ = request.placeholders(
                                    &mut [],
                                    0,
                                    STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
                                    true,
                                );
                            }
                            result
                        }
                        Work::Refresh(path) => provider.populate(None, &path).await,
                        Work::RefreshAll => {
                            let mut result = Ok(());
                            for path in provider.journal.directories().unwrap_or_default() {
                                if let Err(error) = provider.populate(None, &path).await {
                                    result = Err(error);
                                    break;
                                }
                            }
                            result
                        }
                        Work::Changed => Ok(()),
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error,"native Windows file operation failed");
                    }
                });
            }
        });
        let weak = Arc::downgrade(&provider);
        let uploads = tokio::spawn(async move {
            let _ = upload_tx.try_send(());
            if let Some(provider) = weak.upgrade() {
                if let Err(error) = provider.recover_local_changes() {
                    tracing::warn!(%error,"native Windows recovery scan failed");
                }
            }
            let mut retry = tokio::time::interval(Duration::from_secs(30));
            retry.tick().await;
            loop {
                tokio::select! {
                    event = upload_rx.recv() => if event.is_none() { break; },
                    _ = retry.tick() => {
                        if let Some(provider) = weak.upgrade() {
                            let _ = provider.tx.try_send(Work::RefreshAll);
                        }
                    },
                }
                // Coalesce native file notifications until applications close.
                tokio::time::sleep(Duration::from_millis(750)).await;
                let Some(provider) = weak.upgrade() else {
                    break;
                };
                for attempt in 0..6 {
                    match provider.drain().await {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::warn!(%error,"Windows upload remains in the durable pending queue");
                            // A file can remain open after a directory-change
                            // notification. Retry this pending work with bounded
                            // backoff; reconnect recovers any remaining queue.
                            if attempt < 5 {
                                tokio::time::sleep(Duration::from_millis(250_u64 << attempt)).await;
                            }
                        }
                    }
                }
            }
        });
        let mut connection = Self {
            provider,
            watcher: None,
            worker,
            uploads,
        };
        let callback = Arc::downgrade(&connection.provider);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Some(provider) = callback.upgrade() {
                    match event {
                        Ok(event) => provider.changed(event),
                        Err(error) => {
                            tracing::warn!(%error,"Windows sync-root watcher needs reconciliation")
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        watcher
            .watch(&connection.provider.root, RecursiveMode::Recursive)
            .map_err(|error| error.to_string())?;
        connection.watcher = Some(watcher);
        // Fleetfiles already has real local directories. Enable their native
        // population before reporting the provider ready, rather than leaving
        // the first open racing a background conversion of Desktop.
        if connection.provider.route.is_none() {
            connection.provider.populate(None, "").await?;
        }
        connection.refresh();
        Ok(connection)
    }
    pub(crate) fn refresh(&self) {
        let _ = self.provider.tx.try_send(Work::RefreshAll);
    }
}

impl Drop for CloudConnection {
    fn drop(&mut self) {
        self.provider.stopped.store(true, Ordering::Release);
        self.watcher.take();
        unsafe {
            CfDisconnectSyncRoot(self.provider.connection.load(Ordering::Acquire));
        }
        self.worker.abort();
        self.uploads.abort();
        for (_, task) in self.provider.watches.lock().drain() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "uses the real Windows Cloud Files filter and Explorer registration"]
    async fn native_fleetfiles_populates_and_hydrates_its_own_namespace() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("allmystuff_node=debug")
            .try_init();
        use crate::fleetfiles::{FleetfilesMetadata, VersionStamp};
        use sha2::{Digest, Sha256};
        struct Sink;
        impl crate::UiSink for Sink {
            fn emit(&self, _: &str, _: serde_json::Value) {}
            fn restart(&self) -> ! {
                panic!("test node must not restart")
            }
        }
        let mesh = Mesh::new(
            Arc::new(crate::control_client::ControlClient::new().unwrap()),
            Arc::new(Sink),
        );
        let replica = mesh.cloud_test_fleetfiles_replica();
        let version = VersionStamp {
            counter: 1,
            actor: "test-source".into(),
        };
        replica.apply_directory(version.clone(), "Desktop").unwrap();
        let desktop = PathBuf::from(replica.materialize("Desktop").unwrap().unwrap());
        let root = desktop.parent().unwrap().to_path_buf();
        let journal = allmystuff_protocol::myownmesh_state_dir()
            .unwrap()
            .join("cloud-provider")
            .join(format!(
                "{:x}.sqlite3",
                Sha256::digest(root.to_string_lossy().as_bytes())
            ));
        let account = format!("Test-Fleetfiles-{}", std::process::id());
        struct Cleanup {
            connection: Option<CloudConnection>,
            root: PathBuf,
            account: String,
            journal: PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.connection.take();
                let _ = crate::windows_fleetfiles::unregister_test_root(self.account.clone());
                let expected_prefix =
                    format!("allmystuff-fleetfiles-mesh-test-{}-", std::process::id());
                if let (Ok(root), Ok(temp)) = (
                    std::fs::canonicalize(&self.root),
                    std::fs::canonicalize(std::env::temp_dir()),
                ) {
                    if root.parent() == Some(temp.as_path())
                        && root.file_name() == self.root.file_name()
                        && root.file_name().is_some_and(|name| {
                            name.to_string_lossy().starts_with(&expected_prefix)
                        })
                    {
                        let _ = std::fs::remove_dir_all(root);
                    }
                }
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", self.journal.display()));
                }
            }
        }
        let mut cleanup = Cleanup {
            connection: None,
            root: root.clone(),
            account: account.clone(),
            journal,
        };
        let text = b"Fleetfiles native hydration works";
        let hash = format!("{:x}", Sha256::digest(text));
        replica
            .apply_metadata(&FleetfilesMetadata {
                operation: "native-hydration".into(),
                version: version.clone(),
                path: "Desktop/report.txt".into(),
                kind: "file".into(),
                size: text.len() as u64,
                sha256: Some(hash.clone()),
                tombstone: false,
            })
            .unwrap();
        replica
            .begin_file(
                "source",
                "native-hydration".into(),
                version,
                "Desktop/report.txt".into(),
                (text.len() as u64, hash),
                true,
            )
            .unwrap();
        replica
            .write_chunk("source", "native-hydration", 0, text)
            .unwrap();
        replica.commit_file("source", "native-hydration").unwrap();
        assert!(!root.join("Desktop/report.txt").exists());
        cleanup.connection = Some(
            CloudConnection::connect(
                mesh.clone(),
                root.clone(),
                None,
                account,
                "AllMyStuff Fleetfiles native test".into(),
            )
            .await
            .unwrap(),
        );

        // Unlike the scoped-drive integration test, this provider lists the
        // very ledger and cloud namespace that Windows is currently populating.
        let listing_mesh = mesh.clone();
        let list = tokio::task::spawn_blocking(move || {
            listing_mesh.fleetfiles_logical_list("Desktop".into(), None, 10)
        });
        let page = tokio::time::timeout(Duration::from_secs(20), list)
            .await
            .expect("Fleetfiles listing re-entered its own provider")
            .unwrap()
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "report.txt");
        assert_external_text(
            &desktop.join("report.txt"),
            std::str::from_utf8(text).unwrap(),
        )
        .await;
        cleanup.connection.take();
        drop(replica);
        drop(cleanup);
        assert!(
            !root.exists(),
            "native Fleetfiles test data was not removed"
        );
    }

    async fn assert_external_text(path: &Path, expected: &str) {
        // Provider processes deliberately block their own implicit hydration.
        // Exercise the same ordinary application access used by Explorer.
        let path = path.to_string_lossy().replace('\'', "''");
        let expected = expected.replace('\'', "''");
        let script = format!("$ErrorActionPreference='Stop';if([IO.File]::ReadAllText('{path}') -ne '{expected}'){{throw 'Native hydrated text differs'}}");
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            crate::child_process::command("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "uses the real Windows Cloud Files filter and Explorer registration"]
    async fn native_filter_reads_beyond_four_gib() {
        use std::io::{Seek, SeekFrom, Write};
        struct Sink;
        impl crate::UiSink for Sink {
            fn emit(&self, _event: &str, _payload: serde_json::Value) {}
            fn restart(&self) -> ! {
                panic!("test node must not restart")
            }
        }
        let _ = tracing_subscriber::fmt()
            .with_env_filter("allmystuff_node=debug")
            .try_init();
        let base =
            std::env::temp_dir().join(format!("ams-native-filter-test-{}", std::process::id()));
        let source = base.join("source");
        let cache = base.join("cache");
        std::fs::create_dir_all(&source).unwrap();
        let nested = source.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(source.join("source-update.txt"), b"before").unwrap();
        for index in 0..520 {
            std::fs::write(source.join(format!("entry-{index:04}.txt")), b"metadata").unwrap();
        }
        let mut file = File::create(nested.join("large.bin")).unwrap();
        file.set_len(5 * 1024 * 1024 * 1024 + 31).unwrap();
        file.seek(SeekFrom::Start(4 * 1024 * 1024 * 1024 + 17))
            .unwrap();
        file.write_all(b"AMS-native-above-four-GiB").unwrap();
        drop(file);
        let client = Arc::new(crate::control_client::ControlClient::new().unwrap());
        let mesh = Mesh::new(client, Arc::new(Sink));
        mesh.cloud_test_source(source.clone());
        let account = format!("Test-{}", std::process::id());
        let connection = CloudConnection::connect(
            mesh.clone(),
            cache.clone(),
            Some(crate::drive_mount::FLEETFILES_ROUTE.to_string()),
            account.clone(),
            "AllMyStuff native test".into(),
        )
        .await
        .unwrap();
        let alias = crate::drive_mount::test_cloud_alias(&cache, None).unwrap();
        let path = PathBuf::from(format!("{alias}\\nested\\large.bin"))
            .to_string_lossy()
            .replace('\'', "''");
        let script=format!("$ErrorActionPreference='Stop';$f=[IO.File]::OpenRead('{path}');try{{$null=$f.Seek(4294967313,[IO.SeekOrigin]::Begin);$b=New-Object byte[] 25;$n=$f.Read($b,0,$b.Length);if([Text.Encoding]::ASCII.GetString($b)-ne 'AMS-native-above-four-GiB'){{throw 'Native large-file bytes differ'}};Write-Output ('NATIVE_SIZE '+$f.Length)}}finally{{$f.Dispose()}}");
        let output = tokio::time::timeout(
            Duration::from_secs(90),
            crate::child_process::command("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let write = cache
            .join("write-back.txt")
            .to_string_lossy()
            .replace('\'', "''");
        let script = format!("[IO.File]::WriteAllText('{write}','native write recovery');");
        let result = crate::child_process::command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        let mut copied = false;
        for _ in 0..100 {
            if std::fs::read_to_string(source.join("write-back.txt"))
                .is_ok_and(|value| value == "native write recovery")
            {
                copied = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(copied, "a native write was not published to the source");
        for _ in 0..100 {
            if is_in_sync(&cache.join("write-back.txt")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let script = format!("[IO.File]::WriteAllText('{write}','replacement native body');");
        let result = crate::child_process::command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        for _ in 0..100 {
            if std::fs::read_to_string(source.join("write-back.txt"))
                .is_ok_and(|value| value == "replacement native body")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            std::fs::read_to_string(source.join("write-back.txt")).unwrap(),
            "replacement native body"
        );
        for _ in 0..100 {
            if is_in_sync(&cache.join("write-back.txt")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let renamed = cache
            .join("renamed.txt")
            .to_string_lossy()
            .replace('\'', "''");
        let script = format!("[IO.File]::Move('{write}','{renamed}');");
        let result = crate::child_process::command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        for _ in 0..100 {
            if source.join("renamed.txt").is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            source.join("renamed.txt").is_file(),
            "native rename did not reach the source"
        );
        for _ in 0..100 {
            if is_in_sync(&cache.join("renamed.txt")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let renamed_handle = attr_file(&cache.join("renamed.txt")).unwrap();
        check(
            unsafe {
                CfDehydratePlaceholder(
                    renamed_handle.as_raw_handle() as _,
                    0,
                    -1,
                    CF_DEHYDRATE_FLAG_NONE,
                    std::ptr::null_mut(),
                )
            },
            "test renamed hydration",
        )
        .unwrap();
        drop(renamed_handle);
        assert_external_text(&cache.join("renamed.txt"), "replacement native body").await;
        assert_external_text(&cache.join("source-update.txt"), "before").await;
        std::fs::write(
            source.join("source-update.txt"),
            b"source changed after hydration",
        )
        .unwrap();
        connection.refresh();
        for _ in 0..100 {
            if connection
                .provider
                .journal
                .get("source-update.txt")
                .is_some_and(|entry| entry.size == 30)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_external_text(
            &cache.join("source-update.txt"),
            "source changed after hydration",
        )
        .await;
        let script = format!("[IO.File]::Delete('{renamed}');");
        let result = crate::child_process::command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        for _ in 0..100 {
            if !source.join("renamed.txt").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !source.join("renamed.txt").exists(),
            "native delete did not reach the source"
        );
        crate::drive_mount::test_cloud_alias(&cache, Some(&alias)).unwrap();
        drop(connection);
        let reconnected = CloudConnection::connect(
            mesh,
            cache,
            Some(crate::drive_mount::FLEETFILES_ROUTE.to_string()),
            account.clone(),
            "AllMyStuff native test".into(),
        )
        .await
        .unwrap();
        drop(reconnected);
        crate::windows_fleetfiles::unregister_test_root(account).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("NATIVE_SIZE 5368709151"));
    }

    #[test]
    fn cloud_sizes_and_offsets_cross_four_gib_without_narrowing() {
        let size = 5_u64 * 1024 * 1024 * 1024 + 31;
        let entry = FileEntry {
            name: "large.bin".into(),
            native_id: None,
            hidden: false,
            dir: false,
            size,
            modified: Some(1),
            symlink: false,
        };
        assert_eq!(metadata(&entry).unwrap().FileSize as u64, size);
        assert!(child_path("folder", "../escape").is_err());
        assert!(relative_path("C:\\escape").is_err());
        assert!(relative_path("safe/../../escape").is_err());
        assert!(relative_path("safe/.. /escape").is_err());
        assert!(child_path("safe", "file:stream").is_err());
    }

    #[test]
    fn pending_changes_survive_reconnect_until_acknowledged() {
        let path = std::env::temp_dir().join(format!(
            "ams-cloud-journal-{}-{}.sqlite3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let journal = Journal::open(&path).unwrap();
        journal
            .enqueue(&Mutation::Upload("large.bin".into()))
            .unwrap();
        drop(journal);
        let journal = Journal::open(&path).unwrap();
        let (id, mutation) = journal.pending().unwrap().unwrap();
        assert!(matches!(mutation,Mutation::Upload(path) if path=="large.bin"));
        journal.acknowledge(id).unwrap();
        assert!(journal.pending().unwrap().is_none());
        drop(journal);
        std::fs::remove_file(path).unwrap();
    }
}
