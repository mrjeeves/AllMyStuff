//! Transactional, bounded replication primitives for the canonical Fleetfiles root.
//!
//! The OS mount materializes one working replica. Changes are captured from
//! precise filesystem notifications, coalesced by path, streamed in bounded
//! chunks, verified, and only then atomically committed. The SQLite path table
//! is metadata, not a file index: it contains only managed Fleetfiles objects
//! and one current version/tombstone per logical path.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Raw bytes per transfer frame. Base64 expands by 4/3, so 40 KiB plus the
/// operation/channel JSON envelope remains below WebRTC's ~64 KiB message
/// ceiling. This matches the proven files-plane budget.
pub const TRANSFER_CHUNK: usize = 40 * 1024;
const CHANGE_QUEUE: usize = 4096;
const MAX_INBOUND: usize = 32;
const MAX_PATH_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionStamp {
    pub counter: u64,
    pub actor: String,
}

#[derive(Clone, Debug)]
pub struct LocalChange {
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub enum LocalMutation {
    File {
        operation: String,
        version: VersionStamp,
        path: String,
        size: u64,
        sha256: String,
        source: PathBuf,
    },
    Directory {
        operation: String,
        version: VersionStamp,
        path: String,
    },
    Delete {
        operation: String,
        version: VersionStamp,
        path: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetfilesMessage {
    FileBegin {
        operation: String,
        version: VersionStamp,
        path: String,
        size: u64,
        sha256: String,
    },
    FileChunk {
        operation: String,
        offset: u64,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
    },
    FileCommit {
        operation: String,
    },
    Directory {
        operation: String,
        version: VersionStamp,
        path: String,
    },
    Delete {
        operation: String,
        version: VersionStamp,
        path: String,
    },
    Ready {
        operation: String,
        accepted: bool,
        needs_content: bool,
        detail: Option<String>,
    },
    Committed {
        operation: String,
        accepted: bool,
        detail: Option<String>,
    },
}

struct Inbound {
    path: String,
    version: VersionStamp,
    size: u64,
    sha256: String,
    received: u64,
    staging: PathBuf,
    file: File,
}

pub struct FleetfilesReplica {
    root: PathBuf,
    connection: Mutex<Connection>,
    inbound: Mutex<HashMap<(String, String), Inbound>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    overflowed: Arc<AtomicBool>,
}

impl FleetfilesReplica {
    pub fn load(root: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&root);
        let database = allmystuff_protocol::myownmesh_state_dir()
            .map(|dir| dir.join("allmystuff-fleetfiles.sqlite3"));
        let connection = database
            .as_ref()
            .and_then(|path| {
                path.parent()
                    .and_then(|parent| std::fs::create_dir_all(parent).ok())?;
                Connection::open(path).ok()
            })
            .or_else(|| Connection::open_in_memory().ok())
            .expect("Fleetfiles metadata database must open");
        Self::with_connection(root, connection)
    }

    #[cfg(test)]
    fn memory(root: PathBuf) -> Self {
        Self::with_connection(root, Connection::open_in_memory().unwrap())
    }

    fn with_connection(root: PathBuf, connection: Connection) -> Self {
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 CREATE TABLE IF NOT EXISTS path_versions (
                   path TEXT PRIMARY KEY,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   sha256 TEXT,
                   tombstone INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS path_versions_stamp
                   ON path_versions(counter, actor);
                 CREATE TABLE IF NOT EXISTS replication_queue (
                   target TEXT NOT NULL,
                   path TEXT NOT NULL,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   sha256 TEXT,
                   queued_at INTEGER NOT NULL,
                   PRIMARY KEY(target, path)
                 );
                 CREATE INDEX IF NOT EXISTS replication_queue_target
                   ON replication_queue(target, queued_at, path);",
            )
            .expect("Fleetfiles metadata schema must initialize");
        let staging = root.join(".allmystuff-staging");
        let _ = std::fs::create_dir_all(&staging);
        let _ = crate::files::mark_internal_staging_hidden(&staging);
        Self {
            root,
            connection: Mutex::new(connection),
            inbound: Mutex::new(HashMap::new()),
            watcher: Mutex::new(None),
            overflowed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Install one policy-required watcher over the managed root. It never
    /// enumerates the tree and its queue is hard bounded. Overflow is surfaced
    /// for reconciliation instead of growing memory without limit.
    pub fn start_watcher(&self) -> Result<mpsc::Receiver<LocalChange>, String> {
        let (tx, rx) = mpsc::sync_channel(CHANGE_QUEUE);
        let overflowed = self.overflowed.clone();
        let root = self.root.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    overflowed.store(true, Ordering::Relaxed);
                    return;
                };
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for path in event.paths {
                    if is_internal(&root, &path) {
                        continue;
                    }
                    if tx.try_send(LocalChange { path }).is_err() {
                        overflowed.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            })
            .map_err(|error| format!("create Fleetfiles watcher: {error}"))?;
        watcher
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|error| format!("watch Fleetfiles root: {error}"))?;
        *self.watcher.lock() = Some(watcher);
        Ok(rx)
    }

    pub fn take_overflow(&self) -> bool {
        self.overflowed.swap(false, Ordering::Relaxed)
    }

    pub fn logical_used_bytes(&self) -> u64 {
        self.connection
            .lock()
            .query_row(
                "SELECT COALESCE(SUM(size), 0)
                 FROM path_versions
                 WHERE tombstone=0 AND kind='file'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .and_then(|bytes| u64::try_from(bytes).ok())
            .unwrap_or(0)
    }

    pub fn queue_for(&self, target: &str, mutation: &LocalMutation) -> Result<(), String> {
        if target.is_empty() || target.len() > 512 {
            return Err("invalid Fleetfiles replica target".into());
        }
        let (path, version, kind, size, sha256) = match mutation {
            LocalMutation::File {
                version,
                path,
                size,
                sha256,
                ..
            } => (path, version, "file", *size, Some(sha256.as_str())),
            LocalMutation::Directory { version, path, .. } => (path, version, "directory", 0, None),
            LocalMutation::Delete { version, path, .. } => (path, version, "delete", 0, None),
        };
        self.connection
            .lock()
            .execute(
                "INSERT INTO replication_queue(
                   target,path,counter,actor,kind,size,sha256,queued_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,unixepoch())
                 ON CONFLICT(target,path) DO UPDATE SET
                   counter=excluded.counter, actor=excluded.actor, kind=excluded.kind,
                   size=excluded.size, sha256=excluded.sha256, queued_at=excluded.queued_at",
                params![
                    target,
                    path,
                    version.counter,
                    version.actor,
                    kind,
                    size,
                    sha256
                ],
            )
            .map_err(|error| format!("queue Fleetfiles replica: {error}"))?;
        Ok(())
    }

    pub fn pending_for(&self, target: &str, limit: usize) -> Result<Vec<LocalMutation>, String> {
        let limit = limit.clamp(1, 128);
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT path,counter,actor,kind,size,sha256
                 FROM replication_queue
                 WHERE target=?1
                 ORDER BY queued_at,path
                 LIMIT ?2",
            )
            .map_err(|error| format!("prepare Fleetfiles replica queue: {error}"))?;
        let rows = statement
            .query_map(params![target, limit as u64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(|error| format!("read Fleetfiles replica queue: {error}"))?;
        let mut pending = Vec::new();
        for row in rows {
            let (path, counter, actor, kind, size, sha256) =
                row.map_err(|error| format!("decode Fleetfiles replica queue: {error}"))?;
            let version = VersionStamp { counter, actor };
            let operation = operation_id(&version, &path);
            pending.push(match kind.as_str() {
                "file" => LocalMutation::File {
                    operation,
                    version,
                    source: resolve_portable(&self.root, &path)?,
                    path,
                    size,
                    sha256: sha256.ok_or("queued Fleetfiles file has no content hash")?,
                },
                "directory" => LocalMutation::Directory {
                    operation,
                    version,
                    path,
                },
                "delete" => LocalMutation::Delete {
                    operation,
                    version,
                    path,
                },
                _ => return Err("queued Fleetfiles mutation has an invalid kind".into()),
            });
        }
        Ok(pending)
    }

    pub fn acknowledge(&self, target: &str, mutation: &LocalMutation) -> Result<(), String> {
        let (path, version) = match mutation {
            LocalMutation::File { path, version, .. }
            | LocalMutation::Directory { path, version, .. }
            | LocalMutation::Delete { path, version, .. } => (path, version),
        };
        self.connection
            .lock()
            .execute(
                "DELETE FROM replication_queue
                 WHERE target=?1 AND path=?2 AND counter=?3 AND actor=?4",
                params![target, path, version.counter, version.actor],
            )
            .map_err(|error| format!("acknowledge Fleetfiles replica: {error}"))?;
        Ok(())
    }

    pub fn capture(&self, path: &Path, actor: &str) -> Result<Option<LocalMutation>, String> {
        let relative = portable_relative(&self.root, path)?;
        if relative.is_empty() || relative.starts_with(".allmystuff-staging/") {
            return Ok(None);
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("inspect Fleetfiles change: {error}")),
        };
        if metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(format!(
                "Fleetfiles does not follow symbolic link {relative}"
            ));
        }

        let (kind, size, sha256, tombstone) = match metadata {
            None => ("delete", 0, None, true),
            Some(metadata) if metadata.is_dir() => ("directory", 0, None, false),
            Some(metadata) if metadata.is_file() => {
                let (size, hash) = hash_file(path)?;
                ("file", size, Some(hash), false)
            }
            Some(_) => return Ok(None),
        };

        let connection = self.connection.lock();
        let existing: Option<(u64, String, String, u64, Option<String>, bool)> = connection
            .query_row(
                "SELECT counter, actor, kind, size, sha256, tombstone
                 FROM path_versions WHERE path=?1",
                params![relative],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("read Fleetfiles version: {error}"))?;
        if existing
            .as_ref()
            .is_some_and(|(_, _, old_kind, old_size, old_hash, old_tombstone)| {
                old_kind == kind
                    && *old_size == size
                    && old_hash == &sha256
                    && *old_tombstone == tombstone
            })
        {
            return Ok(None);
        }
        let counter: u64 = connection
            .query_row(
                "SELECT COALESCE(MAX(counter), 0) + 1 FROM path_versions",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("advance Fleetfiles version: {error}"))?;
        let version = VersionStamp {
            counter,
            actor: actor.to_string(),
        };
        put_version(
            &connection,
            &relative,
            &version,
            kind,
            size,
            sha256.as_deref(),
            tombstone,
        )?;
        drop(connection);
        let operation = operation_id(&version, &relative);
        Ok(Some(match kind {
            "file" => LocalMutation::File {
                operation,
                version,
                path: relative,
                size,
                sha256: sha256.expect("file hash"),
                source: path.to_path_buf(),
            },
            "directory" => LocalMutation::Directory {
                operation,
                version,
                path: relative,
            },
            _ => LocalMutation::Delete {
                operation,
                version,
                path: relative,
            },
        }))
    }

    pub fn begin_file(
        &self,
        from: &str,
        operation: String,
        version: VersionStamp,
        path: String,
        size: u64,
        sha256: String,
    ) -> Result<bool, String> {
        validate_portable_path(&path)?;
        validate_hash(&sha256)?;
        if !self.should_apply(&path, &version)? {
            return Ok(false);
        }
        let mut inbound = self.inbound.lock();
        if inbound.len() >= MAX_INBOUND
            && !inbound.contains_key(&(from.to_string(), operation.clone()))
        {
            return Err("too many concurrent Fleetfiles transfers".into());
        }
        let staging = self
            .root
            .join(".allmystuff-staging")
            .join(safe_operation_name(&operation));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&staging)
            .map_err(|error| format!("create Fleetfiles staging file: {error}"))?;
        inbound.insert(
            (from.to_string(), operation),
            Inbound {
                path,
                version,
                size,
                sha256,
                received: 0,
                staging,
                file,
            },
        );
        Ok(true)
    }

