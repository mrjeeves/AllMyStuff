//! Fleet-wide metadata for the Files canvas.
//!
//! File bytes and directory listings never enter this store. It contains only
//! presentation records (frames, item placements and view preferences). Each
//! entity is an LWW register with a Lamport stamp; unrelated offline edits
//! merge, while equal-counter conflicts converge by actor id. Deletes are
//! tombstones so a late snapshot cannot resurrect them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_RECORDS: usize = 20_000;
const MAX_MUTATIONS_PER_APPLY: usize = 512;
const MAX_VALUE_BYTES: usize = 8 * 1024;
pub const SNAPSHOT_CHUNK_RECORDS: usize = 16;

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct CanvasStamp {
    pub counter: u64,
    pub actor: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanvasRecord {
    pub id: String,
    pub kind: String,
    pub value: Option<Value>,
    pub stamp: CanvasStamp,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CanvasMutation {
    pub id: String,
    pub kind: String,
    pub value: Option<Value>,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CanvasMessage {
    Patch {
        #[serde(default)]
        epoch: CanvasStamp,
        records: Vec<CanvasRecord>,
    },
    Digest {
        #[serde(default)]
        epoch: CanvasStamp,
        digest: String,
    },
    /// Advances the document-wide anti-resurrection boundary. Receivers drop
    /// every older-epoch record before accepting the live snapshot chunks that
    /// follow. Repeating the same barrier is deliberately a no-op, so a delayed
    /// duplicate cannot erase edits authored after the purge.
    Barrier { epoch: CanvasStamp },
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Persisted {
    /// The latest fleet-wide compaction barrier. Legacy stores/messages default
    /// to epoch zero and continue to merge until the first explicit purge.
    #[serde(default)]
    epoch: CanvasStamp,
    #[serde(default)]
    records: BTreeMap<String, CanvasRecord>,
    #[serde(default)]
    counters: HashMap<String, u64>,
}

#[derive(Clone, Debug)]
pub struct CanvasBatch {
    pub epoch: CanvasStamp,
    pub records: Vec<CanvasRecord>,
}

#[derive(Clone, Debug)]
pub struct CanvasPurge {
    pub epoch: CanvasStamp,
    pub purged: usize,
    pub live_records: Vec<CanvasRecord>,
}

pub struct CanvasStore {
    path: Option<PathBuf>,
    inner: Mutex<Persisted>,
}

impl CanvasStore {
    pub fn load() -> Self {
        Self::load_at(
            allmystuff_protocol::myownmesh_state_dir()
                .map(|dir| dir.join("allmystuff-files-canvas.json")),
        )
    }

    fn load_at(path: Option<PathBuf>) -> Self {
        let mut inner: Persisted = path
            .as_ref()
            .map(|path| crate::persist::load_json(path))
            .unwrap_or_default();
        inner
            .records
            .retain(|id, record| id == &record.id && valid_record(record));
        if !valid_epoch(&inner.epoch) {
            inner.epoch = CanvasStamp::default();
        }
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    pub fn snapshot(&self) -> Vec<CanvasRecord> {
        self.inner.lock().records.values().cloned().collect()
    }

    /// Epoch + records captured under one lock. Pairing them is important: a
    /// purge racing a snapshot must never label pre-purge records as current.
    pub fn snapshot_state(&self) -> CanvasBatch {
        let inner = self.inner.lock();
        CanvasBatch {
            epoch: inner.epoch.clone(),
            records: inner.records.values().cloned().collect(),
        }
    }

    pub fn status(&self) -> (CanvasStamp, usize, usize) {
        let inner = self.inner.lock();
        let tombstones = inner
            .records
            .values()
            .filter(|record| record.deleted)
            .count();
        (
            inner.epoch.clone(),
            inner.records.len().saturating_sub(tombstones),
            tombstones,
        )
    }

    /// A small convergence probe for reconnects where neither process rebooted.
    /// FNV-1a is not an authentication primitive (the fleet channel provides
    /// authenticity); it is only a deterministic change detector.
    pub fn digest(&self) -> String {
        let inner = self.inner.lock();
        digest_inner(&inner)
    }

    pub fn digest_state(&self) -> (CanvasStamp, String) {
        let inner = self.inner.lock();
        (inner.epoch.clone(), digest_inner(&inner))
    }

    pub fn epoch(&self) -> CanvasStamp {
        self.inner.lock().epoch.clone()
    }
}

fn digest_inner(inner: &Persisted) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for record in inner.records.values() {
        let Ok(bytes) = serde_json::to_vec(record) else {
            continue;
        };
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

impl CanvasStore {
    pub fn apply_local(
        &self,
        actor: &str,
        mutations: Vec<CanvasMutation>,
    ) -> Result<Vec<CanvasRecord>, String> {
        self.apply_local_batch(actor, mutations)
            .map(|batch| batch.records)
    }

    /// Apply a local edit and return the epoch atomically paired with it.
    pub fn apply_local_batch(
        &self,
        actor: &str,
        mutations: Vec<CanvasMutation>,
    ) -> Result<CanvasBatch, String> {
        if mutations.len() > MAX_MUTATIONS_PER_APPLY {
            return Err("too many canvas changes in one patch".into());
        }
        if actor.is_empty() || actor.len() > 512 {
            return Err("invalid canvas actor".into());
        }
        let mut inner = self.inner.lock();
        for mutation in &mutations {
            validate(
                &mutation.id,
                &mutation.kind,
                mutation.value.as_ref(),
                mutation.deleted,
            )?;
        }
        let new_ids: HashSet<_> = mutations
            .iter()
            .filter(|mutation| !inner.records.contains_key(&mutation.id))
            .map(|mutation| mutation.id.as_str())
            .collect();
        if inner.records.len().saturating_add(new_ids.len()) > MAX_RECORDS {
            return Err("canvas has too many records".into());
        }
        let old_counter = inner.counters.get(actor).copied();
        // A local edit must outrank the record it is replacing even when that
        // record came from a different actor with a much higher clock. This is
        // a Lamport clock, not an independent per-device sequence.
        let observed = inner
            .counters
            .values()
            .copied()
            .max()
            .unwrap_or_default()
            .max(inner.epoch.counter);
        let mut counter = old_counter.unwrap_or_default().max(observed);
        if counter > u64::MAX.saturating_sub(mutations.len() as u64) {
            return Err("canvas clock exhausted".into());
        }
        let mut previous = HashMap::new();
        for mutation in &mutations {
            previous
                .entry(mutation.id.clone())
                .or_insert_with(|| inner.records.get(&mutation.id).cloned());
        }
        let mut records = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            counter += 1;
            let record = CanvasRecord {
                id: mutation.id,
                kind: mutation.kind,
                value: mutation.value,
                stamp: CanvasStamp {
                    counter,
                    actor: actor.into(),
                },
                deleted: mutation.deleted,
            };
            inner.records.insert(record.id.clone(), record.clone());
            records.push(record);
        }
        inner.counters.insert(actor.into(), counter);
        if let Err(error) = persist(&self.path, &inner) {
            for (id, record) in previous {
                match record {
                    Some(record) => {
                        inner.records.insert(id, record);
                    }
                    None => {
                        inner.records.remove(&id);
                    }
                }
            }
            match old_counter {
                Some(counter) => {
                    inner.counters.insert(actor.into(), counter);
                }
                None => {
                    inner.counters.remove(actor);
                }
            }
            return Err(error);
        }
        Ok(CanvasBatch {
            epoch: inner.epoch.clone(),
            records,
        })
    }

    pub fn merge(&self, incoming: Vec<CanvasRecord>) -> bool {
        let epoch = self.inner.lock().epoch.clone();
        self.merge_at_epoch(epoch, incoming, false)
    }

    /// Merge records scoped to `epoch`. A higher epoch is accepted only when
    /// the caller has authenticated the sender as an owner/manager; advancing
    /// clears every old record before merging so an offline live record cannot
    /// reappear after its tombstone was compacted.
    pub fn merge_at_epoch(
        &self,
        epoch: CanvasStamp,
        incoming: Vec<CanvasRecord>,
        allow_advance: bool,
    ) -> bool {
        if incoming.len() > MAX_RECORDS {
            return false;
        }
        if !valid_epoch(&epoch) {
            return false;
        }
        let mut inner = self.inner.lock();
        if epoch < inner.epoch || (epoch > inner.epoch && !allow_advance) {
            return false;
        }
        let previous = inner.clone();
        let mut changed = epoch > inner.epoch;
        if changed {
            inner.epoch = epoch;
            inner.records.clear();
        }
        for record in incoming {
            if !valid_record(&record) {
                continue;
            }
            let newer = inner
                .records
                .get(&record.id)
                .is_none_or(|old| record.stamp > old.stamp);
            if !newer {
                continue;
            }
            // Never discard a tombstone to make room: doing so lets an old,
            // offline snapshot resurrect deleted canvas entities. At the cap,
            // existing ids may still converge but new ids fail closed.
            if !inner.records.contains_key(&record.id) && inner.records.len() >= MAX_RECORDS {
                continue;
            }
            inner
                .counters
                .entry(record.stamp.actor.clone())
                .and_modify(|n| *n = (*n).max(record.stamp.counter))
                .or_insert(record.stamp.counter);
            inner.records.insert(record.id.clone(), record);
            changed = true;
        }
        if changed {
            if let Err(error) = persist(&self.path, &inner) {
                tracing::error!("persisting merged Files canvas failed: {error}");
                *inner = previous;
                return false;
            }
        }
        changed
    }

    /// Install a strictly newer purge barrier without any records. Snapshot
    /// chunks may follow; a dropped connection heals via the epoch-aware digest
    /// exchange. Equal barriers do nothing so delayed duplicates are harmless.
    pub fn apply_barrier(&self, epoch: CanvasStamp) -> bool {
        if !valid_epoch(&epoch) {
            return false;
        }
        let mut inner = self.inner.lock();
        if epoch <= inner.epoch {
            return false;
        }
        let previous = inner.clone();
        inner.epoch = epoch;
        inner.records.clear();
        if let Err(error) = persist(&self.path, &inner) {
            tracing::error!("persisting Files canvas purge barrier failed: {error}");
            *inner = previous;
            return false;
        }
        true
    }

    /// Compact all tombstones behind a new fleet-wide epoch. Live records stay,
    /// while every older-epoch snapshot becomes permanently ineligible to merge.
    pub fn purge_tombstones(&self, actor: &str) -> Result<CanvasPurge, String> {
        if actor.is_empty() || actor.len() > 512 {
            return Err("invalid canvas actor".into());
        }
        let mut inner = self.inner.lock();
        let purged = inner
            .records
            .values()
            .filter(|record| record.deleted)
            .count();
        if purged == 0 {
            return Ok(CanvasPurge {
                epoch: inner.epoch.clone(),
                purged,
                live_records: inner.records.values().cloned().collect(),
            });
        }
        let previous = inner.clone();
        let next = inner
            .epoch
            .counter
            .max(inner.counters.get(actor).copied().unwrap_or_default())
            .checked_add(1)
            .ok_or("canvas clock exhausted")?;
        inner.epoch = CanvasStamp {
            counter: next,
            actor: actor.into(),
        };
        inner.counters.insert(actor.into(), next);
        inner.records.retain(|_, record| !record.deleted);
        if let Err(error) = persist(&self.path, &inner) {
            *inner = previous;
            return Err(error);
        }
        Ok(CanvasPurge {
            epoch: inner.epoch.clone(),
            purged,
            live_records: inner.records.values().cloned().collect(),
        })
    }
}

fn valid_epoch(epoch: &CanvasStamp) -> bool {
    (epoch.counter == 0 && epoch.actor.is_empty())
        || (epoch.counter > 0 && !epoch.actor.is_empty() && epoch.actor.len() <= 512)
}

fn valid_record(record: &CanvasRecord) -> bool {
    !record.stamp.actor.is_empty()
        && record.stamp.actor.len() <= 512
        && validate(
            &record.id,
            &record.kind,
            record.value.as_ref(),
            record.deleted,
        )
        .is_ok()
}

fn validate(id: &str, kind: &str, value: Option<&Value>, deleted: bool) -> Result<(), String> {
    if id.is_empty() || id.len() > 1024 {
        return Err("invalid canvas record id".into());
    }
    if !matches!(kind, "frame" | "item" | "preference") {
        return Err("invalid canvas record kind".into());
    }
    if deleted {
        return if value.is_none() {
            Ok(())
        } else {
            Err("a canvas tombstone cannot carry a value".into())
        };
    }
    let value = value.ok_or("a live canvas record needs a value")?;
    if value
        .as_object()
        .and_then(|_| serde_json::to_vec(value).ok())
        .is_some_and(|bytes| bytes.len() > MAX_VALUE_BYTES)
    {
        return Err("canvas record is too large".into());
    }
    let object = value.as_object().ok_or("canvas values must be objects")?;
    match kind {
        "frame" => {
            if object.get("id").and_then(Value::as_str) != Some(id)
                || !bounded_string(object.get("title"), 256)
                || !short_string(object.get("color"), 64)
                || !optional_id(object.get("parentId"))
                || !coordinate(object.get("x"))
                || !coordinate(object.get("y"))
                || !dimension(object.get("width"))
                || !dimension(object.get("height"))
            {
                return Err("invalid canvas frame value".into());
            }
        }
        "item" => {
            if !short_string(object.get("id"), 1024)
                || !optional_id(object.get("parentId"))
                || !coordinate(object.get("x"))
                || !coordinate(object.get("y"))
            {
                return Err("invalid canvas item value".into());
            }
        }
        "preference" => {}
        _ => unreachable!(),
    }
    Ok(())
}

fn short_string(value: Option<&Value>, max: usize) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty() && value.len() <= max)
}

fn bounded_string(value: Option<&Value>, max: usize) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| value.len() <= max)
}

fn optional_id(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.is_null() || short_string(Some(value), 1024))
}

