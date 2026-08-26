//! Bounded service-quality observations for distributed fleet placement.
//!
//! This is scheduling evidence, not a user-activity log. It records only
//! lifecycle and operation outcomes that the node already observes. Samples
//! are aggregated into hourly buckets, retained for one week, and capped by
//! peer count. There is no network timer or active probing here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;

const BUCKET_MS: i64 = 60 * 60 * 1_000;
const RETAIN_BUCKETS: i64 = 7 * 24;
const MAX_PEERS: i64 = 512;

#[derive(Clone, Copy)]
struct LiveObservation {
    online: bool,
    since_ms: i64,
}

#[derive(Default)]
struct Aggregate {
    observed_ms: u64,
    online_ms: u64,
    connections: u64,
    disconnects: u64,
    latency_samples: u64,
    latency_sum_ms: u64,
    transfer_bytes: u64,
    transfer_ms: u64,
    successes: u64,
    failures: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceProfile {
    pub peer: String,
    pub state: &'static str,
    pub observed_hours: f64,
    pub availability: f64,
    pub average_latency_ms: Option<f64>,
    pub throughput_mbps: Option<f64>,
    pub operation_reliability: f64,
    pub confidence: f64,
    pub service_score: f64,
    pub connections: u64,
    pub disconnects: u64,
    pub successes: u64,
    pub failures: u64,
}

/// Local observations of other fleet members. SQLite makes the bounded history
/// crash-safe; the current process interval remains in memory so time while
/// this observer itself was stopped is never misclassified as peer downtime.
pub struct ServiceProfiles {
    connection: Mutex<Connection>,
    live: Mutex<HashMap<String, LiveObservation>>,
    last_gc_bucket: AtomicI64,
}

impl ServiceProfiles {
    pub fn load() -> Self {
        let path = allmystuff_protocol::myownmesh_state_dir()
            .map(|dir| dir.join("allmystuff-service-profiles.sqlite3"));
        match Self::open(path.as_ref()) {
            Ok(profiles) => profiles,
            Err(error) => {
                tracing::error!(
                    "fleet service-profile database unavailable; using memory aggregates: {error}"
                );
                Self::open(None).expect("in-memory service-profile schema must initialize")
            }
        }
    }