    pub fn write_chunk(
        &self,
        from: &str,
        operation: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), String> {
        if data.len() > TRANSFER_CHUNK {
            return Err("Fleetfiles chunk exceeds the bounded transfer size".into());
        }
        let mut inbound = self.inbound.lock();
        let transfer = inbound
            .get_mut(&(from.to_string(), operation.to_string()))
            .ok_or("unknown Fleetfiles transfer")?;
        if offset != transfer.received {
            return Err(format!(
                "Fleetfiles chunk offset mismatch: expected {}, got {offset}",
                transfer.received
            ));
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or("Fleetfiles size overflow")?;
        if end > transfer.size {
            return Err("Fleetfiles chunk exceeds declared file size".into());
        }
        transfer
            .file
            .write_all(data)
            .map_err(|error| format!("write Fleetfiles chunk: {error}"))?;
        transfer.received = end;
        Ok(())
    }

    pub fn commit_file(&self, from: &str, operation: &str) -> Result<String, String> {
        let key = (from.to_string(), operation.to_string());
        let mut transfer = self
            .inbound
            .lock()
            .remove(&key)
            .ok_or("unknown Fleetfiles transfer")?;
        transfer
            .file
            .flush()
            .map_err(|error| format!("flush Fleetfiles staging: {error}"))?;
        transfer
            .file
            .sync_all()
            .map_err(|error| format!("sync Fleetfiles staging: {error}"))?;
        if transfer.received != transfer.size {
            let _ = std::fs::remove_file(&transfer.staging);
            return Err(format!(
                "incomplete Fleetfiles transfer: {} of {} bytes",
                transfer.received, transfer.size
            ));
        }
        let (size, actual) = hash_file(&transfer.staging)?;
        if size != transfer.size || actual != transfer.sha256 {
            let _ = std::fs::remove_file(&transfer.staging);
            return Err("Fleetfiles verification failed".into());
        }
        if !self.should_apply(&transfer.path, &transfer.version)? {
            let _ = std::fs::remove_file(&transfer.staging);
            return Ok(transfer.path);
        }
        let target = resolve_portable(&self.root, &transfer.path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create Fleetfiles parent: {error}"))?;
        }
        replace_file(&transfer.staging, &target)?;
        put_version(
            &self.connection.lock(),
            &transfer.path,
            &transfer.version,
            "file",
            transfer.size,
            Some(&transfer.sha256),
            false,
        )?;
        Ok(transfer.path)
    }

    pub fn apply_directory(&self, version: VersionStamp, path: &str) -> Result<bool, String> {
        validate_portable_path(path)?;
        if !self.should_apply(path, &version)? {
            return Ok(false);
        }
        let target = resolve_portable(&self.root, path)?;
        std::fs::create_dir_all(&target)
            .map_err(|error| format!("create replicated directory: {error}"))?;
        put_version(
            &self.connection.lock(),
            path,
            &version,
            "directory",
            0,
            None,
            false,
        )?;
        Ok(true)
    }

    pub fn apply_delete(&self, version: VersionStamp, path: &str) -> Result<bool, String> {
        validate_portable_path(path)?;
        if !self.should_apply(path, &version)? {
            return Ok(false);
        }
        let target = resolve_portable(&self.root, path)?;
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&target),
            Ok(_) => std::fs::remove_file(&target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
        .map_err(|error| format!("delete replicated Fleetfiles path: {error}"))?;
        put_version(
            &self.connection.lock(),
            path,
            &version,
            "delete",
            0,
            None,
            true,
        )?;
        Ok(true)
    }

    fn should_apply(&self, path: &str, incoming: &VersionStamp) -> Result<bool, String> {
        let current: Option<(u64, String)> = self
            .connection
            .lock()
            .query_row(
                "SELECT counter, actor FROM path_versions WHERE path=?1",
                params![path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("read Fleetfiles conflict version: {error}"))?;
        Ok(current.is_none_or(|(counter, actor)| incoming > &VersionStamp { counter, actor }))
    }
}

fn put_version(
    connection: &Connection,
    path: &str,
    version: &VersionStamp,
    kind: &str,
    size: u64,
    sha256: Option<&str>,
    tombstone: bool,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO path_versions(path,counter,actor,kind,size,sha256,tombstone,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,unixepoch())
             ON CONFLICT(path) DO UPDATE SET counter=excluded.counter, actor=excluded.actor,
               kind=excluded.kind, size=excluded.size, sha256=excluded.sha256,
               tombstone=excluded.tombstone, updated_at=excluded.updated_at",
            params![
                path,
                version.counter,
                version.actor,
                kind,
                size,
                sha256,
                tombstone
            ],
        )
        .map_err(|error| format!("commit Fleetfiles version: {error}"))?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, String), String> {
    let mut file = File::open(path).map_err(|error| format!("open Fleetfiles content: {error}"))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("hash Fleetfiles content: {error}"))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hash.update(&buffer[..read]);
    }
    Ok((size, format!("{:x}", hash.finalize())))
}

