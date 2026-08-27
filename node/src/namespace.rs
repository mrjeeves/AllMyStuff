//! Durable, bounded catalog and authoritative mutation log for Fleetfiles.
//!
//! Directory pages are adopted as the user visits them. This is not an index
//! of every disk. It gives native objects and their visible directory entries
//! stable fleet identities; paths remain local bindings and never enter the
//! canvas document.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_PAGE: usize = 512;
const MAX_ID: usize = 512;
const MAX_NAME: usize = 1_024;
const MAX_PATH: usize = 32_768;
const MAX_CATALOG_ENTRIES: i64 = 500_000;
const CATALOG_RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1_000;
const DEFAULT_LIST_LIMIT: usize = 128;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceObservation {
    pub provisional_id: String,
    #[serde(default)]
    pub prior_entry_id: Option<String>,
    pub source_device: String,
    pub native_id: String,
    pub name: String,
    pub native_path: String,
    pub dir: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceAdoption {
    pub provisional_id: String,
    pub entry_id: String,
    pub object_id: String,
    pub version: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceMutationRequest {
    pub operation_id: String,
    #[serde(flatten)]
    pub mutation: NamespaceMutation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NamespaceMutation {
    Create {
        parent_id: String,
        display_name: String,
        kind: String,
        #[serde(default)]
        expected_parent_version: Option<i64>,
    },
    Rename {
        entry_id: String,
        expected_version: i64,
        display_name: String,
    },
    Move {
        entry_id: String,
        expected_version: i64,
        parent_id: String,
        #[serde(default)]
        expected_parent_version: Option<i64>,
    },
    Delete {
        entry_id: String,
        expected_version: i64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceEntry {
    pub entry_id: String,
    pub object_id: String,
    pub parent_id: String,
    pub display_name: String,
    pub kind: String,
    pub hidden: bool,
    pub size: u64,
    pub modified: i64,
    pub version: i64,
    pub tombstone: bool,
    pub conflict_group: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceMutationResult {
    pub operation_id: String,
    pub sequence: i64,
    pub entry: NamespaceEntry,
    pub directory_versions: Vec<DirectoryVersion>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryVersion {
    pub parent_id: String,
    pub version: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespacePage {
    pub parent_id: String,
    pub directory_version: i64,
    pub entries: Vec<NamespaceEntry>,
    pub next_cursor: Option<String>,
}

pub struct NamespaceCatalog {
    connection: Mutex<Connection>,
}

impl NamespaceCatalog {
    pub fn load() -> Self {
        let path = allmystuff_protocol::myownmesh_state_dir()
            .map(|dir| dir.join("allmystuff-files-namespace.sqlite3"));
        match Self::open(path.as_ref()) {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::error!(
                    "Files namespace database unavailable; using memory catalog: {error}"
                );
                Self::open(None).expect("in-memory Files namespace schema must initialize")
            }
        }
    }

    fn open(path: Option<&PathBuf>) -> Result<Self, String> {
        if let Some(path) = path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("create namespace directory: {error}"))?;
            }
        }
        let connection = match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(|error| format!("open namespace database: {error}"))?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|error| format!("configure namespace database: {error}"))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS objects (
                   object_id TEXT PRIMARY KEY,
                   source_device TEXT NOT NULL,
                   native_id TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   updated_at INTEGER NOT NULL,
                   UNIQUE(source_device, native_id)
                 );
                 CREATE TABLE IF NOT EXISTS directory_entries (
                   entry_id TEXT PRIMARY KEY,
                   parent_id TEXT NOT NULL,
                   object_id TEXT NOT NULL REFERENCES objects(object_id),
                   display_name TEXT NOT NULL,
                   name_key TEXT NOT NULL,
                   kind TEXT NOT NULL,
                   hidden INTEGER NOT NULL,
                   size INTEGER NOT NULL,
                   modified INTEGER NOT NULL,
                   tombstone INTEGER NOT NULL DEFAULT 0,
                   updated_at INTEGER NOT NULL,
                   entry_version INTEGER NOT NULL DEFAULT 1,
                   origin_kind TEXT NOT NULL DEFAULT 'adopted',
                   conflict_group TEXT
                 );
                 CREATE TABLE IF NOT EXISTS native_bindings (
                   entry_id TEXT NOT NULL REFERENCES directory_entries(entry_id) ON DELETE CASCADE,
                   device_id TEXT NOT NULL,
                   native_id TEXT NOT NULL,
                   native_path TEXT NOT NULL,
                   status TEXT NOT NULL,
                   observed_at INTEGER NOT NULL,
                   PRIMARY KEY(entry_id, device_id),
                   UNIQUE(device_id, native_id, native_path)
                 );
                 CREATE TABLE IF NOT EXISTS namespace_meta (
                   key TEXT PRIMARY KEY,
                   value INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS directory_versions (
                   parent_id TEXT PRIMARY KEY,
                   version INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS namespace_mutations (
                   sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                   operation_id TEXT NOT NULL UNIQUE,
                   request_hash TEXT NOT NULL,
                   result_json TEXT NOT NULL,
                   applied_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS directory_entries_parent_name
                   ON directory_entries(parent_id, name_key, tombstone);
                 CREATE INDEX IF NOT EXISTS directory_entries_object
                   ON directory_entries(object_id);
                 CREATE INDEX IF NOT EXISTS native_bindings_identity
                   ON native_bindings(device_id, native_id);",
            )
            .map_err(|error| format!("initialize namespace database: {error}"))?;
        ensure_column(
            &connection,
            "directory_entries",
            "entry_version",
            "INTEGER NOT NULL DEFAULT 1",
        )?;
        ensure_column(
            &connection,
            "directory_entries",
            "origin_kind",
            "TEXT NOT NULL DEFAULT 'adopted'",
        )?;
        ensure_column(&connection, "directory_entries", "conflict_group", "TEXT")?;
        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS directory_entries_conflicts
                   ON directory_entries(parent_id, name_key, conflict_group, tombstone);
                 CREATE INDEX IF NOT EXISTS namespace_mutations_applied
                   ON namespace_mutations(applied_at);",
            )
            .map_err(|error| format!("index namespace authority: {error}"))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn adopt_page(
        &self,
        parent_id: &str,
        observations: Vec<NamespaceObservation>,
    ) -> Result<Vec<NamespaceAdoption>, String> {
        valid_field("parent id", parent_id, MAX_ID)?;
        if observations.len() > MAX_PAGE {
            return Err(format!(
                "a namespace page may contain at most {MAX_PAGE} entries"
            ));
        }
        for observation in &observations {
            valid_observation(observation)?;
        }

        let mut connection = self.connection.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| format!("begin namespace adoption: {error}"))?;
        let now = unix_millis();
        let mut adopted = Vec::with_capacity(observations.len());
        for observation in observations {
            adopted.push(adopt_one(&transaction, parent_id, observation, now)?);
        }
        maybe_collect_catalog(&transaction, now)?;
        transaction
            .commit()
            .map_err(|error| format!("commit namespace adoption: {error}"))?;
        Ok(adopted)
    }

    pub fn apply_mutation(
        &self,
        request: NamespaceMutationRequest,
    ) -> Result<NamespaceMutationResult, String> {
        valid_field("operation id", &request.operation_id, MAX_ID)?;
        let request_hash = hash_request(&request)?;
        let mut connection = self.connection.lock();
        let transaction = connection
            .transaction()
            .map_err(|error| format!("begin namespace mutation: {error}"))?;

        let prior: Option<(String, String)> = transaction
            .query_row(
                "SELECT request_hash, result_json FROM namespace_mutations
                 WHERE operation_id = ?1",
                params![request.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("read namespace mutation: {error}"))?;
        if let Some((prior_hash, result_json)) = prior {
            if prior_hash != request_hash {
                return Err(
                    "operation id was already used for a different namespace mutation".into(),
                );
            }
            return serde_json::from_str(&result_json)
                .map_err(|error| format!("decode namespace mutation result: {error}"));
        }

        let now = unix_millis();
        let (entry, directory_versions) = apply_namespace_mutation(&transaction, &request, now)?;
        transaction
            .execute(
                "INSERT INTO namespace_mutations(
                   operation_id, request_hash, result_json, applied_at
                 ) VALUES (?1, ?2, '', ?3)",
                params![request.operation_id, request_hash, now],
            )
            .map_err(|error| format!("record namespace mutation: {error}"))?;
        let sequence = transaction.last_insert_rowid();
        let result = NamespaceMutationResult {
            operation_id: request.operation_id,
            sequence,
            entry,
            directory_versions,
        };
        let result_json = serde_json::to_string(&result)
            .map_err(|error| format!("encode namespace mutation result: {error}"))?;
        transaction
            .execute(
                "UPDATE namespace_mutations SET result_json = ?2 WHERE operation_id = ?1",
                params![result.operation_id, result_json],
            )
            .map_err(|error| format!("finish namespace mutation: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("commit namespace mutation: {error}"))?;
        Ok(result)
    }

    pub fn list_page(
        &self,
        parent_id: &str,
        cursor: Option<&str>,
        limit: usize,
        expected_directory_version: Option<i64>,
    ) -> Result<NamespacePage, String> {
        valid_field("parent id", parent_id, MAX_ID)?;
        if let Some(cursor) = cursor {
            valid_field("cursor", cursor, MAX_ID)?;
        }
        let limit = if limit == 0 {
            DEFAULT_LIST_LIMIT
        } else {
            limit.min(MAX_PAGE)
        };
        let fetch_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(MAX_PAGE as i64);
        let connection = self.connection.lock();
        let directory_version = read_directory_version(&connection, parent_id)?;
        if let Some(expected) = expected_directory_version {
            if expected != directory_version {
                return Err(format!(
                    "directory changed while paging (expected version {expected}, current {directory_version})"
                ));
            }
        }

        let mut entries = if let Some(cursor) = cursor {
            let after_name: String = connection
                .query_row(
                    "SELECT name_key FROM directory_entries
                     WHERE entry_id = ?1 AND parent_id = ?2",
                    params![cursor, parent_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| format!("read namespace cursor: {error}"))?
                .ok_or("namespace cursor expired")?;
            let mut statement = connection
                .prepare(
                    "SELECT entry_id, object_id, parent_id, display_name, kind,
                            hidden, size, modified, entry_version, tombstone, conflict_group
                     FROM directory_entries
                     WHERE parent_id = ?1 AND tombstone = 0
                       AND (name_key > ?2 OR (name_key = ?2 AND entry_id > ?3))
                     ORDER BY name_key, entry_id
                     LIMIT ?4",
                )
                .map_err(|error| format!("prepare namespace page: {error}"))?;
            let entries = statement
                .query_map(
                    params![parent_id, after_name, cursor, fetch_limit],
                    row_to_entry,
                )
                .map_err(|error| format!("list namespace page: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("decode namespace page: {error}"))?;
            entries
        } else {
            let mut statement = connection
                .prepare(
                    "SELECT entry_id, object_id, parent_id, display_name, kind,
                            hidden, size, modified, entry_version, tombstone, conflict_group
                     FROM directory_entries
                     WHERE parent_id = ?1 AND tombstone = 0
                     ORDER BY name_key, entry_id
                     LIMIT ?2",
                )
                .map_err(|error| format!("prepare namespace page: {error}"))?;
            let entries = statement
                .query_map(params![parent_id, fetch_limit], row_to_entry)
                .map_err(|error| format!("list namespace page: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("decode namespace page: {error}"))?;
            entries
        };
        let next_cursor = (entries.len() > limit).then(|| entries[limit - 1].entry_id.clone());
        entries.truncate(limit);
        Ok(NamespacePage {
            parent_id: parent_id.to_string(),
            directory_version,
            entries,
            next_cursor,
        })
    }

    #[cfg(test)]
    fn memory() -> Self {
        Self::open(None).unwrap()
    }
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), String> {
    let exists: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |row| row.get(0),
        )
        .map_err(|error| format!("inspect namespace schema: {error}"))?;
    if exists == 0 {
        connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"),
                [],
            )
            .map_err(|error| format!("migrate namespace schema: {error}"))?;
    }
    Ok(())
}

fn hash_request(request: &NamespaceMutationRequest) -> Result<String, String> {
    let encoded = serde_json::to_vec(request)
        .map_err(|error| format!("encode namespace mutation: {error}"))?;
    let digest = Sha256::digest(encoded);
    let mut value = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(value, "{byte:02x}");
    }
    Ok(value)
}

fn apply_namespace_mutation(
    transaction: &Transaction<'_>,
    request: &NamespaceMutationRequest,
    now: i64,
) -> Result<(NamespaceEntry, Vec<DirectoryVersion>), String> {
    match &request.mutation {
        NamespaceMutation::Create {
            parent_id,
            display_name,
            kind,
            expected_parent_version,
        } => {
            valid_field("parent id", parent_id, MAX_ID)?;
            valid_display_name(display_name)?;
            valid_kind(kind)?;
            check_directory_version(transaction, parent_id, *expected_parent_version)?;
            let object_id = deterministic_id("fleet-object", &[&request.operation_id]);
            let entry_id = deterministic_id("fleet-entry", &[&request.operation_id]);
            let name_key = display_name.to_lowercase();
            transaction
                .execute(
                    "INSERT INTO objects(object_id, source_device, native_id, kind, updated_at)
                     VALUES (?1, 'fleet', ?1, ?2, ?3)",
                    params![object_id, kind, now],
                )
                .map_err(|error| format!("create namespace object: {error}"))?;
            transaction
                .execute(
                    "INSERT INTO directory_entries(
                       entry_id, parent_id, object_id, display_name, name_key, kind,
                       hidden, size, modified, tombstone, updated_at, entry_version,
                       origin_kind, conflict_group
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7, 0, ?7, 1, 'fleet', NULL)",
                    params![
                        entry_id,
                        parent_id,
                        object_id,
                        display_name,
                        name_key,
                        kind,
                        now
                    ],
                )
                .map_err(|error| format!("create namespace entry: {error}"))?;
            let version = bump_directory_version(transaction, parent_id)?;
            refresh_conflict_group(transaction, parent_id, &name_key)?;
            Ok((
                load_entry(transaction, &entry_id, true)?,
                vec![DirectoryVersion {
                    parent_id: parent_id.clone(),
                    version,
                }],
            ))
        }
        NamespaceMutation::Rename {
            entry_id,
            expected_version,
            display_name,
        } => {
            valid_field("entry id", entry_id, MAX_ID)?;
            valid_display_name(display_name)?;
            let current = load_entry(transaction, entry_id, false)?;
            check_entry_version(&current, *expected_version)?;
            let old_key = current.display_name.to_lowercase();
            let new_key = display_name.to_lowercase();
            transaction
                .execute(
                    "UPDATE directory_entries
                     SET display_name = ?2, name_key = ?3, entry_version = entry_version + 1,
                         modified = ?4, updated_at = ?4
                     WHERE entry_id = ?1 AND tombstone = 0",
                    params![entry_id, display_name, new_key, now],
                )
                .map_err(|error| format!("rename namespace entry: {error}"))?;
            let version = bump_directory_version(transaction, &current.parent_id)?;
            refresh_conflict_group(transaction, &current.parent_id, &old_key)?;
            if old_key != new_key {
                refresh_conflict_group(transaction, &current.parent_id, &new_key)?;
            }
            Ok((
                load_entry(transaction, entry_id, false)?,
                vec![DirectoryVersion {
                    parent_id: current.parent_id,
                    version,
                }],
            ))
        }
        NamespaceMutation::Move {
            entry_id,
            expected_version,
            parent_id,
            expected_parent_version,
        } => {
            valid_field("entry id", entry_id, MAX_ID)?;
            valid_field("parent id", parent_id, MAX_ID)?;
            let current = load_entry(transaction, entry_id, false)?;
            check_entry_version(&current, *expected_version)?;
            check_directory_version(transaction, parent_id, *expected_parent_version)?;
            if current.kind == "directory" {
                ensure_no_cycle(transaction, entry_id, parent_id)?;
            }
            if current.parent_id == *parent_id {
                return Ok((
                    current,
                    vec![DirectoryVersion {
                        parent_id: parent_id.clone(),
                        version: read_directory_version(transaction, parent_id)?,
                    }],
                ));
            }
            let name_key = current.display_name.to_lowercase();
            transaction
                .execute(
                    "UPDATE directory_entries
                     SET parent_id = ?2, entry_version = entry_version + 1,
                         modified = ?3, updated_at = ?3
                     WHERE entry_id = ?1 AND tombstone = 0",
                    params![entry_id, parent_id, now],
                )
                .map_err(|error| format!("move namespace entry: {error}"))?;
            let old_version = bump_directory_version(transaction, &current.parent_id)?;
            let new_version = bump_directory_version(transaction, parent_id)?;
            refresh_conflict_group(transaction, &current.parent_id, &name_key)?;
            refresh_conflict_group(transaction, parent_id, &name_key)?;
            Ok((
                load_entry(transaction, entry_id, false)?,
                vec![
                    DirectoryVersion {
                        parent_id: current.parent_id,
                        version: old_version,
                    },
                    DirectoryVersion {
                        parent_id: parent_id.clone(),
                        version: new_version,
                    },
                ],
            ))
        }
        NamespaceMutation::Delete {
            entry_id,
            expected_version,
        } => {
            valid_field("entry id", entry_id, MAX_ID)?;
            let current = load_entry(transaction, entry_id, false)?;
            check_entry_version(&current, *expected_version)?;
            let name_key = current.display_name.to_lowercase();
            transaction
                .execute(
                    "UPDATE directory_entries
                     SET tombstone = 1, conflict_group = NULL,
                         entry_version = entry_version + 1, modified = ?2, updated_at = ?2
                     WHERE entry_id = ?1 AND tombstone = 0",
                    params![entry_id, now],
                )
                .map_err(|error| format!("delete namespace entry: {error}"))?;
            let version = bump_directory_version(transaction, &current.parent_id)?;
            refresh_conflict_group(transaction, &current.parent_id, &name_key)?;
            Ok((
                load_entry(transaction, entry_id, true)?,
                vec![DirectoryVersion {
                    parent_id: current.parent_id,
                    version,
                }],
            ))
        }
    }
}

fn valid_display_name(name: &str) -> Result<(), String> {
    valid_field("display name", name, MAX_NAME)?;
    if name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err("invalid display name".into());
    }
    Ok(())
}

fn valid_kind(kind: &str) -> Result<(), String> {
    if matches!(kind, "file" | "directory") {
        Ok(())
    } else {
        Err("namespace kind must be file or directory".into())
    }
}

fn check_entry_version(entry: &NamespaceEntry, expected: i64) -> Result<(), String> {
    if entry.version == expected {
        Ok(())
    } else {
        Err(format!(
            "stale namespace entry version (expected {expected}, current {})",
            entry.version
        ))
    }
}

fn check_directory_version(
    connection: &Connection,
    parent_id: &str,
    expected: Option<i64>,
) -> Result<(), String> {
    let current = read_directory_version(connection, parent_id)?;
    if expected.is_some_and(|expected| expected != current) {
        Err(format!(
            "stale directory version (expected {}, current {current})",
            expected.unwrap_or_default()
        ))
    } else {
        Ok(())
    }
}

fn read_directory_version(connection: &Connection, parent_id: &str) -> Result<i64, String> {
    connection
        .query_row(
            "SELECT version FROM directory_versions WHERE parent_id = ?1",
            params![parent_id],
            |row| row.get(0),
        )
        .optional()
        .map(|version| version.unwrap_or(0))
        .map_err(|error| format!("read directory version: {error}"))
}

fn bump_directory_version(transaction: &Transaction<'_>, parent_id: &str) -> Result<i64, String> {
    transaction
        .execute(
            "INSERT INTO directory_versions(parent_id, version) VALUES (?1, 1)
             ON CONFLICT(parent_id) DO UPDATE SET version = version + 1",
            params![parent_id],
        )
        .map_err(|error| format!("advance directory version: {error}"))?;
    read_directory_version(transaction, parent_id)
}

fn refresh_conflict_group(
    transaction: &Transaction<'_>,
    parent_id: &str,
    name_key: &str,
) -> Result<(), String> {
    let count: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM directory_entries
             WHERE parent_id = ?1 AND name_key = ?2 AND tombstone = 0",
            params![parent_id, name_key],
            |row| row.get(0),
        )
        .map_err(|error| format!("count namespace conflicts: {error}"))?;
    let group = (count > 1).then(|| deterministic_id("conflict", &[parent_id, name_key]));
    transaction
        .execute(
            "UPDATE directory_entries SET conflict_group = ?3
             WHERE parent_id = ?1 AND name_key = ?2 AND tombstone = 0",
            params![parent_id, name_key, group],
        )
        .map_err(|error| format!("mark namespace conflicts: {error}"))?;
    Ok(())
}

fn ensure_no_cycle(connection: &Connection, entry_id: &str, parent_id: &str) -> Result<(), String> {
    let mut cursor = parent_id.to_string();
    for _ in 0..1_024 {
        if cursor == entry_id {
            return Err("a directory cannot be moved inside itself".into());
        }
        let next: Option<String> = connection
            .query_row(
                "SELECT parent_id FROM directory_entries
                 WHERE entry_id = ?1 AND tombstone = 0",
                params![cursor],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("validate namespace ancestry: {error}"))?;
        match next {
            Some(parent) if parent != cursor => cursor = parent,
            _ => return Ok(()),
        }
    }
    Err("namespace ancestry exceeds the safety bound".into())
}

fn load_entry(
    connection: &Connection,
    entry_id: &str,
    include_tombstone: bool,
) -> Result<NamespaceEntry, String> {
    connection
        .query_row(
            "SELECT entry_id, object_id, parent_id, display_name, kind,
                    hidden, size, modified, entry_version, tombstone, conflict_group
             FROM directory_entries
             WHERE entry_id = ?1 AND (?2 OR tombstone = 0)",
            params![entry_id, include_tombstone],
            row_to_entry,
        )
        .optional()
        .map_err(|error| format!("read namespace entry: {error}"))?
        .ok_or_else(|| "namespace entry is unavailable".into())
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<NamespaceEntry> {
    let size: i64 = row.get(6)?;
    Ok(NamespaceEntry {
        entry_id: row.get(0)?,
        object_id: row.get(1)?,
        parent_id: row.get(2)?,
        display_name: row.get(3)?,
        kind: row.get(4)?,
        hidden: row.get(5)?,
        size: u64::try_from(size).unwrap_or_default(),
        modified: row.get(7)?,
        version: row.get(8)?,
        tombstone: row.get(9)?,
        conflict_group: row.get(10)?,
    })
}

fn adopt_one(
    transaction: &Transaction<'_>,
    parent_id: &str,
    observation: NamespaceObservation,
    now: i64,
) -> Result<NamespaceAdoption, String> {
    let kind = if observation.dir { "directory" } else { "file" };
    let expected_object_id = deterministic_id(
        "object",
        &[&observation.source_device, &observation.native_id],
    );
    transaction
        .execute(
            "INSERT INTO objects(object_id, source_device, native_id, kind, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_device, native_id) DO UPDATE SET
               kind = excluded.kind, updated_at = excluded.updated_at",
            params![
                expected_object_id,
                observation.source_device,
                observation.native_id,
                kind,
                now
            ],
        )
        .map_err(|error| format!("adopt namespace object: {error}"))?;
    let observed_object_id: String = transaction
        .query_row(
            "SELECT object_id FROM objects
             WHERE source_device = ?1 AND native_id = ?2",
            params![observation.source_device, observation.native_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read namespace object: {error}"))?;

    let existing: Option<(String, String)> = transaction
        .query_row(
            "SELECT directory_entries.entry_id, directory_entries.object_id
             FROM native_bindings
             JOIN directory_entries USING(entry_id)
             WHERE device_id = ?1 AND native_id = ?2 AND native_path = ?3",
            params![
                observation.source_device,
                observation.native_id,
                observation.native_path
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| format!("read namespace binding: {error}"))?;
    let rebound: Option<(String, String)> = match observation.prior_entry_id.as_deref() {
        Some(prior) => transaction
            .query_row(
                "SELECT entry_id, object_id FROM directory_entries
                 WHERE entry_id = ?1 AND tombstone = 0",
                params![prior],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("validate namespace rebind: {error}"))?,
        None => None,
    };
    let (entry_id, object_id) = existing.or(rebound).unwrap_or_else(|| {
        (
            deterministic_id(
                "entry",
                &[
                    parent_id,
                    &observation.source_device,
                    &observation.native_id,
                    &observation.native_path,
                ],
            ),
            observed_object_id,
        )
    });
    let name_key = observation.name.to_lowercase();
    transaction
        .execute(
            "INSERT INTO directory_entries(
               entry_id, parent_id, object_id, display_name, name_key, kind,
               hidden, size, modified, tombstone, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)
             ON CONFLICT(entry_id) DO UPDATE SET
               parent_id = excluded.parent_id,
               object_id = excluded.object_id,
               display_name = excluded.display_name,
               name_key = excluded.name_key,
               kind = excluded.kind,
               hidden = excluded.hidden,
               size = excluded.size,
               modified = excluded.modified,
               tombstone = 0,
               updated_at = excluded.updated_at",
            params![
                entry_id,
                parent_id,
                object_id,
                observation.name,
                name_key,
                kind,
                observation.hidden,
                i64::try_from(observation.size).unwrap_or(i64::MAX),
                observation.modified,
                now
            ],
        )
        .map_err(|error| format!("adopt namespace entry: {error}"))?;
    transaction
        .execute(
            "INSERT INTO native_bindings(
               entry_id, device_id, native_id, native_path, status, observed_at
             ) VALUES (?1, ?2, ?3, ?4, 'present', ?5)
             ON CONFLICT(entry_id, device_id) DO UPDATE SET
               native_id = excluded.native_id,
               native_path = excluded.native_path,
               status = 'present',
               observed_at = excluded.observed_at",
            params![
                entry_id,
                observation.source_device,
                observation.native_id,
                observation.native_path,
                now
            ],
        )
        .map_err(|error| format!("adopt native binding: {error}"))?;
    let version: i64 = transaction
        .query_row(
            "SELECT entry_version FROM directory_entries WHERE entry_id = ?1",
            params![entry_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read adopted namespace version: {error}"))?;

    Ok(NamespaceAdoption {
        provisional_id: observation.provisional_id,
        entry_id,
        object_id,
        version,
    })
}

fn maybe_collect_catalog(transaction: &Transaction<'_>, now: i64) -> Result<(), String> {
    const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
    let last: Option<i64> = transaction
        .query_row(
            "SELECT value FROM namespace_meta WHERE key = 'last_gc'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("read namespace maintenance state: {error}"))?;
    if last.is_some_and(|last| now.saturating_sub(last) < DAY_MS) {
        return Ok(());
    }

    let cutoff = now.saturating_sub(CATALOG_RETENTION_MS);
    transaction
        .execute(
            "DELETE FROM native_bindings WHERE observed_at < ?1",
            params![cutoff],
        )
        .map_err(|error| format!("expire namespace bindings: {error}"))?;

    let count: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM directory_entries WHERE origin_kind = 'adopted'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("count namespace entries: {error}"))?;
    let overflow = count.saturating_sub(MAX_CATALOG_ENTRIES);
    if overflow > 0 {
        transaction
            .execute(
                "DELETE FROM native_bindings
                 WHERE rowid IN (
                   SELECT rowid FROM native_bindings
                   ORDER BY observed_at ASC
                   LIMIT ?1
                 )",
                params![overflow],
            )
            .map_err(|error| format!("bound namespace bindings: {error}"))?;
    }

    transaction
        .execute(
            "DELETE FROM directory_entries
             WHERE origin_kind = 'adopted' AND NOT EXISTS (
               SELECT 1 FROM native_bindings
               WHERE native_bindings.entry_id = directory_entries.entry_id
             )",
            [],
        )
        .map_err(|error| format!("collect namespace entries: {error}"))?;
    transaction
        .execute(
            "DELETE FROM objects
             WHERE source_device != 'fleet' AND NOT EXISTS (
               SELECT 1 FROM directory_entries
               WHERE directory_entries.object_id = objects.object_id
             )",
            [],
        )
        .map_err(|error| format!("collect namespace objects: {error}"))?;
    transaction
        .execute(
            "INSERT INTO namespace_meta(key, value) VALUES ('last_gc', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now],
        )
        .map_err(|error| format!("record namespace maintenance: {error}"))?;
    Ok(())
}

fn deterministic_id(namespace: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut value = String::with_capacity(38);
    value.push_str(namespace);
    value.push(':');
    for byte in &digest[..16] {
        use std::fmt::Write;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn valid_observation(observation: &NamespaceObservation) -> Result<(), String> {
    valid_field("provisional id", &observation.provisional_id, MAX_ID)?;
    if let Some(prior) = &observation.prior_entry_id {
        valid_field("prior entry id", prior, MAX_ID)?;
    }
    valid_field("source device", &observation.source_device, MAX_ID)?;
    valid_field("native id", &observation.native_id, MAX_PATH)?;
    valid_field("name", &observation.name, MAX_NAME)?;
    valid_field("native path", &observation.native_path, MAX_PATH)
}

fn valid_field(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        Err(format!("invalid {label}"))
    } else {
        Ok(())
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(
        provisional: &str,
        device: &str,
        native: &str,
        path: &str,
        name: &str,
    ) -> NamespaceObservation {
        NamespaceObservation {
            provisional_id: provisional.into(),
            prior_entry_id: None,
            source_device: device.into(),
            native_id: native.into(),
            name: name.into(),
            native_path: path.into(),
            dir: false,
            hidden: false,
            size: 4,
            modified: 1,
        }
    }

    fn create(
        operation_id: &str,
        parent_id: &str,
        display_name: &str,
        expected_parent_version: Option<i64>,
    ) -> NamespaceMutationRequest {
        NamespaceMutationRequest {
            operation_id: operation_id.into(),
            mutation: NamespaceMutation::Create {
                parent_id: parent_id.into(),
                display_name: display_name.into(),
                kind: "directory".into(),
                expected_parent_version,
            },
        }
    }

    #[test]
    fn exact_binding_reuses_entry_and_object() {
        let catalog = NamespaceCatalog::memory();
        let first = catalog
            .adopt_page(
                "fleet:home",
                vec![observed("a", "pc", "vol:1", "C:/a", "a")],
            )
            .unwrap();
        let second = catalog
            .adopt_page(
                "fleet:home",
                vec![observed("b", "pc", "vol:1", "C:/a", "renamed")],
            )
            .unwrap();
        assert_eq!(first[0].entry_id, second[0].entry_id);
        assert_eq!(first[0].object_id, second[0].object_id);
    }

    #[test]
    fn explicit_rename_rebind_preserves_entry() {
        let catalog = NamespaceCatalog::memory();
        let first = catalog
            .adopt_page(
                "fleet:home",
                vec![observed("a", "pc", "vol:1", "C:/a", "a")],
            )
            .unwrap();
        let mut renamed = observed("b", "pc", "vol:1", "C:/renamed", "renamed");
        renamed.prior_entry_id = Some(first[0].entry_id.clone());
        let second = catalog.adopt_page("fleet:home", vec![renamed]).unwrap();
        assert_eq!(first[0].entry_id, second[0].entry_id);
    }

    #[test]
    fn hardlinks_are_distinct_entries_for_one_object() {
        let catalog = NamespaceCatalog::memory();
        let rows = catalog
            .adopt_page(
                "fleet:home",
                vec![
                    observed("a", "pc", "vol:1", "C:/a", "a"),
                    observed("b", "pc", "vol:1", "C:/b", "b"),
                ],
            )
            .unwrap();
        assert_ne!(rows[0].entry_id, rows[1].entry_id);
        assert_eq!(rows[0].object_id, rows[1].object_id);
    }

    #[test]
    fn equal_names_on_different_devices_do_not_collapse() {
        let catalog = NamespaceCatalog::memory();
        let rows = catalog
            .adopt_page(
                "fleet:home",
                vec![
                    observed("a", "pc-a", "1", "Desktop/report", "report"),
                    observed("b", "pc-b", "1", "Desktop/report", "report"),
                ],
            )
            .unwrap();
        assert_ne!(rows[0].entry_id, rows[1].entry_id);
        assert_ne!(rows[0].object_id, rows[1].object_id);
    }

    #[test]
    fn adoption_is_bounded() {
        let catalog = NamespaceCatalog::memory();
        let rows = (0..=MAX_PAGE)
            .map(|i| {
                observed(
                    &format!("p{i}"),
                    "pc",
                    &format!("n{i}"),
                    &format!("C:/{i}"),
                    &i.to_string(),
                )
            })
            .collect();
        assert!(catalog
            .adopt_page("fleet:home", rows)
            .unwrap_err()
            .contains("at most"));
    }
    #[test]
    fn authoritative_create_is_idempotent_and_operation_ids_cannot_be_reused() {
        let catalog = NamespaceCatalog::memory();
        let request = create("create-one", "fleet:home", "Documents", Some(0));
        let first = catalog.apply_mutation(request.clone()).unwrap();
        let replay = catalog.apply_mutation(request).unwrap();
        assert_eq!(first, replay);

        let collision = create("create-one", "fleet:home", "Different", Some(1));
        assert!(catalog
            .apply_mutation(collision)
            .unwrap_err()
            .contains("already used"));
    }

    #[test]
    fn stale_entry_versions_are_rejected() {
        let catalog = NamespaceCatalog::memory();
        let created = catalog
            .apply_mutation(create("create-stale", "fleet:home", "Draft", Some(0)))
            .unwrap();
        let renamed = catalog
            .apply_mutation(NamespaceMutationRequest {
                operation_id: "rename-current".into(),
                mutation: NamespaceMutation::Rename {
                    entry_id: created.entry.entry_id.clone(),
                    expected_version: 1,
                    display_name: "Final".into(),
                },
            })
            .unwrap();
        assert_eq!(renamed.entry.version, 2);

        let stale = catalog
            .apply_mutation(NamespaceMutationRequest {
                operation_id: "rename-stale".into(),
                mutation: NamespaceMutation::Rename {
                    entry_id: created.entry.entry_id,
                    expected_version: 1,
                    display_name: "Lost edit".into(),
                },
            })
            .unwrap_err();
        assert!(stale.contains("stale namespace entry version"));
    }

    #[test]
    fn equal_names_remain_distinct_and_are_marked_as_conflicts() {
        let catalog = NamespaceCatalog::memory();
        catalog
            .apply_mutation(create("conflict-a", "fleet:home", "Report", Some(0)))
            .unwrap();
        catalog
            .apply_mutation(create("conflict-b", "fleet:home", "Report", Some(1)))
            .unwrap();

        let page = catalog.list_page("fleet:home", None, 10, Some(2)).unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_ne!(page.entries[0].entry_id, page.entries[1].entry_id);
        assert!(page.entries[0].conflict_group.is_some());
        assert_eq!(
            page.entries[0].conflict_group,
            page.entries[1].conflict_group
        );
    }

    #[test]
    fn pagination_is_bounded_and_detects_mid_scan_changes() {
        let catalog = NamespaceCatalog::memory();
        for (index, name) in ["Alpha", "Beta", "Gamma"].into_iter().enumerate() {
            catalog
                .apply_mutation(create(
                    &format!("page-{index}"),
                    "fleet:home",
                    name,
                    Some(index as i64),
                ))
                .unwrap();
        }

        let first = catalog.list_page("fleet:home", None, 2, Some(3)).unwrap();
        assert_eq!(first.entries.len(), 2);
        let cursor = first.next_cursor.clone().unwrap();
        let second = catalog
            .list_page("fleet:home", Some(&cursor), 2, Some(3))
            .unwrap();
        assert_eq!(second.entries.len(), 1);

        catalog
            .apply_mutation(create("page-later", "fleet:home", "Delta", Some(3)))
            .unwrap();
        assert!(catalog
            .list_page("fleet:home", Some(&cursor), 2, Some(3))
            .unwrap_err()
            .contains("directory changed"));
    }

    #[test]
    fn directory_moves_cannot_create_cycles() {
        let catalog = NamespaceCatalog::memory();
        let parent = catalog
            .apply_mutation(create("cycle-parent", "fleet:home", "Parent", Some(0)))
            .unwrap();
        let child = catalog
            .apply_mutation(create(
                "cycle-child",
                &parent.entry.entry_id,
                "Child",
                Some(0),
            ))
            .unwrap();
        let error = catalog
            .apply_mutation(NamespaceMutationRequest {
                operation_id: "cycle-move".into(),
                mutation: NamespaceMutation::Move {
                    entry_id: parent.entry.entry_id,
                    expected_version: 1,
                    parent_id: child.entry.entry_id,
                    expected_parent_version: Some(0),
                },
            })
            .unwrap_err();
        assert!(error.contains("inside itself"));
    }

    #[test]
    fn stable_identity_and_idempotency_survive_restart() {
        let path = std::env::temp_dir().join(format!(
            "allmystuff-namespace-{}-{}.sqlite3",
            std::process::id(),
            unix_millis()
        ));
        let request = create("restart-create", "fleet:home", "Persistent", Some(0));
        let first = {
            let catalog = NamespaceCatalog::open(Some(&path)).unwrap();
            catalog.apply_mutation(request.clone()).unwrap()
        };
        let replay = {
            let catalog = NamespaceCatalog::open(Some(&path)).unwrap();
            let replay = catalog.apply_mutation(request).unwrap();
            let page = catalog.list_page("fleet:home", None, 10, Some(1)).unwrap();
            assert_eq!(page.entries, vec![first.entry.clone()]);
            replay
        };
        assert_eq!(first, replay);

        for suffix in ["", "-wal", "-shm"] {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(candidate));
        }
    }
}
