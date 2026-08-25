//! Durable, bounded catalog for the fleet filesystem namespace.
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
            .busy_timeout(Duration::from_secs(2))
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
                   updated_at INTEGER NOT NULL
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
                 CREATE INDEX IF NOT EXISTS directory_entries_parent_name
                   ON directory_entries(parent_id, name_key, tombstone);
                 CREATE INDEX IF NOT EXISTS directory_entries_object
                   ON directory_entries(object_id);
                 CREATE INDEX IF NOT EXISTS native_bindings_identity
                   ON native_bindings(device_id, native_id);",
            )
            .map_err(|error| format!("initialize namespace database: {error}"))?;
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

    #[cfg(test)]
    fn memory() -> Self {
        Self::open(None).unwrap()
    }
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
    let object_id: String = transaction
        .query_row(
            "SELECT object_id FROM objects
             WHERE source_device = ?1 AND native_id = ?2",
            params![observation.source_device, observation.native_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read namespace object: {error}"))?;

    let existing: Option<String> = transaction
        .query_row(
            "SELECT entry_id FROM native_bindings
             WHERE device_id = ?1 AND native_id = ?2 AND native_path = ?3",
            params![
                observation.source_device,
                observation.native_id,
                observation.native_path
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("read namespace binding: {error}"))?;
    let rebound = match observation.prior_entry_id.as_deref() {
        Some(prior) => transaction
            .query_row(
                "SELECT entry_id FROM directory_entries
                 WHERE entry_id = ?1 AND object_id = ?2",
                params![prior, object_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("validate namespace rebind: {error}"))?,
        None => None,
    };
    let entry_id = existing.or(rebound).unwrap_or_else(|| {
        deterministic_id(
            "entry",
            &[
                parent_id,
                &observation.source_device,
                &observation.native_id,
                &observation.native_path,
            ],
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

    Ok(NamespaceAdoption {
        provisional_id: observation.provisional_id,
        entry_id,
        object_id,
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
        .query_row("SELECT COUNT(*) FROM directory_entries", [], |row| row.get(0))
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
             WHERE NOT EXISTS (
               SELECT 1 FROM native_bindings
               WHERE native_bindings.entry_id = directory_entries.entry_id
             )",
            [],
        )
        .map_err(|error| format!("collect namespace entries: {error}"))?;
    transaction
        .execute(
            "DELETE FROM objects
             WHERE NOT EXISTS (
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
}