fn operation_id(version: &VersionStamp, path: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(version.actor.as_bytes());
    hash.update(version.counter.to_le_bytes());
    hash.update(path.as_bytes());
    format!(
        "{}-{}-{:x}",
        version.actor,
        version.counter,
        hash.finalize()
    )
}

fn safe_operation_name(operation: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(operation.as_bytes());
    format!("{:x}.part", hash.finalize())
}

fn validate_hash(hash: &str) -> Result<(), String> {
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("invalid Fleetfiles content hash".into())
    }
}

fn portable_relative(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Fleetfiles change escaped its managed root")?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("Fleetfiles path contains a non-portable component".into());
        };
        parts.push(part.to_string_lossy().into_owned());
    }
    let path = parts.join("/");
    if !path.is_empty() {
        validate_portable_path(&path)?;
    }
    Ok(path)
}

fn validate_portable_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\0')
    {
        return Err("invalid Fleetfiles path".into());
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err("invalid Fleetfiles path component".into());
        }
        if component.ends_with([' ', '.'])
            || component
                .chars()
                .any(|c| c.is_control() || "<>:\"\\|?*".contains(c))
        {
            return Err(format!(
                "the name {component:?} is not portable across fleet computers"
            ));
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        ) {
            return Err(format!("the name {component:?} is reserved on Windows"));
        }
    }
    Ok(())
}

