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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const BODY_CACHE_ENTRIES: usize = 128;
const BODY_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const BODY_CACHE_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
const LEDGER_PAGE_BYTES: usize = 32 * 1024;
static CONTENT_STAGE_ID: AtomicU64 = AtomicU64::new(1);

/// One enabled local allocation available to the Fleetfiles immutable-content
/// store. `quota_bytes` is already reduced by the fleet reserve policy; the
/// user-visible sync root is deliberately not an allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageAllocationRoot {
    pub id: String,
    pub root: PathBuf,
    pub quota_bytes: u64,
}

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetfilesMetadata {
    pub operation: String,
    pub version: VersionStamp,
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub tombstone: bool,
}

impl FleetfilesMetadata {
    pub fn from_mutation(mutation: &LocalMutation) -> Self {
        match mutation {
            LocalMutation::File {
                operation,
                version,
                path,
                size,
                sha256,
                ..
            } => Self {
                operation: operation.clone(),
                version: version.clone(),
                path: path.clone(),
                kind: "file".into(),
                size: *size,
                sha256: Some(sha256.clone()),
                tombstone: false,
            },
            LocalMutation::Directory {
                operation,
                version,
                path,
            } => Self {
                operation: operation.clone(),
                version: version.clone(),
                path: path.clone(),
                kind: "directory".into(),
                size: 0,
                sha256: None,
                tombstone: false,
            },
            LocalMutation::Delete {
                operation,
                version,
                path,
            } => Self {
                operation: operation.clone(),
                version: version.clone(),
                path: path.clone(),
                kind: "delete".into(),
                size: 0,
                sha256: None,
                tombstone: true,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogicalEntry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub modified: i64,
    pub version: VersionStamp,
    pub materialized: bool,
    pub content_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogicalDirectoryPage {
    pub parent: String,
    pub entries: Vec<LogicalEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogicalSearchPage {
    pub query: String,
    pub entries: Vec<LogicalEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionHistoryEntry {
    pub path: String,
    pub version: VersionStamp,
    pub kind: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub tombstone: bool,
    pub recorded_at: i64,
    pub content_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionHistoryPage {
    pub path: String,
    pub entries: Vec<VersionHistoryEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetfilesLedgerDigest {
    pub entries: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetfilesLedgerCursor {
    pub path: String,
    pub counter: u64,
    pub actor: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetfilesLedgerPage {
    pub entries: Vec<FleetfilesMetadata>,
    pub next_cursor: Option<FleetfilesLedgerCursor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetfilesMessage {
    Metadata {
        metadata: FleetfilesMetadata,
    },
    LedgerProbe {
        operation: String,
        digest: FleetfilesLedgerDigest,
    },
    LedgerStatus {
        operation: String,
        digest: FleetfilesLedgerDigest,
    },
    LedgerPageRequest {
        operation: String,
        after: Option<FleetfilesLedgerCursor>,
        limit: u16,
    },
    LedgerPage {
        operation: String,
        entries: Vec<FleetfilesMetadata>,
        next_cursor: Option<FleetfilesLedgerCursor>,
    },
    LedgerApply {
        operation: String,
        entries: Vec<FleetfilesMetadata>,
    },
    BodyRequest {
        operation: String,
        path: String,
        version: VersionStamp,
        size: u64,
        sha256: String,
    },
    FileBegin {
        operation: String,
        version: VersionStamp,
        path: String,
        size: u64,
        sha256: String,
        #[serde(default)]
        cache_only: bool,
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
    materialize: bool,
    cache_only: bool,
    received: u64,
    staging: PathBuf,
    file: File,
}

pub struct FleetfilesReplica {
    root: PathBuf,
    connection: Mutex<Connection>,
    storage_allocations: Mutex<Vec<StorageAllocationRoot>>,
    local_device: Mutex<Option<String>>,
    history_retention_days: AtomicU64,
    ledger_digest_cache: Mutex<Option<(i64, FleetfilesLedgerDigest)>>,
    content_store_lock: Mutex<()>,
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
    pub(crate) fn memory(root: PathBuf) -> Self {
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
                 DROP TRIGGER IF EXISTS fleetfiles_path_search_insert;
                 DROP TRIGGER IF EXISTS fleetfiles_path_search_delete;
                 DROP TRIGGER IF EXISTS fleetfiles_path_search_update;
                 CREATE TABLE IF NOT EXISTS version_history (
                   path TEXT NOT NULL,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   sha256 TEXT,
                   tombstone INTEGER NOT NULL,
                   recorded_at INTEGER NOT NULL,
                   PRIMARY KEY(path, counter, actor)
                 );
                 CREATE INDEX IF NOT EXISTS version_history_path
                   ON version_history(path, counter DESC, actor DESC);
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
                   ON replication_queue(target, queued_at, path);
                 CREATE TABLE IF NOT EXISTS content_version_queue (
                   target TEXT NOT NULL,
                   path TEXT NOT NULL,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   sha256 TEXT,
                   queued_at INTEGER NOT NULL,
                   PRIMARY KEY(target, path, counter, actor)
                 );
                 CREATE INDEX IF NOT EXISTS content_version_queue_target
                   ON content_version_queue(target, queued_at, counter, actor, path);
                 CREATE TABLE IF NOT EXISTS metadata_queue (
                   target TEXT NOT NULL,
                   path TEXT NOT NULL,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   sha256 TEXT,
                   tombstone INTEGER NOT NULL,
                   queued_at INTEGER NOT NULL,
                   PRIMARY KEY(target, path)
                 );
                 CREATE INDEX IF NOT EXISTS metadata_queue_target
                   ON metadata_queue(target, queued_at, path);
                 CREATE TABLE IF NOT EXISTS metadata_history_queue (
                   target TEXT NOT NULL,
                   path TEXT NOT NULL,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   sha256 TEXT,
                   tombstone INTEGER NOT NULL,
                   queued_at INTEGER NOT NULL,
                   PRIMARY KEY(target, path, counter, actor)
                 );
                 CREATE INDEX IF NOT EXISTS metadata_history_queue_target
                   ON metadata_history_queue(target, queued_at, counter, actor, path);
                 CREATE TABLE IF NOT EXISTS fleetfiles_meta (
                   key TEXT PRIMARY KEY,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS content_replicas (
                   allocation_id TEXT NOT NULL,
                   sha256 TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   path TEXT NOT NULL,
                   verified_at INTEGER NOT NULL,
                   PRIMARY KEY(allocation_id, sha256)
                 );
                 CREATE INDEX IF NOT EXISTS content_replicas_allocation
                   ON content_replicas(allocation_id, verified_at);
                 CREATE TABLE IF NOT EXISTS content_cache (
                   sha256 TEXT PRIMARY KEY,
                   size INTEGER NOT NULL,
                   path TEXT NOT NULL,
                   accessed_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS content_cache_accessed
                   ON content_cache(accessed_at DESC, sha256);
                 CREATE TABLE IF NOT EXISTS replica_receipts (
                   device TEXT NOT NULL,
                   path TEXT NOT NULL,
                   counter INTEGER NOT NULL,
                   actor TEXT NOT NULL,
                   sha256 TEXT NOT NULL,
                   size INTEGER NOT NULL,
                   verified_at INTEGER NOT NULL,
                   PRIMARY KEY(device, path, counter, actor)
                 );
                 CREATE INDEX IF NOT EXISTS replica_receipts_content
                   ON replica_receipts(sha256, verified_at);
                 BEGIN IMMEDIATE;
                 INSERT OR IGNORE INTO fleetfiles_meta(key, value)
                   VALUES('root_layout', 'desktop_v1');
                 UPDATE path_versions SET path = char(31) || path
                   WHERE (SELECT value FROM fleetfiles_meta WHERE key='root_layout') = 'desktop_v1';
                 UPDATE path_versions SET path = 'Desktop/' || substr(path, 2)
                   WHERE unicode(substr(path, 1, 1)) = 31;
                 UPDATE replication_queue SET path = char(31) || path
                   WHERE (SELECT value FROM fleetfiles_meta WHERE key='root_layout') = 'desktop_v1';
                 UPDATE replication_queue SET path = 'Desktop/' || substr(path, 2)
                   WHERE unicode(substr(path, 1, 1)) = 31;
                 INSERT OR IGNORE INTO content_version_queue(
                   target,path,counter,actor,kind,size,sha256,queued_at
                 ) SELECT target,path,counter,actor,kind,size,sha256,queued_at
                   FROM replication_queue;
                 INSERT OR IGNORE INTO metadata_history_queue(
                   target,path,counter,actor,kind,size,sha256,tombstone,queued_at
                 ) SELECT target,path,counter,actor,kind,size,sha256,tombstone,queued_at
                   FROM metadata_queue;
                 UPDATE fleetfiles_meta SET value='fleet_root_v2' WHERE key='root_layout';
                 INSERT OR IGNORE INTO version_history(
                   path,counter,actor,kind,size,sha256,tombstone,recorded_at
                 ) SELECT path,counter,actor,kind,size,sha256,tombstone,updated_at
                   FROM path_versions;
                 COMMIT;",
            )
            .expect("Fleetfiles metadata schema must initialize");
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS fleetfiles_path_search USING fts5(
                   path,
                   content='path_versions',
                   content_rowid='rowid',
                   tokenize='trigram'
                 );
                 CREATE TRIGGER fleetfiles_path_search_insert
                   AFTER INSERT ON path_versions BEGIN
                     INSERT INTO fleetfiles_path_search(rowid,path)
                       VALUES(new.rowid,new.path);
                   END;
                 CREATE TRIGGER fleetfiles_path_search_delete
                   AFTER DELETE ON path_versions BEGIN
                     INSERT INTO fleetfiles_path_search(
                       fleetfiles_path_search,rowid,path
                     ) VALUES('delete',old.rowid,old.path);
                   END;
                 CREATE TRIGGER fleetfiles_path_search_update
                   AFTER UPDATE ON path_versions BEGIN
                     INSERT INTO fleetfiles_path_search(
                       fleetfiles_path_search,rowid,path
                     ) VALUES('delete',old.rowid,old.path);
                     INSERT INTO fleetfiles_path_search(rowid,path)
                       VALUES(new.rowid,new.path);
                   END;",
            )
            .expect("Fleetfiles path search schema must initialize");
        let search_index_ready = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM fleetfiles_meta WHERE key='path_search_v1')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !search_index_ready {
            connection
                .execute(
                    "INSERT INTO fleetfiles_path_search(fleetfiles_path_search) VALUES('rebuild')",
                    [],
                )
                .expect("Fleetfiles path search index must initialize");
            connection
                .execute(
                    "INSERT OR REPLACE INTO fleetfiles_meta(key,value) VALUES('path_search_v1','ready')",
                    [],
                )
                .expect("Fleetfiles path search migration must record");
        }
        let staging = root.join(".allmystuff-staging");
        let _ = std::fs::create_dir_all(&staging);
        let _ = crate::files::mark_internal_staging_hidden(&staging);
        Self {
            root,
            connection: Mutex::new(connection),
            storage_allocations: Mutex::new(Vec::new()),
            local_device: Mutex::new(None),
            history_retention_days: AtomicU64::new(30),
            ledger_digest_cache: Mutex::new(None),
            content_store_lock: Mutex::new(()),
            inbound: Mutex::new(HashMap::new()),
            watcher: Mutex::new(None),
            overflowed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Install the policy watcher and stream one startup reconciliation through
    /// the same bounded queue. The walk holds no file list in memory; duplicate
    /// watcher events collapse naturally when capture sees the same version.
    pub fn start_watcher(&self) -> Result<mpsc::Receiver<LocalChange>, String> {
        let (tx, rx) = mpsc::sync_channel(CHANGE_QUEUE);
        let scan_tx = tx.clone();
        let overflowed = self.overflowed.clone();
        let root = self.root.clone();
        let scan_root = root.clone();
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
        let _ = std::thread::Builder::new()
            .name("fleetfiles-reconcile".into())
            .spawn(move || {
                let root = scan_root.clone();
                let mut directories = vec![scan_root];
                while let Some(directory) = directories.pop() {
                    let Ok(children) = std::fs::read_dir(&directory) else {
                        continue;
                    };
                    for child in children.flatten() {
                        let path = child.path();
                        if is_internal(&root, &path) {
                            continue;
                        }
                        let Ok(file_type) = child.file_type() else {
                            continue;
                        };
                        if file_type.is_symlink() {
                            continue;
                        }
                        let Ok(metadata) = child.metadata() else {
                            continue;
                        };
                        if scan_tx.send(LocalChange { path: path.clone() }).is_err() {
                            return;
                        }
                        if metadata.is_dir() {
                            directories.push(path);
                        }
                    }
                }
            });
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

    /// Read a bounded page from the authoritative logical namespace. This is
    /// independent of whether any body is currently materialized on this
    /// computer; the cursor is the last portable path returned.
    pub fn list_directory(
        &self,
        parent: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<LogicalDirectoryPage, String> {
        if !parent.is_empty() {
            validate_portable_path(parent)?;
        }
        let prefix = if parent.is_empty() {
            String::new()
        } else {
            format!("{parent}/")
        };
        let after = cursor.unwrap_or(&prefix);
        if let Some(cursor) = cursor {
            validate_portable_path(cursor)?;
            let rest = cursor
                .strip_prefix(&prefix)
                .ok_or("Fleetfiles directory cursor belongs to another parent")?;
            if rest.is_empty() || rest.contains('/') {
                return Err("invalid Fleetfiles directory cursor".into());
            }
        }
        let limit = limit.clamp(1, 512);
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT path,counter,actor,kind,size,sha256,updated_at
                 FROM path_versions
                 WHERE tombstone=0
                   AND substr(path,1,?1)=?2
                   AND instr(substr(path,?3),'/')=0
                   AND path>?4
                 ORDER BY path
                 LIMIT ?5",
            )
            .map_err(|error| format!("prepare Fleetfiles logical directory: {error}"))?;
        let rows = statement
            .query_map(
                params![
                    prefix.chars().count() as u64,
                    prefix,
                    prefix.chars().count() as u64 + 1,
                    after,
                    limit as u64 + 1,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .map_err(|error| format!("read Fleetfiles logical directory: {error}"))?;
        let mut decoded = Vec::new();
        for row in rows {
            decoded.push(
                row.map_err(|error| format!("decode Fleetfiles logical directory: {error}"))?,
            );
        }
        let has_more = decoded.len() > limit;
        decoded.truncate(limit);
        let mut entries = Vec::with_capacity(decoded.len());
        for (path, counter, actor, kind, size, sha256, modified) in decoded {
            let content_available = sha256.as_deref().is_some_and(|hash| {
                connection
                    .query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM content_replicas WHERE sha256=?1
                           UNION ALL
                           SELECT 1 FROM content_cache WHERE sha256=?1
                         )",
                        params![hash],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            });
            let materialized = resolve_portable(&self.root, &path)
                .map(|candidate| candidate.exists())
                .unwrap_or(false);
            entries.push(LogicalEntry {
                name: path.rsplit('/').next().unwrap_or(&path).to_string(),
                path,
                kind,
                size,
                sha256,
                modified,
                version: VersionStamp { counter, actor },
                materialized,
                content_available,
            });
        }
        let next_cursor = has_more
            .then(|| entries.last().map(|entry| entry.path.clone()))
            .flatten();
        Ok(LogicalDirectoryPage {
            parent: parent.to_string(),
            entries,
            next_cursor,
        })
    }

    /// Search the current logical namespace without materializing it. The
    /// trigram index makes ordinary substring searches independent of the
    /// number of file bodies, while keyset paging keeps every response bounded.
    pub fn search(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<LogicalSearchPage, String> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(LogicalSearchPage {
                query: String::new(),
                entries: Vec::new(),
                next_cursor: None,
            });
        }
        if query.len() > 1024 {
            return Err("Fleetfiles search query is too long".into());
        }
        if let Some(cursor) = cursor {
            validate_portable_path(cursor)?;
        }
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let after = cursor.unwrap_or("");
        let limit = limit.clamp(1, 512);
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT current.path,current.counter,current.actor,current.kind,
                        current.size,current.sha256,current.updated_at
                 FROM fleetfiles_path_search AS search
                 JOIN path_versions AS current ON current.rowid=search.rowid
                 WHERE search.path LIKE ?1 ESCAPE '\\'
                   AND current.tombstone=0 AND current.path>?2
                 ORDER BY current.path
                 LIMIT ?3",
            )
            .map_err(|error| format!("prepare Fleetfiles logical search: {error}"))?;
        let rows = statement
            .query_map(params![pattern, after, limit as u64 + 1], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|error| format!("read Fleetfiles logical search: {error}"))?;
        let mut decoded = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("decode Fleetfiles logical search: {error}"))?;
        let has_more = decoded.len() > limit;
        decoded.truncate(limit);
        let mut entries = Vec::with_capacity(decoded.len());
        for (path, counter, actor, kind, size, sha256, modified) in decoded {
            let content_available = sha256.as_deref().is_some_and(|hash| {
                connection
                    .query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM content_replicas WHERE sha256=?1
                           UNION ALL
                           SELECT 1 FROM content_cache WHERE sha256=?1
                         )",
                        params![hash],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            });
            let materialized = resolve_portable(&self.root, &path)
                .map(|candidate| candidate.exists())
                .unwrap_or(false);
            entries.push(LogicalEntry {
                name: path.rsplit('/').next().unwrap_or(&path).to_string(),
                path,
                kind,
                size,
                sha256,
                modified,
                version: VersionStamp { counter, actor },
                materialized,
                content_available,
            });
        }
        let next_cursor = has_more
            .then(|| entries.last().map(|entry| entry.path.clone()))
            .flatten();
        Ok(LogicalSearchPage {
            query: query.to_string(),
            entries,
            next_cursor,
        })
    }

    pub fn version_history(
        &self,
        path: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<VersionHistoryPage, String> {
        validate_portable_path(path)?;
        let offset = cursor
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| "invalid Fleetfiles history cursor")?;
        let limit = limit.clamp(1, 256);
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT counter,actor,kind,size,sha256,tombstone,recorded_at
                 FROM version_history WHERE path=?1
                 ORDER BY counter DESC,actor DESC LIMIT ?2 OFFSET ?3",
            )
            .map_err(|error| format!("prepare Fleetfiles history: {error}"))?;
        let rows = statement
            .query_map(params![path, limit as u64 + 1, offset as u64], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|error| format!("read Fleetfiles history: {error}"))?;
        let mut entries = Vec::new();
        for row in rows {
            let (counter, actor, kind, size, sha256, tombstone, recorded_at) =
                row.map_err(|error| format!("decode Fleetfiles history: {error}"))?;
            let content_available = sha256.as_deref().is_some_and(|hash| {
                connection
                    .query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM content_replicas WHERE sha256=?1
                           UNION ALL
                           SELECT 1 FROM content_cache WHERE sha256=?1
                         )",
                        params![hash],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            });
            entries.push(VersionHistoryEntry {
                path: path.to_string(),
                version: VersionStamp { counter, actor },
                kind,
                size,
                sha256,
                tombstone,
                recorded_at,
                content_available,
            });
        }
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(VersionHistoryPage {
            path: path.to_string(),
            entries,
            next_cursor: has_more.then(|| (offset + limit).to_string()),
        })
    }

    /// Digest the append-only version ledger without loading it into memory.
    /// Recorded-at is deliberately excluded: receipt time differs by peer,
    /// while the logical version tuple and metadata must converge exactly.
    pub fn ledger_digest(&self) -> Result<FleetfilesLedgerDigest, String> {
        let connection = self.connection.lock();
        let last_row: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(rowid),0) FROM version_history",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("read Fleetfiles ledger revision: {error}"))?;
        if let Some((cached_row, digest)) = self.ledger_digest_cache.lock().as_ref() {
            if *cached_row == last_row {
                return Ok(digest.clone());
            }
        }
        let mut statement = connection
            .prepare(
                "SELECT path,counter,actor,kind,size,sha256,tombstone
                 FROM version_history ORDER BY path,counter,actor",
            )
            .map_err(|error| format!("prepare Fleetfiles ledger digest: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            })
            .map_err(|error| format!("read Fleetfiles ledger digest: {error}"))?;
        let mut digest = Sha256::new();
        let mut entries = 0_u64;
        for row in rows {
            let (path, counter, actor, kind, size, sha256, tombstone) =
                row.map_err(|error| format!("decode Fleetfiles ledger digest: {error}"))?;
            for field in [
                path.as_bytes(),
                actor.as_bytes(),
                kind.as_bytes(),
                sha256.as_deref().unwrap_or("").as_bytes(),
            ] {
                digest.update((field.len() as u64).to_be_bytes());
                digest.update(field);
            }
            digest.update(counter.to_be_bytes());
            digest.update(size.to_be_bytes());
            digest.update([u8::from(tombstone)]);
            entries = entries.saturating_add(1);
        }
        let result = FleetfilesLedgerDigest {
            entries,
            sha256: format!("{:x}", digest.finalize()),
        };
        *self.ledger_digest_cache.lock() = Some((last_row, result.clone()));
        Ok(result)
    }

    /// Return one keyset-paged ledger slice for reconnect anti-entropy.
    pub fn ledger_page(
        &self,
        after: Option<&FleetfilesLedgerCursor>,
        limit: usize,
    ) -> Result<FleetfilesLedgerPage, String> {
        if let Some(cursor) = after {
            validate_portable_path(&cursor.path)?;
            if cursor.actor.is_empty() || cursor.actor.len() > 512 {
                return Err("invalid Fleetfiles ledger cursor actor".into());
            }
        }
        let limit = limit.clamp(1, 128);
        let (after_path, after_counter, after_actor) = after
            .map(|cursor| (cursor.path.as_str(), cursor.counter, cursor.actor.as_str()))
            .unwrap_or(("", 0, ""));
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT path,counter,actor,kind,size,sha256,tombstone
                 FROM version_history
                 WHERE path>?1
                    OR (path=?1 AND counter>?2)
                    OR (path=?1 AND counter=?2 AND actor>?3)
                 ORDER BY path,counter,actor LIMIT ?4",
            )
            .map_err(|error| format!("prepare Fleetfiles ledger page: {error}"))?;
        let rows = statement
            .query_map(
                params![after_path, after_counter, after_actor, limit as u64 + 1],
                |row| {
                    let path = row.get::<_, String>(0)?;
                    let version = VersionStamp {
                        counter: row.get(1)?,
                        actor: row.get(2)?,
                    };
                    Ok(FleetfilesMetadata {
                        operation: operation_id(&version, &path),
                        path,
                        version,
                        kind: row.get(3)?,
                        size: row.get(4)?,
                        sha256: row.get(5)?,
                        tombstone: row.get(6)?,
                    })
                },
            )
            .map_err(|error| format!("read Fleetfiles ledger page: {error}"))?;
        let decoded = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("decode Fleetfiles ledger page: {error}"))?;
        let mut entries = Vec::with_capacity(decoded.len().min(limit));
        let mut encoded_bytes = 0_usize;
        for metadata in decoded.into_iter().take(limit) {
            let bytes = serde_json::to_vec(&metadata)
                .map_err(|error| format!("encode Fleetfiles ledger metadata: {error}"))?
                .len();
            if !entries.is_empty() && encoded_bytes.saturating_add(bytes) > LEDGER_PAGE_BYTES {
                break;
            }
            encoded_bytes = encoded_bytes.saturating_add(bytes);
            entries.push(metadata);
        }
        let has_more = entries.len() == limit
            || entries.last().is_some_and(|entry| {
                connection
                    .query_row(
                        "SELECT EXISTS(
                               SELECT 1 FROM version_history
                               WHERE path>?1
                                  OR (path=?1 AND counter>?2)
                                  OR (path=?1 AND counter=?2 AND actor>?3)
                             )",
                        params![entry.path, entry.version.counter, entry.version.actor,],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            });
        let next_cursor =
            has_more
                .then(|| entries.last())
                .flatten()
                .map(|metadata| FleetfilesLedgerCursor {
                    path: metadata.path.clone(),
                    counter: metadata.version.counter,
                    actor: metadata.version.actor.clone(),
                });
        Ok(FleetfilesLedgerPage {
            entries,
            next_cursor,
        })
    }

    pub fn file_version(
        &self,
        path: &str,
        version: &VersionStamp,
    ) -> Result<(u64, String), String> {
        validate_portable_path(path)?;
        self.connection
            .lock()
            .query_row(
                "SELECT size,sha256 FROM version_history
                 WHERE path=?1 AND counter=?2 AND actor=?3
                   AND kind='file' AND tombstone=0",
                params![path, version.counter, version.actor],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("locate Fleetfiles file version: {error}"))?
            .ok_or_else(|| "that Fleetfiles version is not a restorable file".into())
    }

    pub fn current_file_version(
        &self,
        path: &str,
    ) -> Result<Option<(VersionStamp, u64, String)>, String> {
        validate_portable_path(path)?;
        let current = read_current(&self.connection.lock(), path)?;
        Ok(current.and_then(|current| {
            (!current.tombstone && current.kind == "file").then(|| {
                (
                    current.version,
                    current.size,
                    current.sha256.unwrap_or_default(),
                )
            })
        }))
    }

    pub fn body_mutation(
        &self,
        operation: String,
        path: &str,
        version: &VersionStamp,
        size: u64,
        sha256: &str,
    ) -> Result<LocalMutation, String> {
        let (known_size, known_hash) = self.file_version(path, version)?;
        if known_size != size || known_hash != sha256 {
            return Err("Fleetfiles body request does not match version metadata".into());
        }
        let source = self
            .local_body_source(sha256, size)?
            .ok_or("that Fleetfiles body is not available on this device")?;
        Ok(LocalMutation::File {
            operation,
            version: version.clone(),
            path: path.to_string(),
            size,
            sha256: sha256.to_string(),
            source,
        })
    }

    pub fn has_body(&self, sha256: &str, size: u64) -> bool {
        self.local_body_source(sha256, size)
            .ok()
            .flatten()
            .is_some()
    }

    /// Materialize the current logical winner from a local immutable body.
    /// Returning `None` means the namespace is known but another replica must
    /// supply the body; it never means the file does not exist logically.
    pub fn materialize(&self, path: &str) -> Result<Option<String>, String> {
        validate_portable_path(path)?;
        let current = {
            let connection = self.connection.lock();
            read_current(&connection, path)?
        };
        let Some(current) = current else {
            return Ok(None);
        };
        if current.tombstone {
            return Ok(None);
        }
        let target = resolve_portable(&self.root, path)?;
        if current.kind == "directory" {
            std::fs::create_dir_all(&target)
                .map_err(|error| format!("materialize Fleetfiles directory: {error}"))?;
            return Ok(Some(target.to_string_lossy().into_owned()));
        }
        let Some(hash) = current.sha256.as_deref() else {
            return Err("current Fleetfiles file has no content hash".into());
        };
        if target.is_file() {
            match hash_file(&target) {
                Ok((size, actual)) if size == current.size && actual == hash => {
                    return Ok(Some(target.to_string_lossy().into_owned()));
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(
                        "could not validate Fleetfiles materialization {}: {error}",
                        target.display()
                    );
                }
            }
        }
        let Some(source) = self.local_body_source(hash, current.size)? else {
            return Ok(None);
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create Fleetfiles materialization parent: {error}"))?;
        }
        let stage_id = CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed);
        let staging = self
            .root
            .join(".allmystuff-staging")
            .join(format!("materialize-{stage_id}.part"));
        std::fs::copy(&source, &staging)
            .map_err(|error| format!("stage Fleetfiles materialization: {error}"))?;
        let (actual_size, actual_hash) = hash_file(&staging)?;
        if actual_size != current.size || actual_hash != hash {
            let _ = std::fs::remove_file(&staging);
            return Err("Fleetfiles materialization body failed verification".into());
        }
        replace_file(&staging, &target)?;
        Ok(Some(target.to_string_lossy().into_owned()))
    }

    /// Restore an available historical body into the working adapter. The
    /// watcher captures this write as a brand-new current version, so restore
    /// never rewinds clocks or destroys the version being replaced.
    pub fn restore_version(&self, path: &str, version: &VersionStamp) -> Result<String, String> {
        validate_portable_path(path)?;
        let historical: Option<(String, u64)> = self
            .connection
            .lock()
            .query_row(
                "SELECT sha256,size FROM version_history
                 WHERE path=?1 AND counter=?2 AND actor=?3
                   AND kind='file' AND tombstone=0",
                params![path, version.counter, version.actor],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("locate Fleetfiles history version: {error}"))?;
        let Some((hash, size)) = historical else {
            return Err("that Fleetfiles history version is not a restorable file".into());
        };
        let Some(source) = self.local_body_source(&hash, size)? else {
            return Err("that historical body is not available on this device".into());
        };
        let target = resolve_portable(&self.root, path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create Fleetfiles restore parent: {error}"))?;
        }
        let stage_id = CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed);
        let staging = self
            .root
            .join(".allmystuff-staging")
            .join(format!("restore-{stage_id}.part"));
        std::fs::copy(&source, &staging)
            .map_err(|error| format!("stage Fleetfiles history restore: {error}"))?;
        let (actual_size, actual_hash) = hash_file(&staging)?;
        if actual_size != size || actual_hash != hash {
            let _ = std::fs::remove_file(&staging);
            return Err("Fleetfiles history body failed restore verification".into());
        }
        replace_file(&staging, &target)?;
        Ok(target.to_string_lossy().into_owned())
    }

    /// Replace the local content-placement view with the currently enabled
    /// allocations. The storage plan remains authoritative; this is a bounded
    /// executor projection used for new immutable versions.
    pub fn set_storage_allocations(
        &self,
        mut allocations: Vec<StorageAllocationRoot>,
        local_device: String,
        history_retention_days: u16,
    ) {
        allocations.retain(|allocation| {
            !allocation.id.is_empty() && allocation.quota_bytes > 0 && allocation.root.is_absolute()
        });
        allocations.sort_by(|left, right| left.id.cmp(&right.id));
        allocations.dedup_by(|left, right| left.id == right.id);
        *self.storage_allocations.lock() = allocations;
        *self.local_device.lock() = (!local_device.is_empty()).then_some(local_device);
        self.history_retention_days
            .store(u64::from(history_retention_days), Ordering::Relaxed);
    }

    pub fn persist_mutation_content(&self, mutation: &LocalMutation) -> Result<bool, String> {
        match mutation {
            LocalMutation::File {
                source,
                size,
                sha256,
                ..
            } => self.persist_verified_content(source, *size, sha256),
            LocalMutation::Directory { .. } | LocalMutation::Delete { .. } => {
                Ok(!self.storage_allocations.lock().is_empty())
            }
        }
    }

    /// Store one verified immutable body on exactly one enabled allocation on
    /// this device. Multiple volumes on one device form a capacity pool, not
    /// independent failure domains, so the scheduler stripes bodies across
    /// them instead of creating policy-meaningless duplicates.
    fn persist_verified_content(
        &self,
        source: &Path,
        size: u64,
        sha256: &str,
    ) -> Result<bool, String> {
        validate_hash(sha256)?;
        let allocations = self.storage_allocations.lock().clone();
        if allocations.is_empty() {
            return Ok(false);
        }
        let _store_guard = self.content_store_lock.lock();

        for allocation in &allocations {
            let target = allocation_content_path(allocation, sha256);
            if target.is_file() {
                let (actual_size, actual_hash) = hash_file(&target)?;
                if actual_size == size && actual_hash == sha256 {
                    record_content_replica(
                        &self.connection.lock(),
                        allocation,
                        sha256,
                        size,
                        &target,
                    )?;
                    return Ok(true);
                }
            }
        }

        let mut eligible = Vec::new();
        for allocation in allocations {
            let mut used = self
                .connection
                .lock()
                .query_row(
                    "SELECT COALESCE(SUM(size), 0) FROM content_replicas
                     WHERE allocation_id=?1",
                    params![allocation.id],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap_or(0);
            if used.saturating_add(size) > allocation.quota_bytes {
                used = self.reclaim_historical_content(&allocation, used, size)?;
            }
            if used.saturating_add(size) <= allocation.quota_bytes {
                eligible.push((allocation, used));
            }
        }
        eligible.sort_by(|(left, left_used), (right, right_used)| {
            (u128::from(*left_used) * u128::from(right.quota_bytes))
                .cmp(&(u128::from(*right_used) * u128::from(left.quota_bytes)))
                .then_with(|| left.id.cmp(&right.id))
        });
        let Some((allocation, _)) = eligible.into_iter().next() else {
            return Err("enabled Fleetfiles allocations have no remaining content budget".into());
        };
        let target = allocation_content_path(&allocation, sha256);
        let parent = target
            .parent()
            .ok_or("Fleetfiles content target has no parent")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create Fleetfiles content shard: {error}"))?;
        let stage_id = CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".{sha256}.{stage_id}.part"));
        std::fs::copy(source, &staging)
            .map_err(|error| format!("stage Fleetfiles immutable content: {error}"))?;
        let (actual_size, actual_hash) = hash_file(&staging)?;
        if actual_size != size || actual_hash != sha256 {
            let _ = std::fs::remove_file(&staging);
            return Err("Fleetfiles allocation verification failed".into());
        }
        replace_file(&staging, &target)?;
        record_content_replica(&self.connection.lock(), &allocation, sha256, size, &target)?;
        Ok(true)
    }

    /// Reclaim immutable bodies only when a current write cannot fit. Current
    /// namespace winners are never candidates. Bodies older than the policy
    /// window go first; if necessary, the oldest unpinned history inside the
    /// window yields next. Version metadata is intentionally retained.
    fn reclaim_historical_content(
        &self,
        allocation: &StorageAllocationRoot,
        mut used: u64,
        required: u64,
    ) -> Result<u64, String> {
        let retention_days = self.history_retention_days.load(Ordering::Relaxed);
        let cutoff = unix_now().saturating_sub(retention_days.saturating_mul(86_400)) as i64;
        let local_device = self.local_device.lock().clone();
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT cr.sha256,cr.size,cr.path,COALESCE(MAX(vh.recorded_at),0) AS last_seen
                 FROM content_replicas cr
                 LEFT JOIN version_history vh ON vh.sha256=cr.sha256
                 WHERE cr.allocation_id=?1
                   AND NOT EXISTS(
                     SELECT 1 FROM path_versions current
                     WHERE current.tombstone=0 AND current.kind='file'
                       AND current.sha256=cr.sha256
                   )
                   AND NOT EXISTS(
                     SELECT 1 FROM content_version_queue queued
                     WHERE queued.sha256=cr.sha256
                   )
                 GROUP BY cr.sha256,cr.size,cr.path
                 ORDER BY CASE WHEN last_seen<?2 THEN 0 ELSE 1 END,last_seen,cr.sha256",
            )
            .map_err(|error| format!("prepare Fleetfiles history reclamation: {error}"))?;
        let candidates = statement
            .query_map(params![allocation.id, cutoff], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| format!("read Fleetfiles history reclamation: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("decode Fleetfiles history reclamation: {error}"))?;
        drop(statement);
        for (sha256, bytes, path) in candidates {
            if used.saturating_add(required) <= allocation.quota_bytes {
                break;
            }
            match std::fs::remove_file(Path::new(&path)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!("could not reclaim Fleetfiles history body {sha256}: {error}");
                    continue;
                }
            }
            connection
                .execute(
                    "DELETE FROM content_replicas WHERE allocation_id=?1 AND sha256=?2",
                    params![allocation.id, sha256],
                )
                .map_err(|error| format!("forget reclaimed Fleetfiles history body: {error}"))?;
            if let Some(device) = local_device.as_deref() {
                connection
                    .execute(
                        "DELETE FROM replica_receipts WHERE device=?1 AND sha256=?2",
                        params![device, sha256],
                    )
                    .map_err(|error| format!("forget reclaimed Fleetfiles receipt: {error}"))?;
            }
            used = used.saturating_sub(bytes);
        }
        Ok(used)
    }

    fn outbound_content_path(&self, sha256: &str) -> PathBuf {
        self.root
            .join(".allmystuff-staging")
            .join("outbound-v1")
            .join(&sha256[..2])
            .join(sha256)
    }

    fn ensure_outbound_body(&self, source: &Path, size: u64, sha256: &str) -> Result<(), String> {
        if self.has_local_content(sha256, size) {
            return Ok(());
        }
        let target = self.outbound_content_path(sha256);
        if target.is_file() {
            let (actual_size, actual_hash) = hash_file(&target)?;
            if actual_size == size && actual_hash == sha256 {
                return Ok(());
            }
        }
        let parent = target
            .parent()
            .ok_or("Fleetfiles outbound content has no parent")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create Fleetfiles outbound cache: {error}"))?;
        let stage_id = CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".{sha256}.{stage_id}.part"));
        std::fs::copy(source, &staging)
            .map_err(|error| format!("stage Fleetfiles outbound content: {error}"))?;
        let (actual_size, actual_hash) = hash_file(&staging)?;
        if actual_size != size || actual_hash != sha256 {
            let _ = std::fs::remove_file(&staging);
            return Err("Fleetfiles outbound content changed before it could be queued".into());
        }
        replace_file(&staging, &target)
    }

    fn queued_content_source(
        &self,
        connection: &Connection,
        logical_path: &str,
        size: u64,
        sha256: Option<&str>,
    ) -> Result<PathBuf, String> {
        let sha256 = sha256.ok_or("queued Fleetfiles file has no content hash")?;
        if let Some(path) = connection
            .query_row(
                "SELECT path FROM content_replicas WHERE sha256=?1 AND size=?2
                 ORDER BY verified_at DESC LIMIT 1",
                params![sha256, size],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("locate queued Fleetfiles content: {error}"))?
            .filter(|path| Path::new(path).is_file())
        {
            return Ok(PathBuf::from(path));
        }
        let outbound = self.outbound_content_path(sha256);
        if outbound.is_file() {
            return Ok(outbound);
        }
        let visible = resolve_portable(&self.root, logical_path)?;
        let (actual_size, actual_hash) = hash_file(&visible)?;
        if actual_size == size && actual_hash == sha256 {
            Ok(visible)
        } else {
            Err("queued Fleetfiles content body is no longer available".into())
        }
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
                source,
                ..
            } => {
                self.ensure_outbound_body(source, *size, sha256)?;
                (path, version, "file", *size, Some(sha256.as_str()))
            }
            LocalMutation::Directory { version, path, .. } => (path, version, "directory", 0, None),
            LocalMutation::Delete { version, path, .. } => (path, version, "delete", 0, None),
        };
        self.connection
            .lock()
            .execute(
                "INSERT INTO content_version_queue(
                   target,path,counter,actor,kind,size,sha256,queued_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,unixepoch())
                 ON CONFLICT(target,path,counter,actor) DO UPDATE SET
                   queued_at=excluded.queued_at",
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

    /// Queue namespace knowledge independently of content placement. Every
    /// fleet member receives this stream, including devices with no allocated
    /// storage volume.
    pub fn queue_metadata(
        &self,
        target: &str,
        metadata: &FleetfilesMetadata,
    ) -> Result<(), String> {
        if target.is_empty() || target.len() > 512 {
            return Err("invalid Fleetfiles metadata target".into());
        }
        validate_metadata(metadata)?;
        self.connection
            .lock()
            .execute(
                "INSERT INTO metadata_history_queue(
                   target,path,counter,actor,kind,size,sha256,tombstone,queued_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,unixepoch())
                 ON CONFLICT(target,path,counter,actor) DO UPDATE SET
                   queued_at=excluded.queued_at",
                params![
                    target,
                    metadata.path,
                    metadata.version.counter,
                    metadata.version.actor,
                    metadata.kind,
                    metadata.size,
                    metadata.sha256,
                    metadata.tombstone,
                ],
            )
            .map_err(|error| format!("queue Fleetfiles metadata: {error}"))?;
        Ok(())
    }

    pub fn pending_metadata(
        &self,
        target: &str,
        limit: usize,
    ) -> Result<Vec<FleetfilesMetadata>, String> {
        let limit = limit.clamp(1, 256);
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT path,counter,actor,kind,size,sha256,tombstone
                 FROM metadata_history_queue WHERE target=?1
                 ORDER BY queued_at,counter,actor,path LIMIT ?2",
            )
            .map_err(|error| format!("prepare Fleetfiles metadata queue: {error}"))?;
        let rows = statement
            .query_map(params![target, limit as u64], |row| {
                let path = row.get::<_, String>(0)?;
                let version = VersionStamp {
                    counter: row.get(1)?,
                    actor: row.get(2)?,
                };
                Ok(FleetfilesMetadata {
                    operation: operation_id(&version, &path),
                    path,
                    version,
                    kind: row.get(3)?,
                    size: row.get(4)?,
                    sha256: row.get(5)?,
                    tombstone: row.get(6)?,
                })
            })
            .map_err(|error| format!("read Fleetfiles metadata queue: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("decode Fleetfiles metadata queue: {error}"))
    }

    pub fn acknowledge_metadata(
        &self,
        target: &str,
        metadata: &FleetfilesMetadata,
    ) -> Result<(), String> {
        self.connection
            .lock()
            .execute(
                "DELETE FROM metadata_history_queue
                 WHERE target=?1 AND path=?2 AND counter=?3 AND actor=?4",
                params![
                    target,
                    metadata.path,
                    metadata.version.counter,
                    metadata.version.actor,
                ],
            )
            .map_err(|error| format!("acknowledge Fleetfiles metadata: {error}"))?;
        Ok(())
    }

    pub fn apply_metadata(&self, metadata: &FleetfilesMetadata) -> Result<bool, String> {
        validate_metadata(metadata)?;
        let connection = self.connection.lock();
        if let Some(current) = read_current(&connection, &metadata.path)? {
            if current.version > metadata.version {
                append_version_history(
                    &connection,
                    &metadata.path,
                    &metadata.version,
                    &metadata.kind,
                    metadata.size,
                    metadata.sha256.as_deref(),
                    metadata.tombstone,
                )?;
                return Ok(false);
            }
            if current.version == metadata.version {
                ensure_same_metadata(&current, metadata)?;
                append_version_history(
                    &connection,
                    &metadata.path,
                    &metadata.version,
                    &metadata.kind,
                    metadata.size,
                    metadata.sha256.as_deref(),
                    metadata.tombstone,
                )?;
                return Ok(false);
            }
        }
        put_version(
            &connection,
            &metadata.path,
            &metadata.version,
            &metadata.kind,
            metadata.size,
            metadata.sha256.as_deref(),
            metadata.tombstone,
        )?;
        Ok(true)
    }

    pub fn record_replica_receipt(
        &self,
        device: &str,
        mutation: &LocalMutation,
    ) -> Result<(), String> {
        let LocalMutation::File {
            version,
            path,
            size,
            sha256,
            ..
        } = mutation
        else {
            return Ok(());
        };
        if device.is_empty() || device.len() > 512 {
            return Err("invalid Fleetfiles replica receipt device".into());
        }
        self.connection
            .lock()
            .execute(
                "INSERT INTO replica_receipts(
                   device,path,counter,actor,sha256,size,verified_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,unixepoch())
                 ON CONFLICT(device,path,counter,actor) DO UPDATE SET
                   sha256=excluded.sha256,size=excluded.size,
                   verified_at=excluded.verified_at",
                params![device, path, version.counter, version.actor, sha256, size],
            )
            .map_err(|error| format!("record Fleetfiles replica receipt: {error}"))?;
        Ok(())
    }

    pub fn replica_count(&self, path: &str, version: &VersionStamp) -> u64 {
        self.connection
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM replica_receipts
                 WHERE path=?1 AND counter=?2 AND actor=?3",
                params![path, version.counter, version.actor],
                |row| row.get(0),
            )
            .unwrap_or(0)
    }

