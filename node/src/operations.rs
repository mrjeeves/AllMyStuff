//! Durable, bounded operation history and idempotency state for Fleetfiles.
//!
//! The operation store is the shared seam for UI requests, mount adapters,
//! background workers, and recovery. Worker attempts are deliberately not the
//! operation identity: retrying work must never publish a second result.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const VISIBLE_OPERATIONS: i64 = 200;
const RETAIN_SUCCESSES: i64 = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPhase {
    Scanning,
    AwaitingApproval,
    Staging,
    Transferring,
    Verifying,
    Committing,
    Materializing,
    Cancelling,
    Compensating,
    Complete,
    Failed,
    Cancelled,
}

impl OperationPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scanning => "scanning",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Staging => "staging",
            Self::Transferring => "transferring",
            Self::Verifying => "verifying",
            Self::Committing => "committing",
            Self::Materializing => "materializing",
            Self::Cancelling => "cancelling",
            Self::Compensating => "compensating",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "scanning" => Self::Scanning,
            "awaiting-approval" => Self::AwaitingApproval,
            "staging" => Self::Staging,
            "transferring" => Self::Transferring,
            "verifying" => Self::Verifying,
            "committing" => Self::Committing,
            "materializing" => Self::Materializing,
            "cancelling" => Self::Cancelling,
            "compensating" => Self::Compensating,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    fn permits(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        match self {
            Self::Scanning => matches!(
                next,
                Self::AwaitingApproval | Self::Cancelled | Self::Failed
            ),
            Self::AwaitingApproval => {
                matches!(next, Self::Staging | Self::Cancelled | Self::Failed)
            }
            Self::Staging => {
                matches!(next, Self::Transferring | Self::Cancelling | Self::Failed)
            }
            Self::Transferring => matches!(
                next,
                Self::Verifying | Self::Committing | Self::Cancelling | Self::Failed
            ),
            Self::Verifying => {
                matches!(next, Self::Committing | Self::Cancelling | Self::Failed)
            }
            Self::Committing => matches!(
                next,
                Self::Materializing | Self::Compensating | Self::Complete | Self::Failed
            ),
            Self::Materializing => {
                matches!(next, Self::Complete | Self::Compensating | Self::Failed)
            }
            Self::Cancelling => {
                matches!(next, Self::Cancelled | Self::Compensating | Self::Failed)
            }
            Self::Compensating => matches!(next, Self::Cancelled | Self::Failed),
            Self::Complete | Self::Failed | Self::Cancelled => false,
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub id: String,
    pub operation_type: String,
    pub idempotency_scope: String,
    pub intent: String,
    pub phase: String,
    pub target_label: String,
    pub object_ids: Vec<String>,
    pub preconditions: serde_json::Value,
    pub policy: Option<serde_json::Value>,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub progress_bytes: u64,
    pub error: Option<String>,
    pub retry_condition: Option<String>,
    pub cancellation_requested: bool,
    pub verification_result: Option<String>,
    pub residue: Option<String>,
    pub started_at: u64,
    pub updated_at: u64,
}

impl OperationRecord {
    pub fn file_transfer(
        id: String,
        target_label: String,
        files: u64,
        folders: u64,
        bytes: u64,
        started_at: u64,
    ) -> Self {
        Self {
            idempotency_scope: format!("file-transfer:{id}"),
            intent: format!("Send selected files to {target_label}"),
            id,
            operation_type: "file-transfer".into(),
            phase: OperationPhase::Staging.as_str().into(),
            target_label,
            object_ids: Vec::new(),
            preconditions: serde_json::json!({
                "expectedFiles": files,
                "expectedFolders": folders,
                "expectedBytes": bytes,
            }),
            policy: None,
            files,
            folders,
            bytes,
            progress_bytes: 0,
            error: None,
            retry_condition: None,
            cancellation_requested: false,
            verification_result: None,
            residue: None,
            started_at,
            updated_at: started_at,
        }
    }
}

pub struct OperationsStore {
    connection: Mutex<Connection>,
}

impl OperationsStore {
    pub fn load() -> Self {
        let path = allmystuff_protocol::myownmesh_state_dir()
            .map(|dir| dir.join("allmystuff-operations.sqlite3"));
        match Self::open(path.as_ref()) {
            Ok(store) => store,
            Err(error) => {
                tracing::error!("operation database unavailable; using memory history: {error}");
                Self::open(None).expect("in-memory operation schema must initialize")
            }
        }
    }

    fn open(path: Option<&PathBuf>) -> Result<Self, String> {
        if let Some(path) = path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("create operation directory: {error}"))?;
            }
        }
        let connection = match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(|error| format!("open operation database: {error}"))?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|error| format!("configure operation database: {error}"))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS operations (
                   id TEXT PRIMARY KEY,
                   operation_type TEXT NOT NULL,
                   idempotency_scope TEXT NOT NULL UNIQUE,
                   intent TEXT NOT NULL,
                   phase TEXT NOT NULL,
                   target_label TEXT NOT NULL,
                   object_ids_json TEXT NOT NULL,
                   preconditions_json TEXT NOT NULL,
                   policy_json TEXT,
                   files INTEGER NOT NULL DEFAULT 0,
                   folders INTEGER NOT NULL DEFAULT 0,
                   bytes INTEGER NOT NULL DEFAULT 0,
                   progress_bytes INTEGER NOT NULL DEFAULT 0,
                   error TEXT,
                   retry_condition TEXT,
                   cancellation_requested INTEGER NOT NULL DEFAULT 0,
                   verification_result TEXT,
                   residue TEXT,
                   started_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   dismissed INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX IF NOT EXISTS operations_visible
                   ON operations(dismissed, updated_at DESC);
                 CREATE INDEX IF NOT EXISTS operations_phase
                   ON operations(phase, updated_at DESC);
                 CREATE TABLE IF NOT EXISTS operation_attempts (
                   operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
                   attempt INTEGER NOT NULL,
                   worker TEXT NOT NULL,
                   phase TEXT NOT NULL,
                   started_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   finished_at INTEGER,
                   error TEXT,
                   PRIMARY KEY(operation_id, attempt)
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS operation_attempts_active
                   ON operation_attempts(operation_id)
                   WHERE finished_at IS NULL;",
            )
            .map_err(|error| format!("initialize operation database: {error}"))?;

        let now = now_ms();
        connection
            .execute(
                "UPDATE operations
                    SET phase = 'failed',
                        error = COALESCE(error, 'AllMyStuff restarted before this operation finished'),
                        retry_condition = COALESCE(retry_condition, 'Review the operation and retry it'),
                        updated_at = ?1
                  WHERE phase IN (
                    'scanning', 'awaiting-approval', 'staging', 'transferring',
                    'verifying', 'committing', 'materializing', 'cancelling',
                    'compensating'
                  )",
                params![as_i64(now)],
            )
            .map_err(|error| format!("recover interrupted operations: {error}"))?;
        connection
            .execute(
                "UPDATE operation_attempts
                    SET phase = 'failed',
                        error = COALESCE(error, 'AllMyStuff restarted during this worker attempt'),
                        updated_at = ?1,
                        finished_at = ?1
                  WHERE finished_at IS NULL",
                params![as_i64(now)],
            )
            .map_err(|error| format!("recover interrupted worker attempts: {error}"))?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn insert(&self, operation: &OperationRecord) -> Result<(), String> {
        validate(operation)?;
        let object_ids = serde_json::to_string(&operation.object_ids)
            .map_err(|error| format!("encode operation object ids: {error}"))?;
        let preconditions = serde_json::to_string(&operation.preconditions)
            .map_err(|error| format!("encode operation preconditions: {error}"))?;
        let policy = operation
            .policy
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| format!("encode operation policy: {error}"))?;
        let connection = self.connection.lock();
        connection
            .execute(
                "INSERT INTO operations (
                   id, operation_type, idempotency_scope, intent, phase,
                   target_label, object_ids_json, preconditions_json, policy_json,
                   files, folders, bytes, progress_bytes, error, retry_condition,
                   cancellation_requested, verification_result, residue,
                   started_at, updated_at
                 ) VALUES (
                   ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                   ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20
                 )",
                params![
                    operation.id,
                    operation.operation_type,
                    operation.idempotency_scope,
                    operation.intent,
                    operation.phase,
                    operation.target_label,
                    object_ids,
                    preconditions,
                    policy,
                    as_i64(operation.files),
                    as_i64(operation.folders),
                    as_i64(operation.bytes),
                    as_i64(operation.progress_bytes),
                    operation.error,
                    operation.retry_condition,
                    operation.cancellation_requested,
                    operation.verification_result,
                    operation.residue,
                    as_i64(operation.started_at),
                    as_i64(operation.updated_at),
                ],
            )
            .map_err(|error| match &error {
                rusqlite::Error::SqliteFailure(failure, _)
                    if failure.code == ErrorCode::ConstraintViolation =>
                {
                    format!("operation id or idempotency scope already exists: {error}")
                }
                _ => format!("persist operation: {error}"),
            })?;
        Ok(())
    }

    pub fn begin_attempt(&self, id: &str, worker: &str) -> Result<u64, String> {
        if worker.is_empty() || worker.len() > 512 {
            return Err("invalid operation worker".into());
        }
        let now = now_ms();
        let mut connection = self.connection.lock();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin worker-attempt transaction: {error}"))?;
        let operation_phase: Option<String> = transaction
            .query_row(
                "SELECT phase FROM operations WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("find operation for worker attempt: {error}"))?;
        let Some(operation_phase) = operation_phase else {
            return Err("operation does not exist".into());
        };
        if OperationPhase::parse(&operation_phase).is_some_and(OperationPhase::terminal) {
            return Err("terminal operation cannot start another worker attempt".into());
        }
        let active: Option<i64> = transaction
            .query_row(
                "SELECT attempt FROM operation_attempts
                  WHERE operation_id = ?1 AND finished_at IS NULL",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("find active worker attempt: {error}"))?;
        if active.is_some() {
            return Err("operation already has an active worker attempt".into());
        }
        let attempt: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(attempt), 0) + 1
                   FROM operation_attempts WHERE operation_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|error| format!("allocate worker attempt: {error}"))?;
        transaction
            .execute(
                "INSERT INTO operation_attempts (
                   operation_id, attempt, worker, phase, started_at, updated_at
                 ) VALUES (?1, ?2, ?3, 'running', ?4, ?4)",
                params![id, attempt, worker, as_i64(now)],
            )
            .map_err(|error| format!("persist worker attempt: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("commit worker attempt: {error}"))?;
        Ok(from_i64(attempt))
    }

    pub fn finish_attempt(
        &self,
        id: &str,
        attempt: u64,
        phase: &str,
        error: Option<&str>,
    ) -> Result<bool, String> {
        if !matches!(phase, "succeeded" | "failed" | "cancelled") {
            return Err("invalid terminal worker-attempt phase".into());
        }
        let now = now_ms();
        let connection = self.connection.lock();
        let changed = connection
            .execute(
                "UPDATE operation_attempts
                    SET phase = ?3, error = ?4, updated_at = ?5, finished_at = ?5
                  WHERE operation_id = ?1 AND attempt = ?2 AND finished_at IS NULL",
                params![id, as_i64(attempt), phase, error, as_i64(now)],
            )
            .map_err(|error| format!("finish worker attempt: {error}"))?;
        Ok(changed > 0)
    }

    pub fn update_progress(&self, id: &str, progress_bytes: u64) -> Result<bool, String> {
        let now = now_ms();
        let connection = self.connection.lock();
        let changed = connection
            .execute(
                "UPDATE operations
                    SET progress_bytes = MAX(progress_bytes, ?2), updated_at = ?3
                  WHERE id = ?1 AND phase NOT IN ('complete', 'failed', 'cancelled')",
                params![id, as_i64(progress_bytes), as_i64(now)],
            )
            .map_err(|error| format!("update operation progress: {error}"))?;
        Ok(changed > 0)
    }

    pub fn update_transfer(
        &self,
        id: &str,
        phase: OperationPhase,
        error: Option<&str>,
        cancellation_requested: bool,
    ) -> Result<bool, String> {
        let now = now_ms();
        let retry_condition =
            (phase == OperationPhase::Failed).then_some("Review the operation and retry it");
        let verification_result =
            (phase == OperationPhase::Complete).then_some("Destination commit completed");
        let mut connection = self.connection.lock();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin operation update: {error}"))?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT phase FROM operations WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("read current operation phase: {error}"))?;
        let Some(current) = current else {
            return Ok(false);
        };
        let current = OperationPhase::parse(&current)
            .ok_or_else(|| "operation has an unknown persisted phase".to_string())?;
        if !current.permits(phase) {
            return Err(format!(
                "invalid operation transition from {} to {}",
                current.as_str(),
                phase.as_str()
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE operations
                    SET phase = ?2,
                        error = ?3,
                        retry_condition = ?4,
                        cancellation_requested = cancellation_requested OR ?5,
                        verification_result = ?6,
                        updated_at = ?7
                  WHERE id = ?1",
                params![
                    id,
                    phase.as_str(),
                    error,
                    retry_condition,
                    cancellation_requested,
                    verification_result,
                    as_i64(now),
                ],
            )
            .map_err(|error| format!("update operation: {error}"))?;
        if phase.terminal() {
            compact_successes(&transaction)?;
        }
        transaction
            .commit()
            .map_err(|error| format!("commit operation update: {error}"))?;
        Ok(changed > 0)
    }

    pub fn dismiss(&self, id: &str) -> Result<bool, String> {
        let connection = self.connection.lock();
        let changed = connection
            .execute(
                "DELETE FROM operations
                  WHERE id = ?1 AND phase IN ('complete', 'failed', 'cancelled')",
                params![id],
            )
            .map_err(|error| format!("dismiss operation: {error}"))?;
        Ok(changed > 0)
    }

    pub fn snapshot(&self) -> Result<Vec<OperationRecord>, String> {
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare(
                "SELECT id, operation_type, idempotency_scope, intent, phase,
                        target_label, object_ids_json, preconditions_json, policy_json,
                        files, folders, bytes, progress_bytes, error, retry_condition,
                        cancellation_requested, verification_result, residue,
                        started_at, updated_at
                   FROM operations
                  WHERE dismissed = 0
                  ORDER BY updated_at DESC, started_at DESC
                  LIMIT ?1",
            )
            .map_err(|error| format!("prepare operation snapshot: {error}"))?;
        let rows = statement
            .query_map(params![VISIBLE_OPERATIONS], |row| {
                let object_ids: String = row.get(6)?;
                let preconditions: String = row.get(7)?;
                let policy: Option<String> = row.get(8)?;
                Ok(OperationRecord {
                    id: row.get(0)?,
                    operation_type: row.get(1)?,
                    idempotency_scope: row.get(2)?,
                    intent: row.get(3)?,
                    phase: row.get(4)?,
                    target_label: row.get(5)?,
                    object_ids: serde_json::from_str(&object_ids).unwrap_or_default(),
                    preconditions: serde_json::from_str(&preconditions)
                        .unwrap_or_else(|_| serde_json::json!({})),
                    policy: policy.and_then(|value| serde_json::from_str(&value).ok()),
                    files: from_i64(row.get(9)?),
                    folders: from_i64(row.get(10)?),
                    bytes: from_i64(row.get(11)?),
                    progress_bytes: from_i64(row.get(12)?),
                    error: row.get(13)?,
                    retry_condition: row.get(14)?,
                    cancellation_requested: row.get(15)?,
                    verification_result: row.get(16)?,
                    residue: row.get(17)?,
                    started_at: from_i64(row.get(18)?),
                    updated_at: from_i64(row.get(19)?),
                })
            })
            .map_err(|error| format!("query operation snapshot: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read operation snapshot: {error}"))
    }
}

fn compact_successes(connection: &Connection) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM operations
              WHERE phase IN ('complete', 'cancelled')
                AND id NOT IN (
                  SELECT id FROM operations
                   WHERE phase IN ('complete', 'cancelled')
                   ORDER BY updated_at DESC
                   LIMIT ?1
                )",
            params![RETAIN_SUCCESSES],
        )
        .map_err(|error| format!("compact operation history: {error}"))?;
    Ok(())
}