    fn open(path: Option<&PathBuf>) -> Result<Self, String> {
        if let Some(path) = path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("create service-profile directory: {error}"))?;
            }
        }
        let connection = match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(|error| format!("open service-profile database: {error}"))?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| format!("configure service-profile database: {error}"))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 CREATE TABLE IF NOT EXISTS peer_service_buckets (
                   peer TEXT NOT NULL,
                   bucket INTEGER NOT NULL,
                   observed_ms INTEGER NOT NULL DEFAULT 0,
                   online_ms INTEGER NOT NULL DEFAULT 0,
                   connections INTEGER NOT NULL DEFAULT 0,
                   disconnects INTEGER NOT NULL DEFAULT 0,
                   latency_samples INTEGER NOT NULL DEFAULT 0,
                   latency_sum_ms INTEGER NOT NULL DEFAULT 0,
                   transfer_bytes INTEGER NOT NULL DEFAULT 0,
                   transfer_ms INTEGER NOT NULL DEFAULT 0,
                   successes INTEGER NOT NULL DEFAULT 0,
                   failures INTEGER NOT NULL DEFAULT 0,
                   PRIMARY KEY(peer, bucket)
                 );
                 CREATE INDEX IF NOT EXISTS peer_service_recent
                   ON peer_service_buckets(bucket, peer);",
            )
            .map_err(|error| format!("initialize service-profile database: {error}"))?;
        Ok(Self {
            connection: Mutex::new(connection),
            live: Mutex::new(HashMap::new()),
            last_gc_bucket: AtomicI64::new(i64::MIN),
        })
    }

    /// Mark a peer transport state transition. Repeated presence/approved
    /// events in the same state are no-ops and therefore create no chatter.
    pub fn note_state(&self, peer: &str, online: bool) {
        self.note_state_at(peer, online, unix_millis());
    }

    fn note_state_at(&self, peer: &str, online: bool, now_ms: i64) {
        let peer = bounded_peer(peer);
        if peer.is_empty() {
            return;
        }
        let previous = {
            let mut live = self.live.lock();
            match live.get(&peer).copied() {
                Some(previous) if previous.online == online => return,
                previous => {
                    live.insert(
                        peer.clone(),
                        LiveObservation {
                            online,
                            since_ms: now_ms,
                        },
                    );
                    previous
                }
            }
        };
        if let Some(previous) = previous {
            self.add_interval(&peer, previous.online, previous.since_ms, now_ms);
        }
        self.add_counts(
            &peer,
            now_ms,
            if online { 1 } else { 0 },
            if online { 0 } else { 1 },
            0,
            0,
            0,
            0,
            0,
            0,
        );
    }

    /// Record a real request/response outcome. Callers measure traffic they
    /// already needed; this component never emits a probe.
    pub fn note_request(&self, peer: &str, elapsed: Duration, succeeded: bool) {
        let latency = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        self.add_counts(
            &bounded_peer(peer),
            unix_millis(),
            0,
            0,
            1,
            latency,
            0,
            0,
            u64::from(succeeded),
            u64::from(!succeeded),
        );
    }

    /// Record one completed or failed transfer, including achieved throughput
    /// when bytes moved. A cancelled operation is not treated as peer failure.
    pub fn note_transfer(
        &self,
        peer: &str,
        bytes: u64,
        elapsed: Duration,
        outcome: TransferOutcome,
    ) {
        self.add_counts(
            &bounded_peer(peer),
            unix_millis(),
            0,
            0,
            0,
            0,
            bytes,
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            u64::from(outcome == TransferOutcome::Succeeded),
            u64::from(outcome == TransferOutcome::Failed),
        );
    }

    pub fn snapshot(&self) -> Vec<ServiceProfile> {
        self.snapshot_at(unix_millis())
    }

    fn snapshot_at(&self, now_ms: i64) -> Vec<ServiceProfile> {
        let live = self.live.lock().clone();
        let cutoff_bucket = now_ms.div_euclid(BUCKET_MS) - RETAIN_BUCKETS + 1;
        let mut aggregates: HashMap<String, Aggregate> = HashMap::new();
        {
            let connection = self.connection.lock();
            let mut statement = match connection.prepare(
                "SELECT peer,
                        SUM(observed_ms), SUM(online_ms),
                        SUM(connections), SUM(disconnects),
                        SUM(latency_samples), SUM(latency_sum_ms),
                        SUM(transfer_bytes), SUM(transfer_ms),
                        SUM(successes), SUM(failures)
                 FROM peer_service_buckets
                 WHERE bucket >= ?1
                 GROUP BY peer",
            ) {
                Ok(statement) => statement,
                Err(error) => {
                    tracing::warn!("read service profiles failed: {error}");
                    return Vec::new();
                }
            };
            let rows = match statement.query_map(params![cutoff_bucket], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Aggregate {
                        observed_ms: sql_u64(row.get(1)?),
                        online_ms: sql_u64(row.get(2)?),
                        connections: sql_u64(row.get(3)?),
                        disconnects: sql_u64(row.get(4)?),
                        latency_samples: sql_u64(row.get(5)?),
                        latency_sum_ms: sql_u64(row.get(6)?),
                        transfer_bytes: sql_u64(row.get(7)?),
                        transfer_ms: sql_u64(row.get(8)?),
                        successes: sql_u64(row.get(9)?),
                        failures: sql_u64(row.get(10)?),
                    },
                ))
            }) {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!("query service profiles failed: {error}");
                    return Vec::new();
                }
            };
            for row in rows.flatten() {
                aggregates.insert(row.0, row.1);
            }
        }

        for (peer, observation) in &live {
            let elapsed = now_ms.saturating_sub(observation.since_ms).max(0) as u64;
            let aggregate = aggregates.entry(peer.clone()).or_default();
            aggregate.observed_ms = aggregate.observed_ms.saturating_add(elapsed);
            if observation.online {
                aggregate.online_ms = aggregate.online_ms.saturating_add(elapsed);
            }
        }

        let mut profiles = aggregates
            .into_iter()
            .map(|(peer, aggregate)| {
                profile_from(
                    peer.clone(),
                    live.get(&peer).map(|value| value.online),
                    aggregate,
                )
            })
            .collect::<Vec<_>>();
        profiles.sort_by(|left, right| {
            right
                .service_score
                .total_cmp(&left.service_score)
                .then_with(|| left.peer.cmp(&right.peer))
        });
        profiles
    }

    fn add_interval(&self, peer: &str, online: bool, start_ms: i64, end_ms: i64) {
        if end_ms <= start_ms {
            return;
        }
        let mut cursor = start_ms;
        let connection = self.connection.lock();
        while cursor < end_ms {
            let bucket = cursor.div_euclid(BUCKET_MS);
            let boundary = bucket.saturating_add(1).saturating_mul(BUCKET_MS);
            let stop = end_ms.min(boundary);
            let elapsed = stop.saturating_sub(cursor) as u64;
            if let Err(error) = connection.execute(
                "INSERT INTO peer_service_buckets(peer, bucket, observed_ms, online_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(peer, bucket) DO UPDATE SET
                   observed_ms = observed_ms + excluded.observed_ms,
                   online_ms = online_ms + excluded.online_ms",
                params![
                    peer,
                    bucket,
                    sql_i64(elapsed),
                    sql_i64(if online { elapsed } else { 0 })
                ],
            ) {
                tracing::warn!("record service interval failed: {error}");
                break;
            }
            cursor = stop;
        }
        drop(connection);
        self.maybe_collect(end_ms);
    }

    #[allow(clippy::too_many_arguments)]
    fn add_counts(
        &self,
        peer: &str,
        now_ms: i64,
        connections: u64,
        disconnects: u64,
        latency_samples: u64,
        latency_sum_ms: u64,
        transfer_bytes: u64,
        transfer_ms: u64,
        successes: u64,
        failures: u64,
    ) {
        if peer.is_empty() {
            return;
        }
        let bucket = now_ms.div_euclid(BUCKET_MS);
        let connection = self.connection.lock();
        if let Err(error) = connection.execute(
            "INSERT INTO peer_service_buckets(
               peer, bucket, connections, disconnects,
               latency_samples, latency_sum_ms, transfer_bytes, transfer_ms,
               successes, failures
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(peer, bucket) DO UPDATE SET
               connections = connections + excluded.connections,
               disconnects = disconnects + excluded.disconnects,
               latency_samples = latency_samples + excluded.latency_samples,
               latency_sum_ms = latency_sum_ms + excluded.latency_sum_ms,
               transfer_bytes = transfer_bytes + excluded.transfer_bytes,
               transfer_ms = transfer_ms + excluded.transfer_ms,
               successes = successes + excluded.successes,
               failures = failures + excluded.failures",
            params![
                peer,
                bucket,
                sql_i64(connections),
                sql_i64(disconnects),
                sql_i64(latency_samples),
                sql_i64(latency_sum_ms),
                sql_i64(transfer_bytes),
                sql_i64(transfer_ms),
                sql_i64(successes),
                sql_i64(failures)
            ],
        ) {
            tracing::warn!("record service outcome failed: {error}");
        }
        drop(connection);
        self.maybe_collect(now_ms);
    }

    fn maybe_collect(&self, now_ms: i64) {
        let bucket = now_ms.div_euclid(BUCKET_MS);
        if self.last_gc_bucket.swap(bucket, Ordering::Relaxed) == bucket {
            return;
        }
        let cutoff = bucket - RETAIN_BUCKETS + 1;
        let connection = self.connection.lock();
        if let Err(error) = connection.execute(
            "DELETE FROM peer_service_buckets WHERE bucket < ?1",
            params![cutoff],
        ) {
            tracing::warn!("expire service profiles failed: {error}");
        }
        if let Err(error) = connection.execute(
            "DELETE FROM peer_service_buckets
             WHERE peer IN (
               SELECT peer
               FROM peer_service_buckets
               GROUP BY peer
               ORDER BY MAX(bucket) DESC, peer
               LIMIT -1 OFFSET ?1
             )",
            params![MAX_PEERS],
        ) {
            tracing::warn!("bound service-profile peers failed: {error}");
        }
    }

    #[cfg(test)]
    fn memory() -> Self {
        Self::open(None).unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

fn profile_from(peer: String, live: Option<bool>, aggregate: Aggregate) -> ServiceProfile {
    let availability = if aggregate.observed_ms == 0 {
        0.85
    } else {
        aggregate.online_ms as f64 / aggregate.observed_ms as f64
    }
    .clamp(0.0, 1.0);
    let operations = aggregate.successes.saturating_add(aggregate.failures);
    let operation_reliability = if operations == 0 {
        0.95
    } else {
        aggregate.successes as f64 / operations as f64
    }
    .clamp(0.0, 1.0);
    let average_latency_ms = (aggregate.latency_samples > 0)
        .then(|| aggregate.latency_sum_ms as f64 / aggregate.latency_samples as f64);
    let throughput_mbps = (aggregate.transfer_ms > 0)
        .then(|| aggregate.transfer_bytes as f64 * 8.0 / aggregate.transfer_ms as f64 / 1_000.0);
    let observed_confidence =
        (aggregate.observed_ms as f64 / (RETAIN_BUCKETS * BUCKET_MS) as f64).clamp(0.0, 1.0);
    let operation_confidence = (operations as f64 / 20.0).clamp(0.0, 1.0);
    let confidence = (observed_confidence * 0.6 + operation_confidence * 0.4).clamp(0.0, 1.0);
    let latency_quality = average_latency_ms
        .map(|latency| 1.0 / (1.0 + latency / 100.0))
        .unwrap_or(0.5);
    let throughput_quality = throughput_mbps
        .map(|throughput| (throughput / 100.0).clamp(0.0, 1.0))
        .unwrap_or(0.5);
    let measured_score = availability * 0.4
        + operation_reliability * 0.3
        + latency_quality * 0.15
        + throughput_quality * 0.15;
    // Conservative prior: an unobserved peer is usable, but cannot beat a
    // well-observed healthy peer merely because it has no failures recorded.
    let service_score = 0.55 * (1.0 - confidence) + measured_score * confidence;

    ServiceProfile {
        peer,
        state: match live {
            Some(true) => "online",
            Some(false) => "offline",
            None => "unknown",
        },
        observed_hours: aggregate.observed_ms as f64 / BUCKET_MS as f64,
        availability,
        average_latency_ms,
        throughput_mbps,
        operation_reliability,
        confidence,
        service_score,
        connections: aggregate.connections,
        disconnects: aggregate.disconnects,
        successes: aggregate.successes,
        failures: aggregate.failures,
    }
}

fn bounded_peer(peer: &str) -> String {
    peer.trim().chars().take(512).collect()
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i128::from(i64::MAX) as u128) as i64
}