    pub fn pending_for(&self, target: &str, limit: usize) -> Result<Vec<LocalMutation>, String> {
        let limit = limit.clamp(1, 128);
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT path,counter,actor,kind,size,sha256
                 FROM content_version_queue
                 WHERE target=?1
                 ORDER BY queued_at,counter,actor,path
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
                    source: self.queued_content_source(
                        &connection,
                        &path,
                        size,
                        sha256.as_deref(),
                    )?,
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
        let (path, version, sha256) = match mutation {
            LocalMutation::File {
                path,
                version,
                sha256,
                ..
            } => (path, version, Some(sha256.as_str())),
            LocalMutation::Directory { path, version, .. }
            | LocalMutation::Delete { path, version, .. } => (path, version, None),
        };
        let connection = self.connection.lock();
        connection
            .execute(
                "DELETE FROM content_version_queue
                 WHERE target=?1 AND path=?2 AND counter=?3 AND actor=?4",
                params![target, path, version.counter, version.actor],
            )
            .map_err(|error| format!("acknowledge Fleetfiles replica: {error}"))?;
        if let Some(hash) = sha256 {
            let still_queued: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM content_version_queue WHERE sha256=?1)",
                    params![hash],
                    |row| row.get(0),
                )
                .unwrap_or(true);
            if !still_queued {
                let _ = std::fs::remove_file(self.outbound_content_path(hash));
            }
        }
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
        body: (u64, String),
        cache_only: bool,
    ) -> Result<bool, String> {
        let (size, sha256) = body;
        validate_portable_path(&path)?;
        validate_hash(&sha256)?;
        let current = {
            let connection = self.connection.lock();
            read_current(&connection, &path)?
        };
        let mut materialize = !cache_only;
        if let Some(current) = current {
            if current.version > version {
                let known_history: bool = self
                    .connection
                    .lock()
                    .query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM version_history
                           WHERE path=?1 AND counter=?2 AND actor=?3
                             AND kind='file' AND size=?4 AND sha256=?5 AND tombstone=0
                         )",
                        params![path, version.counter, version.actor, size, sha256],
                        |row| row.get(0),
                    )
                    .map_err(|error| format!("validate historical Fleetfiles body: {error}"))?;
                if !known_history {
                    return Err("historical Fleetfiles body has no matching metadata".into());
                }
                if if cache_only {
                    self.local_body_source(&sha256, size)?.is_some()
                } else {
                    self.has_local_content(&sha256, size)
                } {
                    return Ok(false);
                }
                materialize = false;
            }
            if current.version == version {
                let metadata = FleetfilesMetadata {
                    operation: operation.clone(),
                    version: version.clone(),
                    path: path.clone(),
                    kind: "file".into(),
                    size,
                    sha256: Some(sha256.clone()),
                    tombstone: false,
                };
                ensure_same_metadata(&current, &metadata)?;
                if if cache_only {
                    self.local_body_source(&sha256, size)?.is_some()
                } else {
                    self.has_local_content(&sha256, size)
                } {
                    return Ok(false);
                }
            }
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
                materialize,
                cache_only,
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
        if transfer.cache_only {
            self.persist_cached_content(&transfer.staging, transfer.size, &transfer.sha256)?;
            let _ = std::fs::remove_file(&transfer.staging);
            return Ok(transfer.path);
        }
        // Durable content belongs to an enabled allocation. The user-visible
        // sync root below is a working materialization kept temporarily for
        // the existing mount adapter during the placeholder migration.
        if !self.persist_verified_content(&transfer.staging, transfer.size, &transfer.sha256)? {
            let _ = std::fs::remove_file(&transfer.staging);
            return Err("this device has no Fleetfiles content allocation".into());
        }
        if !transfer.materialize {
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
        if let Some(current) = read_current(&self.connection.lock(), path)? {
            if current.version > version {
                return Ok(false);
            }
            if current.version == version {
                ensure_same_metadata(
                    &current,
                    &FleetfilesMetadata {
                        operation: operation_id(&version, path),
                        version: version.clone(),
                        path: path.to_string(),
                        kind: "directory".into(),
                        size: 0,
                        sha256: None,
                        tombstone: false,
                    },
                )?;
            }
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
        if let Some(current) = read_current(&self.connection.lock(), path)? {
            if current.version > version {
                return Ok(false);
            }
            if current.version == version {
                ensure_same_metadata(
                    &current,
                    &FleetfilesMetadata {
                        operation: operation_id(&version, path),
                        version: version.clone(),
                        path: path.to_string(),
                        kind: "delete".into(),
                        size: 0,
                        sha256: None,
                        tombstone: true,
                    },
                )?;
            }
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

    fn has_local_content(&self, sha256: &str, size: u64) -> bool {
        let replica: Option<String> = self
            .connection
            .lock()
            .query_row(
                "SELECT path FROM content_replicas WHERE sha256=?1 AND size=?2 LIMIT 1",
                params![sha256, size],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        replica.is_some_and(|path| Path::new(&path).is_file())
    }

    fn local_body_source(&self, sha256: &str, size: u64) -> Result<Option<PathBuf>, String> {
        validate_hash(sha256)?;
        let connection = self.connection.lock();
        let replica = connection
            .query_row(
                "SELECT path FROM content_replicas WHERE sha256=?1 AND size=?2
                 ORDER BY verified_at DESC LIMIT 1",
                params![sha256, size],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("locate Fleetfiles content body: {error}"))?;
        if let Some(path) = replica.filter(|path| Path::new(path).is_file()) {
            return Ok(Some(PathBuf::from(path)));
        }
        let cached = connection
            .query_row(
                "SELECT path FROM content_cache WHERE sha256=?1 AND size=?2",
                params![sha256, size],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("locate Fleetfiles cached body: {error}"))?;
        let Some(path) = cached else {
            return Ok(None);
        };
        if !Path::new(&path).is_file() {
            let _ =
                connection.execute("DELETE FROM content_cache WHERE sha256=?1", params![sha256]);
            return Ok(None);
        }
        connection
            .execute(
                "UPDATE content_cache SET accessed_at=unixepoch() WHERE sha256=?1",
                params![sha256],
            )
            .map_err(|error| format!("touch Fleetfiles cached body: {error}"))?;
        Ok(Some(PathBuf::from(path)))
    }

    fn persist_cached_content(&self, source: &Path, size: u64, sha256: &str) -> Result<(), String> {
        validate_hash(sha256)?;
        let target = self
            .root
            .join(".allmystuff-staging")
            .join("cache-v1")
            .join(&sha256[..2])
            .join(sha256);
        let parent = target
            .parent()
            .ok_or("Fleetfiles body cache target has no parent")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create Fleetfiles body cache shard: {error}"))?;
        if !target.is_file() {
            std::fs::copy(source, &target)
                .map_err(|error| format!("cache Fleetfiles body: {error}"))?;
        }
        let (actual_size, actual_hash) = hash_file(&target)?;
        if actual_size != size || actual_hash != sha256 {
            let _ = std::fs::remove_file(&target);
            return Err("Fleetfiles cached body failed verification".into());
        }
        let connection = self.connection.lock();
        connection
            .execute(
                "INSERT INTO content_cache(sha256,size,path,accessed_at)
                 VALUES (?1,?2,?3,unixepoch())
                 ON CONFLICT(sha256) DO UPDATE SET
                   size=excluded.size,path=excluded.path,accessed_at=excluded.accessed_at",
                params![sha256, size, target.to_string_lossy().into_owned()],
            )
            .map_err(|error| format!("record Fleetfiles cached body: {error}"))?;
        let cutoff = unix_now() as i64 - BODY_CACHE_MAX_AGE_SECS;
        let mut statement = connection
            .prepare(
                "SELECT sha256,size,path FROM content_cache
                 ORDER BY accessed_at DESC,sha256",
            )
            .map_err(|error| format!("prepare Fleetfiles body cache cleanup: {error}"))?;
        let cached = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| format!("read Fleetfiles body cache cleanup: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("decode Fleetfiles body cache cleanup: {error}"))?;
        drop(statement);
        let mut kept = 0_usize;
        let mut kept_bytes = 0_u64;
        for (hash, bytes, path) in cached {
            let accessed = connection
                .query_row(
                    "SELECT accessed_at FROM content_cache WHERE sha256=?1",
                    params![hash],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0);
            let keep_newest = kept == 0;
            let keep = keep_newest
                || (accessed >= cutoff
                    && kept < BODY_CACHE_ENTRIES
                    && kept_bytes.saturating_add(bytes) <= BODY_CACHE_BYTES);
            if keep {
                kept += 1;
                kept_bytes = kept_bytes.saturating_add(bytes);
                continue;
            }
            let _ = std::fs::remove_file(&path);
            connection
                .execute("DELETE FROM content_cache WHERE sha256=?1", params![hash])
                .map_err(|error| format!("forget Fleetfiles cached body: {error}"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct CurrentVersion {
    version: VersionStamp,
    kind: String,
    size: u64,
    sha256: Option<String>,
    tombstone: bool,
}

fn read_current(connection: &Connection, path: &str) -> Result<Option<CurrentVersion>, String> {
    connection
        .query_row(
            "SELECT counter,actor,kind,size,sha256,tombstone
             FROM path_versions WHERE path=?1",
            params![path],
            |row| {
                Ok(CurrentVersion {
                    version: VersionStamp {
                        counter: row.get(0)?,
                        actor: row.get(1)?,
                    },
                    kind: row.get(2)?,
                    size: row.get(3)?,
                    sha256: row.get(4)?,
                    tombstone: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read Fleetfiles conflict version: {error}"))
}

fn validate_metadata(metadata: &FleetfilesMetadata) -> Result<(), String> {
    validate_portable_path(&metadata.path)?;
    if metadata.version.actor.is_empty() || metadata.version.actor.len() > 512 {
        return Err("invalid Fleetfiles metadata actor".into());
    }
    match metadata.kind.as_str() {
        "file" if !metadata.tombstone && metadata.sha256.is_some() => {
            validate_hash(metadata.sha256.as_deref().unwrap())
        }
        "directory" if !metadata.tombstone && metadata.size == 0 && metadata.sha256.is_none() => {
            Ok(())
        }
        "delete" if metadata.tombstone && metadata.size == 0 && metadata.sha256.is_none() => Ok(()),
        _ => Err("invalid Fleetfiles metadata shape".into()),
    }
}

fn ensure_same_metadata(
    current: &CurrentVersion,
    incoming: &FleetfilesMetadata,
) -> Result<(), String> {
    if current.kind == incoming.kind
        && current.size == incoming.size
        && current.sha256 == incoming.sha256
        && current.tombstone == incoming.tombstone
    {
        Ok(())
    } else {
        Err("Fleetfiles version stamp collision".into())
    }
}

fn allocation_content_path(allocation: &StorageAllocationRoot, sha256: &str) -> PathBuf {
    allocation
        .root
        .join("content-v1")
        .join(&sha256[..2])
        .join(sha256)
}

fn record_content_replica(
    connection: &Connection,
    allocation: &StorageAllocationRoot,
    sha256: &str,
    size: u64,
    path: &Path,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO content_replicas(allocation_id,sha256,size,path,verified_at)
             VALUES (?1,?2,?3,?4,unixepoch())
             ON CONFLICT(allocation_id,sha256) DO UPDATE SET
               size=excluded.size,path=excluded.path,verified_at=excluded.verified_at",
            params![
                allocation.id,
                sha256,
                size,
                path.to_string_lossy().into_owned(),
            ],
        )
        .map_err(|error| format!("record Fleetfiles content replica: {error}"))?;
    Ok(())
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
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| format!("begin Fleetfiles version commit: {error}"))?;
    append_version_history(&transaction, path, version, kind, size, sha256, tombstone)?;
    transaction
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
    transaction
        .commit()
        .map_err(|error| format!("finish Fleetfiles version commit: {error}"))?;
    Ok(())
}

fn append_version_history(
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
            "INSERT OR IGNORE INTO version_history(
               path,counter,actor,kind,size,sha256,tombstone,recorded_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,unixepoch())",
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
        .map_err(|error| format!("append Fleetfiles history: {error}"))?;
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

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
            || name == ".allmystuff-fleetfiles-root"
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
    fn ignores_replication_internals_and_macos_bookkeeping() {
        let root = PathBuf::from("fleetfiles-root");
        assert!(is_internal(&root, &root.join(".allmystuff-staging/chunk")));
        assert!(is_internal(
            &root,
            &root.join(".allmystuff-fleetfiles-root")
        ));
        assert!(is_internal(&root, &root.join("photos/.DS_Store")));
        assert!(is_internal(&root, &root.join("photos/._image.png")));
        assert!(is_internal(&root, &root.join(".Spotlight-V100/index")));
        assert!(!is_internal(&root, &root.join(".env")));
        assert!(!is_internal(&root, &root.join("photos/image.png")));
    }

    #[test]
    fn legacy_desktop_metadata_moves_under_the_visible_desktop_folder_once() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE path_versions (
                   path TEXT PRIMARY KEY, counter INTEGER NOT NULL, actor TEXT NOT NULL,
                   kind TEXT NOT NULL, size INTEGER NOT NULL, sha256 TEXT,
                   tombstone INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE replication_queue (
                   target TEXT NOT NULL, path TEXT NOT NULL, counter INTEGER NOT NULL,
                   actor TEXT NOT NULL, kind TEXT NOT NULL, size INTEGER NOT NULL,
                   sha256 TEXT, queued_at INTEGER NOT NULL, PRIMARY KEY(target, path)
                 );
                 INSERT INTO path_versions VALUES
                   ('report.txt', 1, 'a', 'file', 1, 'hash', 0, 1),
                   ('Desktop/already.txt', 2, 'a', 'file', 1, 'hash', 0, 1);
                 INSERT INTO replication_queue VALUES
                   ('peer', 'queued.txt', 1, 'a', 'file', 1, 'hash', 1);",
            )
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-layout-test-{}",
            std::process::id()
        ));
        let replica = FleetfilesReplica::with_connection(root.clone(), connection);
        let db = replica.connection.lock();
        let mut statement = db
            .prepare("SELECT path FROM path_versions ORDER BY path")
            .unwrap();
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            paths,
            vec![
                "Desktop/Desktop/already.txt".to_string(),
                "Desktop/report.txt".to_string()
            ]
        );
        let queued: String = db
            .query_row("SELECT path FROM replication_queue", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, "Desktop/queued.txt");
        let layout: String = db
            .query_row(
                "SELECT value FROM fleetfiles_meta WHERE key='root_layout'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(layout, "fleet_root_v2");
        drop(statement);
        drop(db);
        drop(replica);
        let _ = std::fs::remove_dir_all(root);
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
    fn equal_state_ledger_probe_is_constant_and_small() {
        let digest = FleetfilesLedgerDigest {
            entries: 10_000_000,
            sha256: "a".repeat(64),
        };
        let request = serde_json::to_vec(&FleetfilesMessage::LedgerProbe {
            operation: "ledger-probe".into(),
            digest: digest.clone(),
        })
        .unwrap();
        let response = serde_json::to_vec(&FleetfilesMessage::LedgerStatus {
            operation: "ledger-probe".into(),
            digest,
        })
        .unwrap();
        assert!(
            request.len() + response.len() < 512,
            "equal ledgers must reconcile with a constant-size exchange"
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
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "test-volume".into(),
                root: root.join("allocated"),
                quota_bytes: 1024,
            }],
            "local".into(),
            30,
        );
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
                (
                    5,
                    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
                ),
                false,
            )
            .unwrap());
        replica.write_chunk("peer", "one", 0, b"hello").unwrap();
        replica.commit_file("peer", "one").unwrap();
        assert_eq!(std::fs::read(root.join("hello.txt")).unwrap(), b"hello");
        assert!(replica
            .begin_file(
                "peer",
                "stale".into(),
                v1,
                "hello.txt".into(),
                (
                    0,
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                ),
                false,
            )
            .unwrap_err()
            .contains("version stamp collision"));
        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_arrives_before_content_without_blocking_materialization() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-metadata-first-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "test-volume".into(),
                root: root.join("allocated"),
                quota_bytes: 1024,
            }],
            "local".into(),
            30,
        );
        let version = VersionStamp {
            counter: 7,
            actor: "origin".into(),
        };
        let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let metadata = FleetfilesMetadata {
            operation: "metadata-first".into(),
            version: version.clone(),
            path: "Desktop/hello.txt".into(),
            kind: "file".into(),
            size: 5,
            sha256: Some(hash.into()),
            tombstone: false,
        };
        assert!(replica.apply_metadata(&metadata).unwrap());
        assert!(replica
            .begin_file(
                "peer",
                "metadata-first".into(),
                version,
                "Desktop/hello.txt".into(),
                (5, hash.into()),
                false,
            )
            .unwrap());
        replica
            .write_chunk("peer", "metadata-first", 0, b"hello")
            .unwrap();
        replica.commit_file("peer", "metadata-first").unwrap();
        assert_eq!(
            std::fs::read(root.join("Desktop/hello.txt")).unwrap(),
            b"hello"
        );
        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn late_history_metadata_and_body_never_replace_the_current_winner() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-late-history-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "history-volume".into(),
                root: root.join("allocated"),
                quota_bytes: 1024,
            }],
            "storage-device".into(),
            30,
        );
        let v1 = VersionStamp {
            counter: 1,
            actor: "origin".into(),
        };
        let v2 = VersionStamp {
            counter: 2,
            actor: "origin".into(),
        };
        let first_hash = "a7937b64b8caa58f03721bb6bacf5c78cb235febe0e70b1b84cd99541461a08e";
        let second_hash = "16367aacb67a4a017c8da8ab95682ccb390863780f7114dda0a0e0c55644c7c4";
        let newer = FleetfilesMetadata {
            operation: "newer".into(),
            version: v2.clone(),
            path: "Desktop/report.txt".into(),
            kind: "file".into(),
            size: 6,
            sha256: Some(second_hash.into()),
            tombstone: false,
        };
        let older = FleetfilesMetadata {
            operation: "older".into(),
            version: v1.clone(),
            path: "Desktop/report.txt".into(),
            kind: "file".into(),
            size: 5,
            sha256: Some(first_hash.into()),
            tombstone: false,
        };
        assert!(replica.apply_metadata(&newer).unwrap());
        assert!(!replica.apply_metadata(&older).unwrap());
        assert_eq!(
            replica
                .version_history("Desktop/report.txt", None, 8)
                .unwrap()
                .entries
                .len(),
            2
        );

        assert!(replica
            .begin_file(
                "peer",
                "older".into(),
                v1,
                "Desktop/report.txt".into(),
                (5, first_hash.into()),
                false,
            )
            .unwrap());
        replica.write_chunk("peer", "older", 0, b"first").unwrap();
        replica.commit_file("peer", "older").unwrap();
        assert!(!root.join("Desktop/report.txt").exists());

        assert!(replica
            .begin_file(
                "peer",
                "newer".into(),
                v2,
                "Desktop/report.txt".into(),
                (6, second_hash.into()),
                false,
            )
            .unwrap());
        replica.write_chunk("peer", "newer", 0, b"second").unwrap();
        replica.commit_file("peer", "newer").unwrap();
        assert_eq!(
            std::fs::read(root.join("Desktop/report.txt")).unwrap(),
            b"second"
        );

        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_directory_pages_and_history_do_not_depend_on_materialization() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-logical-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        let first = FleetfilesMetadata {
            operation: "one".into(),
            version: VersionStamp {
                counter: 1,
                actor: "peer".into(),
            },
            path: "Desktop/report.txt".into(),
            kind: "file".into(),
            size: 3,
            sha256: Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()),
            tombstone: false,
        };
        let mut second = first.clone();
        second.operation = "two".into();
        second.version.counter = 2;
        second.size = 0;
        second.sha256 =
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into());
        replica.apply_metadata(&first).unwrap();
        replica.apply_metadata(&second).unwrap();
        replica
            .apply_metadata(&FleetfilesMetadata {
                operation: "folder".into(),
                version: VersionStamp {
                    counter: 3,
                    actor: "peer".into(),
                },
                path: "Desktop/Projects".into(),
                kind: "directory".into(),
                size: 0,
                sha256: None,
                tombstone: false,
            })
            .unwrap();
        replica
            .apply_metadata(&FleetfilesMetadata {
                operation: "nested".into(),
                version: VersionStamp {
                    counter: 4,
                    actor: "peer".into(),
                },
                path: "Desktop/Projects/Quarterly Report.txt".into(),
                kind: "file".into(),
                size: 0,
                sha256: Some(
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                ),
                tombstone: false,
            })
            .unwrap();

        let page = replica.list_directory("Desktop", None, 1).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(page.next_cursor.is_some());
        assert!(!page.entries[0].materialized);
        let rest = replica
            .list_directory("Desktop", page.next_cursor.as_deref(), 8)
            .unwrap();
        assert_eq!(rest.entries.len(), 1);
        let history = replica
            .version_history("Desktop/report.txt", None, 8)
            .unwrap();
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].version.counter, 2);
        let search = replica.search("REPORT", None, 1).unwrap();
        assert_eq!(search.entries.len(), 1);
        assert!(search.next_cursor.is_some());
        assert!(!search.entries[0].materialized);
        let search_rest = replica
            .search("REPORT", search.next_cursor.as_deref(), 8)
            .unwrap();
        assert_eq!(search_rest.entries.len(), 1);
        assert!(search_rest.next_cursor.is_none());
        let search_plan = {
            let connection = replica.connection.lock();
            let mut statement = connection
                .prepare(
                    "EXPLAIN QUERY PLAN
                     SELECT rowid FROM fleetfiles_path_search
                     WHERE path LIKE '%report%'",
                )
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(
            search_plan
                .iter()
                .any(|step| step.contains("VIRTUAL TABLE INDEX")),
            "search must use the disposable FTS index: {search_plan:?}"
        );

        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_queue_preserves_every_offline_version_without_content() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-metadata-queue-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        let mut metadata = FleetfilesMetadata {
            operation: "one".into(),
            version: VersionStamp {
                counter: 1,
                actor: "local".into(),
            },
            path: "Desktop/Project".into(),
            kind: "directory".into(),
            size: 0,
            sha256: None,
            tombstone: false,
        };
        replica.queue_metadata("peer", &metadata).unwrap();
        let older = metadata.clone();
        metadata.operation = "two".into();
        metadata.version.counter = 2;
        metadata.kind = "delete".into();
        metadata.tombstone = true;
        replica.queue_metadata("peer", &metadata).unwrap();
        let pending = replica.pending_metadata("peer", 8).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].version.counter, 1);
        assert_eq!(pending[1].version.counter, 2);
        assert!(pending[1].tombstone);
        replica.acknowledge_metadata("peer", &metadata).unwrap();
        assert_eq!(replica.pending_metadata("peer", 8).unwrap().len(), 1);
        replica.acknowledge_metadata("peer", &older).unwrap();
        assert!(replica.pending_metadata("peer", 8).unwrap().is_empty());
        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn current_bodies_displace_oldest_history_when_capacity_is_needed() {
        let base = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-history-pressure-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let visible = base.join("visible");
        let allocation = base.join("allocated");
        std::fs::create_dir_all(visible.join("Desktop")).unwrap();
        let replica = FleetfilesReplica::memory(visible.clone());
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "small-volume".into(),
                root: allocation,
                quota_bytes: 8,
            }],
            "local-device".into(),
            30,
        );

        let report = visible.join("Desktop/report.txt");
        std::fs::write(&report, b"old").unwrap();
        let old = replica.capture(&report, "local-device").unwrap().unwrap();
        replica.persist_mutation_content(&old).unwrap();
        let LocalMutation::File {
            sha256: old_hash, ..
        } = &old
        else {
            panic!("file mutation expected");
        };
        std::fs::write(&report, b"new").unwrap();
        let new = replica.capture(&report, "local-device").unwrap().unwrap();
        replica.persist_mutation_content(&new).unwrap();

        let other = visible.join("Desktop/keep.txt");
        std::fs::write(&other, b"keep").unwrap();
        let current = replica.capture(&other, "local-device").unwrap().unwrap();
        replica.persist_mutation_content(&current).unwrap();

        let connection = replica.connection.lock();
        let old_present: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM content_replicas WHERE sha256=?1)",
                params![old_hash],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!old_present, "historical body should yield under pressure");
        let history_count: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM version_history WHERE path='Desktop/report.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            history_count, 2,
            "history metadata must survive reclamation"
        );
        drop(connection);
        drop(replica);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn materialize_and_restore_are_verified_and_restore_creates_new_history() {
        let base = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-restore-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let visible = base.join("visible");
        std::fs::create_dir_all(visible.join("Desktop")).unwrap();
        let file = visible.join("Desktop/report.txt");
        let replica = FleetfilesReplica::memory(visible.clone());
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "history-volume".into(),
                root: base.join("allocated"),
                quota_bytes: 1024,
            }],
            "local-device".into(),
            30,
        );
        std::fs::write(&file, b"first").unwrap();
        let first = replica.capture(&file, "local-device").unwrap().unwrap();
        replica.persist_mutation_content(&first).unwrap();
        let first_version = match &first {
            LocalMutation::File { version, .. } => version.clone(),
            _ => panic!("file mutation expected"),
        };
        std::fs::write(&file, b"second").unwrap();
        let second = replica.capture(&file, "local-device").unwrap().unwrap();
        replica.persist_mutation_content(&second).unwrap();

        std::fs::write(&file, b"stale").unwrap();
        assert!(replica.materialize("Desktop/report.txt").unwrap().is_some());
        assert_eq!(std::fs::read(&file).unwrap(), b"second");
        replica
            .restore_version("Desktop/report.txt", &first_version)
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"first");
        replica
            .capture(&file, "local-device")
            .unwrap()
            .expect("restore must become a new version");
        assert_eq!(
            replica
                .version_history("Desktop/report.txt", None, 8)
                .unwrap()
                .entries
                .len(),
            3
        );
        drop(replica);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn verified_content_is_placed_in_an_enabled_allocation() {
        let base = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-content-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let visible = base.join("visible");
        let allocation = base.join("allocated-volume");
        std::fs::create_dir_all(visible.join("Desktop")).unwrap();
        let source = visible.join("Desktop/report.txt");
        std::fs::write(&source, b"durable content").unwrap();

        let replica = FleetfilesReplica::memory(visible);
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "local-volume".into(),
                root: allocation.clone(),
                quota_bytes: 1024,
            }],
            "local-device".into(),
            30,
        );
        let mutation = replica.capture(&source, "local-device").unwrap().unwrap();
        assert!(replica.persist_mutation_content(&mutation).unwrap());

        let LocalMutation::File { sha256, .. } = mutation else {
            panic!("capturing a file must produce a file mutation");
        };
        let stored = allocation_content_path(
            &StorageAllocationRoot {
                id: "local-volume".into(),
                root: allocation,
                quota_bytes: 1024,
            },
            &sha256,
        );
        assert_eq!(std::fs::read(stored).unwrap(), b"durable content");
        assert_eq!(
            replica
                .connection
                .lock()
                .query_row("SELECT COUNT(*) FROM content_replicas", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            1
        );

        drop(replica);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn content_placement_refuses_to_exceed_the_allocation_budget() {
        let base = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-quota-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let visible = base.join("visible");
        let allocation = base.join("allocated-volume");
        std::fs::create_dir_all(visible.join("Desktop")).unwrap();
        let source = visible.join("Desktop/large.txt");
        std::fs::write(&source, b"larger than quota").unwrap();

        let replica = FleetfilesReplica::memory(visible);
        replica.set_storage_allocations(
            vec![StorageAllocationRoot {
                id: "tiny-volume".into(),
                root: allocation.clone(),
                quota_bytes: 4,
            }],
            "local-device".into(),
            30,
        );
        let mutation = replica.capture(&source, "local-device").unwrap().unwrap();
        let error = replica.persist_mutation_content(&mutation).unwrap_err();

        assert!(error.contains("no remaining content budget"));
        assert!(!allocation.join("content-v1").exists());
        assert_eq!(
            replica
                .connection
                .lock()
                .query_row("SELECT COUNT(*) FROM content_replicas", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            0
        );

        drop(replica);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn durable_content_queue_preserves_and_acknowledges_exact_versions() {
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
        assert_eq!(pending.len(), 2);
        assert!(matches!(
            &pending[0],
            LocalMutation::Directory {
                version: VersionStamp { counter: 1, .. },
                ..
            }
        ));
        assert!(matches!(
            &pending[1],
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

    #[test]
    fn offline_file_queue_keeps_the_exact_body_after_the_working_file_changes() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-outbound-queue-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("Desktop")).unwrap();
        let file = root.join("Desktop/report.txt");
        std::fs::write(&file, b"first body").unwrap();
        let replica = FleetfilesReplica::memory(root.clone());
        let mutation = replica.capture(&file, "local-device").unwrap().unwrap();
        replica.queue_for("offline-peer", &mutation).unwrap();

        std::fs::write(&file, b"second body").unwrap();
        let pending = replica.pending_for("offline-peer", 8).unwrap();
        let LocalMutation::File { source, .. } = &pending[0] else {
            panic!("queued file expected");
        };
        assert_eq!(std::fs::read(source).unwrap(), b"first body");
        assert_ne!(source, &file);
        replica.acknowledge("offline-peer", &pending[0]).unwrap();
        assert!(!source.exists());

        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ledger_digest_and_bounded_pages_converge_missing_history() {
        let source_root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-ledger-source-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let target_root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-ledger-target-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let source = FleetfilesReplica::memory(source_root.clone());
        let target = FleetfilesReplica::memory(target_root.clone());
        for counter in 1..=300_u64 {
            source
                .apply_metadata(&FleetfilesMetadata {
                    operation: format!("operation-{counter}"),
                    version: VersionStamp {
                        counter,
                        actor: "origin".into(),
                    },
                    path: format!("Desktop/folder/file-{counter:04}.txt"),
                    kind: "file".into(),
                    size: counter,
                    sha256: Some(format!("{counter:064x}")),
                    tombstone: false,
                })
                .unwrap();
        }
        assert_ne!(
            source.ledger_digest().unwrap(),
            target.ledger_digest().unwrap()
        );

        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = source.ledger_page(cursor.as_ref(), 64).unwrap();
            let wire = serde_json::to_vec(&FleetfilesMessage::LedgerPage {
                operation: "page".into(),
                entries: page.entries.clone(),
                next_cursor: page.next_cursor.clone(),
            })
            .unwrap();
            assert!(
                wire.len() < 64 * 1024,
                "ledger page must fit the data-channel envelope"
            );
            for metadata in page.entries {
                target.apply_metadata(&metadata).unwrap();
            }
            pages += 1;
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert!(pages > 1, "large ledgers must be paged");
        assert_eq!(
            source.ledger_digest().unwrap(),
            target.ledger_digest().unwrap()
        );

        drop(source);
        drop(target);
        std::fs::remove_dir_all(source_root).unwrap();
        std::fs::remove_dir_all(target_root).unwrap();
    }

    #[test]
    fn cache_only_body_hydration_needs_no_storage_allocation() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-fleetfiles-body-cache-test-{}-{}",
            std::process::id(),
            CONTENT_STAGE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let replica = FleetfilesReplica::memory(root.clone());
        let hash = format!("{:x}", Sha256::digest(b"hello fleet"));
        let version = VersionStamp {
            counter: 1,
            actor: "remote".into(),
        };
        replica
            .apply_metadata(&FleetfilesMetadata {
                operation: "cached".into(),
                version: version.clone(),
                path: "Desktop/report.txt".into(),
                kind: "file".into(),
                size: 11,
                sha256: Some(hash.clone()),
                tombstone: false,
            })
            .unwrap();
        assert!(replica
            .begin_file(
                "peer",
                "cached".into(),
                version,
                "Desktop/report.txt".into(),
                (11, hash.clone()),
                true,
            )
            .unwrap());
        replica
            .write_chunk("peer", "cached", 0, b"hello fleet")
            .unwrap();
        replica.commit_file("peer", "cached").unwrap();
        assert!(replica.has_body(&hash, 11));
        assert_eq!(
            replica
                .connection
                .lock()
                .query_row("SELECT COUNT(*) FROM content_replicas", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            0,
            "a hydrated cache body must not claim durable replica status"
        );
        replica.materialize("Desktop/report.txt").unwrap().unwrap();
        assert_eq!(
            std::fs::read(root.join("Desktop/report.txt")).unwrap(),
            b"hello fleet"
        );

        drop(replica);
        std::fs::remove_dir_all(root).unwrap();
    }
}