fn resolve_portable(root: &Path, path: &str) -> Result<PathBuf, String> {
    validate_portable_path(path)?;
    let mut resolved = root.to_path_buf();
    for component in path.split('/') {
        resolved.push(component);
    }
    Ok(resolved)
}

fn is_internal(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == ".allmystuff-staging"
            || name == ".DS_Store"
            || name == ".Spotlight-V100"
            || name == ".Trashes"
            || name == ".fseventsd"
            || name == ".TemporaryItems"
            || name == ".DocumentRevisions-V100"
            || name.starts_with("._")
    })
}

fn replace_file(staging: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let mut from: Vec<u16> = staging.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        let ok = unsafe {
            MoveFileExW(
                from.as_mut_ptr(),
                to.as_mut_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(format!(
                "commit Fleetfiles file: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(staging, target).map_err(|error| format!("commit Fleetfiles file: {error}"))
    }
}

pub struct ChunkReader {
    file: File,
    offset: u64,
}

impl ChunkReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        Ok(Self {
            file: File::open(path).map_err(|error| format!("open Fleetfiles transfer: {error}"))?,
            offset: 0,
        })
    }

    pub fn next_chunk(&mut self) -> Result<Option<(u64, Vec<u8>)>, String> {
        let mut data = vec![0_u8; TRANSFER_CHUNK];
        let read = self
            .file
            .read(&mut data)
            .map_err(|error| format!("read Fleetfiles transfer: {error}"))?;
        if read == 0 {
            return Ok(None);
        }
        data.truncate(read);
        let offset = self.offset;
        self.offset = self.offset.saturating_add(read as u64);
        Ok(Some((offset, data)))
    }
}

mod bytes_b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_names_that_collide_across_operating_systems() {
        assert!(validate_portable_path("good/fleet file.txt").is_ok());
        assert!(validate_portable_path("../escape").is_err());
        assert!(validate_portable_path("bad:name").is_err());
        assert!(validate_portable_path("CON.txt").is_err());
        assert!(validate_portable_path("trailing.").is_err());
    }

    #[test]
    fn ignores_replication_staging_and_macos_bookkeeping_only() {
        let root = PathBuf::from("fleetfiles-root");
        assert!(is_internal(&root, &root.join(".allmystuff-staging/chunk")));
        assert!(is_internal(&root, &root.join("photos/.DS_Store")));
        assert!(is_internal(&root, &root.join("photos/._image.png")));
        assert!(is_internal(&root, &root.join(".Spotlight-V100/index")));
        assert!(!is_internal(&root, &root.join(".env")));
        assert!(!is_internal(&root, &root.join("photos/image.png")));
    }

    #[test]
    fn transfer_chunk_stays_below_the_data_channel_envelope_budget() {
        let encoded = serde_json::to_vec(&FleetfilesMessage::FileChunk {
            operation: "f".repeat(80),
            offset: u64::MAX,
            data: vec![0xff; TRANSFER_CHUNK],
        })
        .unwrap();
        assert!(
            encoded.len() < 56 * 1024,
            "Fleetfiles chunk JSON was {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn verified_commit_wins_and_stale_version_does_not() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        let v1 = VersionStamp {
            counter: 1,
            actor: "a".into(),
        };
        assert!(replica
            .begin_file(
                "peer",
                "one".into(),
                v1.clone(),
                "hello.txt".into(),
                5,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into()
            )
            .unwrap());
        replica.write_chunk("peer", "one", 0, b"hello").unwrap();
        replica.commit_file("peer", "one").unwrap();
        assert_eq!(std::fs::read(root.join("hello.txt")).unwrap(), b"hello");
        assert!(!replica
            .begin_file(
                "peer",
                "stale".into(),
                v1,
                "hello.txt".into(),
                0,
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into()
            )
            .unwrap());
        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn durable_queue_is_latest_wins_and_acknowledges_exact_versions() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-queue-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        let older = LocalMutation::Directory {
            operation: "older".into(),
            version: VersionStamp {
                counter: 1,
                actor: "local".into(),
            },
            path: "project".into(),
        };
        let newer = LocalMutation::Delete {
            operation: "newer".into(),
            version: VersionStamp {
                counter: 2,
                actor: "local".into(),
            },
            path: "project".into(),
        };

        replica.queue_for("peer", &older).unwrap();
        replica.queue_for("peer", &newer).unwrap();
        let pending = replica.pending_for("peer", 32).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            &pending[0],
            LocalMutation::Delete {
                version: VersionStamp { counter: 2, .. },
                ..
            }
        ));

        replica.acknowledge("peer", &older).unwrap();
        assert_eq!(replica.pending_for("peer", 32).unwrap().len(), 1);
        replica.acknowledge("peer", &newer).unwrap();
        assert!(replica.pending_for("peer", 32).unwrap().is_empty());

        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }
}