fn validate(operation: &OperationRecord) -> Result<(), String> {
    if operation.id.is_empty()
        || operation.id.len() > 128
        || !operation
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("invalid operation id".into());
    }
    if operation.idempotency_scope.is_empty() || operation.idempotency_scope.len() > 512 {
        return Err("invalid operation idempotency scope".into());
    }
    if operation.operation_type.is_empty() || operation.operation_type.len() > 80 {
        return Err("invalid operation type".into());
    }
    if OperationPhase::parse(&operation.phase).is_none() {
        return Err("invalid operation phase".into());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn as_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn from_i64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(id: &str, target: &str) -> OperationRecord {
        OperationRecord::file_transfer(id.into(), target.into(), 2, 1, 42, now_ms())
    }

    #[test]
    fn duplicate_id_cannot_replace_an_operation() {
        let store = OperationsStore::open(None).unwrap();
        store.insert(&transfer("same-id", "first")).unwrap();
        assert!(store.insert(&transfer("same-id", "second")).is_err());
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].target_label, "first");
    }

    #[test]
    fn restart_marks_nonterminal_work_failed_and_keeps_idempotency() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "allmystuff-operations-{}-{unique}.sqlite3",
            std::process::id()
        ));
        {
            let store = OperationsStore::open(Some(&path)).unwrap();
            store.insert(&transfer("restart-id", "peer")).unwrap();
        }
        {
            let reopened = OperationsStore::open(Some(&path)).unwrap();
            let snapshot = reopened.snapshot().unwrap();
            assert_eq!(snapshot[0].phase, "failed");
            assert!(snapshot[0].retry_condition.is_some());
            assert!(reopened
                .insert(&transfer("restart-id", "duplicate"))
                .is_err());
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn cancellation_request_survives_terminal_update() {
        let store = OperationsStore::open(None).unwrap();
        store.insert(&transfer("cancel-id", "peer")).unwrap();
        store
            .update_transfer("cancel-id", OperationPhase::Cancelling, None, true)
            .unwrap();
        store
            .update_transfer("cancel-id", OperationPhase::Cancelled, None, false)
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        assert!(snapshot[0].cancellation_requested);
    }

    #[test]
    fn worker_attempts_are_distinct_and_never_overlap() {
        let store = OperationsStore::open(None).unwrap();
        store.insert(&transfer("attempt-id", "peer")).unwrap();
        let first = store.begin_attempt("attempt-id", "worker-a").unwrap();
        assert!(store.begin_attempt("attempt-id", "worker-b").is_err());
        assert!(store
            .finish_attempt("attempt-id", first, "failed", Some("route lost"))
            .unwrap());
        let second = store.begin_attempt("attempt-id", "worker-b").unwrap();
        assert_eq!(second, first + 1);
        assert!(store
            .finish_attempt("attempt-id", second, "succeeded", None)
            .unwrap());
    }

    #[test]
    fn invalid_phase_transition_is_rejected() {
        let store = OperationsStore::open(None).unwrap();
        store.insert(&transfer("phase-id", "peer")).unwrap();
        assert!(store
            .update_transfer("phase-id", OperationPhase::Complete, None, false)
            .is_err());
        assert_eq!(store.snapshot().unwrap()[0].phase, "staging");
    }

    #[test]
    fn successful_history_is_compacted_but_failures_remain() {
        let store = OperationsStore::open(None).unwrap();
        for index in 0..60 {
            let id = format!("complete-{index}");
            store.insert(&transfer(&id, "peer")).unwrap();
            for phase in [
                OperationPhase::Transferring,
                OperationPhase::Verifying,
                OperationPhase::Committing,
                OperationPhase::Complete,
            ] {
                store.update_transfer(&id, phase, None, false).unwrap();
            }
        }
        store.insert(&transfer("failed-one", "peer")).unwrap();
        store
            .update_transfer("failed-one", OperationPhase::Failed, Some("boom"), false)
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.len(), RETAIN_SUCCESSES as usize + 1);
        assert!(snapshot
            .iter()
            .any(|operation| operation.id == "failed-one"));
    }
}