fn coordinate(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_f64)
        .is_some_and(|value| value.is_finite() && value.abs() <= 10_000_000.0)
}

fn dimension(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_f64)
        .is_some_and(|value| value.is_finite() && (16.0..=1_000_000.0).contains(&value))
}

fn persist(path: &Option<PathBuf>, inner: &Persisted) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    let parent = path.parent().ok_or("canvas state path has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("creating canvas state directory: {error}"))?;
    let bytes = serde_json::to_vec_pretty(inner)
        .map_err(|error| format!("serializing Files canvas: {error}"))?;
    crate::persist::write_atomic(path, &bytes)
        .map_err(|error| format!("persisting Files canvas: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutation(id: &str, value: i32) -> CanvasMutation {
        CanvasMutation {
            id: id.into(),
            kind: "item".into(),
            value: Some(serde_json::json!({ "id": id, "x": value, "y": 0, "parentId": null })),
            deleted: false,
        }
    }

    #[test]
    fn concurrent_records_converge_by_actor() {
        let left = CanvasStore::load_at(None);
        let right = CanvasStore::load_at(None);
        let a = left
            .apply_local("alpha", vec![mutation("same", 1)])
            .unwrap();
        let b = right
            .apply_local("beta", vec![mutation("same", 2)])
            .unwrap();
        left.merge(b.clone());
        right.merge(a);
        assert_eq!(left.snapshot(), right.snapshot());
        assert_eq!(left.snapshot()[0].value.as_ref().unwrap()["x"], 2);
    }

    #[test]
    fn unrelated_offline_edits_survive_merge() {
        let store = CanvasStore::load_at(None);
        let a = store.apply_local("a", vec![mutation("one", 1)]).unwrap();
        let other = CanvasStore::load_at(None);
        let b = other.apply_local("b", vec![mutation("two", 2)]).unwrap();
        store.merge(b);
        assert_eq!(store.snapshot().len(), 2);
        assert!(!store.merge(a), "echo is idempotent");
    }

    #[test]
    fn local_edit_outranks_a_remote_high_counter() {
        let store = CanvasStore::load_at(None);
        let remote = CanvasRecord {
            id: "same".into(),
            kind: "item".into(),
            value: mutation("same", 1).value,
            stamp: CanvasStamp {
                counter: 500,
                actor: "remote".into(),
            },
            deleted: false,
        };
        assert!(store.merge(vec![remote]));

        let local = store
            .apply_local("local", vec![mutation("same", 2)])
            .unwrap();
        assert_eq!(local[0].stamp.counter, 501);

        let peer = CanvasStore::load_at(None);
        assert!(peer.merge(store.snapshot()));
        assert_eq!(peer.snapshot()[0].value.as_ref().unwrap()["x"], 2);
    }

    #[test]
    fn rejected_local_batch_is_transactional() {
        let store = CanvasStore::load_at(None);
        let invalid = CanvasMutation {
            id: "bad".into(),
            kind: "unknown".into(),
            value: None,
            deleted: false,
        };
        assert!(store
            .apply_local("a", vec![mutation("valid", 1), invalid])
            .is_err());
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn malformed_geometry_and_mismatched_frame_identity_are_rejected() {
        let store = CanvasStore::load_at(None);
        let frame = CanvasMutation {
            id: "frame:a".into(),
            kind: "frame".into(),
            value: Some(serde_json::json!({
                "id": "frame:b",
                "title": "bad",
                "color": "violet",
                "parentId": null,
                "x": 0,
                "y": 0,
                "width": -10,
                "height": 100
            })),
            deleted: false,
        };
        assert!(store.apply_local("a", vec![frame]).is_err());
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn failed_persistence_rolls_back_the_local_change() {
        let target = std::env::temp_dir().join(format!(
            "allmystuff-canvas-test-directory-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&target).unwrap();
        let store = CanvasStore::load_at(Some(target.clone()));
        assert!(store.apply_local("a", vec![mutation("one", 1)]).is_err());
        assert!(store.snapshot().is_empty());
        std::fs::remove_dir(&target).unwrap();
    }

    #[test]
    fn digest_detects_divergence_and_matches_after_merge() {
        let left = CanvasStore::load_at(None);
        let right = CanvasStore::load_at(None);
        assert_eq!(left.digest(), right.digest());
        let patch = left.apply_local("a", vec![mutation("one", 1)]).unwrap();
        assert_ne!(left.digest(), right.digest());
        assert!(right.merge(patch));
        assert_eq!(left.digest(), right.digest());
    }

    #[test]
    fn old_snapshot_cannot_resurrect_a_tombstone() {
        let store = CanvasStore::load_at(None);
        let live = store.apply_local("a", vec![mutation("one", 1)]).unwrap();
        store
            .apply_local(
                "a",
                vec![CanvasMutation {
                    id: "one".into(),
                    kind: "item".into(),
                    value: None,
                    deleted: true,
                }],
            )
            .unwrap();
        assert!(!store.merge(live));
        assert!(store.snapshot()[0].deleted);
    }

    #[test]
    fn purge_rejects_every_pre_epoch_snapshot() {
        let store = CanvasStore::load_at(None);
        let stale = store
            .apply_local("owner", vec![mutation("deleted", 1), mutation("kept", 2)])
            .unwrap();
        store
            .apply_local(
                "owner",
                vec![CanvasMutation {
                    id: "deleted".into(),
                    kind: "item".into(),
                    value: None,
                    deleted: true,
                }],
            )
            .unwrap();

        let purge = store.purge_tombstones("owner").unwrap();
        assert_eq!(purge.purged, 1);
        assert_eq!(purge.live_records.len(), 1);
        assert!(!store.merge_at_epoch(CanvasStamp::default(), stale, true));
        assert_eq!(store.snapshot()[0].id, "kept");
    }

    #[test]
    fn repeated_barrier_cannot_erase_post_purge_edits() {
        let store = CanvasStore::load_at(None);
        store
            .apply_local("owner", vec![mutation("gone", 1)])
            .unwrap();
        store
            .apply_local(
                "owner",
                vec![CanvasMutation {
                    id: "gone".into(),
                    kind: "item".into(),
                    value: None,
                    deleted: true,
                }],
            )
            .unwrap();
        let epoch = store.purge_tombstones("owner").unwrap().epoch;
        store
            .apply_local("member", vec![mutation("after", 2)])
            .unwrap();

        assert!(!store.apply_barrier(epoch));
        assert_eq!(store.snapshot()[0].id, "after");
    }

    #[test]
    fn higher_epoch_patch_requires_manager_authorization() {
        let store = CanvasStore::load_at(None);
        let epoch = CanvasStamp {
            counter: 7,
            actor: "owner".into(),
        };
        let records = CanvasStore::load_at(None)
            .apply_local("owner", vec![mutation("one", 1)])
            .unwrap();

        assert!(!store.merge_at_epoch(epoch.clone(), records.clone(), false));
        assert_eq!(store.epoch(), CanvasStamp::default());
        assert!(store.merge_at_epoch(epoch.clone(), records, true));
        assert_eq!(store.epoch(), epoch);
    }

    #[test]
    fn concurrent_purges_converge_by_epoch_actor() {
        let left = CanvasStore::load_at(None);
        let right = CanvasStore::load_at(None);
        let initial = left
            .apply_local("seed", vec![mutation("kept", 1), mutation("gone", 2)])
            .unwrap();
        assert!(right.merge(initial));
        for store in [&left, &right] {
            store
                .apply_local(
                    "seed",
                    vec![CanvasMutation {
                        id: "gone".into(),
                        kind: "item".into(),
                        value: None,
                        deleted: true,
                    }],
                )
                .unwrap();
        }
        let a = left.purge_tombstones("alpha").unwrap();
        let b = right.purge_tombstones("beta").unwrap();
        let winner = if a.epoch > b.epoch { a } else { b };

        for store in [&left, &right] {
            if store.epoch() < winner.epoch {
                assert!(store.apply_barrier(winner.epoch.clone()));
                assert!(store.merge_at_epoch(
                    winner.epoch.clone(),
                    winner.live_records.clone(),
                    false,
                ));
            }
        }
        assert_eq!(left.epoch(), right.epoch());
        assert_eq!(left.snapshot(), right.snapshot());
    }

    #[test]
    fn empty_purge_does_not_create_network_churn() {
        let store = CanvasStore::load_at(None);
        store
            .apply_local("owner", vec![mutation("live", 1)])
            .unwrap();
        let before = store.epoch();
        let purge = store.purge_tombstones("owner").unwrap();
        assert_eq!(purge.purged, 0);
        assert_eq!(purge.epoch, before);
    }
}