fn sql_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn sql_u64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_counts_only_while_this_process_observes_the_peer() {
        let profiles = ServiceProfiles::memory();
        let hour = BUCKET_MS;
        profiles.note_state_at("peer-a", true, hour);
        profiles.note_state_at("peer-a", false, hour + 30 * 60 * 1_000);
        let snapshot = profiles.snapshot_at(hour + 60 * 60 * 1_000);
        assert_eq!(snapshot.len(), 1);
        assert!((snapshot[0].observed_hours - 1.0).abs() < 0.001);
        assert!((snapshot[0].availability - 0.5).abs() < 0.001);
        assert_eq!(snapshot[0].state, "offline");
    }

    #[test]
    fn intervals_are_split_across_buckets_and_old_history_expires() {
        let profiles = ServiceProfiles::memory();
        profiles.note_state_at("peer-a", true, BUCKET_MS - 1_000);
        profiles.note_state_at("peer-a", false, BUCKET_MS + 1_000);
        let first = profiles.snapshot_at(BUCKET_MS + 1_000);
        assert!((first[0].observed_hours - (2_000.0 / BUCKET_MS as f64)).abs() < 0.0001);

        profiles.note_state_at("peer-a", true, (RETAIN_BUCKETS + 2) * BUCKET_MS);
        let recent = profiles.snapshot_at((RETAIN_BUCKETS + 2) * BUCKET_MS);
        assert_eq!(recent[0].connections, 1);
    }

    #[test]
    fn sparse_history_gets_a_conservative_candidate_prior() {
        let profile = profile_from(
            "new".into(),
            Some(true),
            Aggregate {
                observed_ms: 1_000,
                online_ms: 1_000,
                successes: 1,
                ..Aggregate::default()
            },
        );
        assert!(profile.confidence < 0.1);
        assert!(profile.service_score < 0.6);
    }

    #[test]
    fn request_and_transfer_statistics_are_bounded_aggregates() {
        let profiles = ServiceProfiles::memory();
        profiles.note_request("peer-a", Duration::from_millis(20), true);
        profiles.note_transfer(
            "peer-a",
            10_000_000,
            Duration::from_secs(1),
            TransferOutcome::Succeeded,
        );
        let snapshot = profiles.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].successes, 2);
        assert_eq!(snapshot[0].average_latency_ms, Some(20.0));
        assert!(snapshot[0].throughput_mbps.unwrap() > 79.0);
    }

    #[test]
    fn cancellations_do_not_poison_peer_reliability() {
        let profiles = ServiceProfiles::memory();
        profiles.note_transfer(
            "peer-a",
            0,
            Duration::from_millis(5),
            TransferOutcome::Cancelled,
        );
        let snapshot = profiles.snapshot();
        assert_eq!(snapshot[0].successes, 0);
        assert_eq!(snapshot[0].failures, 0);
        assert_eq!(snapshot[0].operation_reliability, 0.95);
    }
}
