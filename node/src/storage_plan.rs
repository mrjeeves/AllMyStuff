//! Small, durable fleet storage policy.
//!
//! This is deliberately separate from the Files canvas: allocations and
//! durability policy are operational authority, not layout metadata. The plan
//! is bounded by fleet devices/volumes and converges as per-record LWW state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const MAX_ALLOCATIONS: usize = 512;
pub const PLAN_CHUNK: usize = 16;

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct PlanStamp {
    pub counter: u64,
    pub actor: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StoragePolicy {
    #[serde(alias = "ordinaryReplicas")]
    pub replicas: u8,
    pub reserve_percent: u8,
    pub version_retention_days: u16,
    pub rebalance_gib_per_day: u32,
    pub pause_on_metered: bool,
}

impl Default for StoragePolicy {
    fn default() -> Self {
        Self {
            replicas: 2,
            reserve_percent: 10,
            version_retention_days: 30,
            rebalance_gib_per_day: 50,
            pause_on_metered: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRecord {
    pub value: StoragePolicy,
    pub stamp: PlanStamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageAllocation {
    pub id: String,
    pub device: String,
    pub volume: String,
    pub quota_bytes: u64,
    pub enabled: bool,
    pub stamp: PlanStamp,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoragePlanSnapshot {
    pub policy: PolicyRecord,
    pub allocations: Vec<StorageAllocation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoragePlanMessage {
    Patch {
        policy: Option<PolicyRecord>,
        allocations: Vec<StorageAllocation>,
    },
    Digest {
        digest: String,
    },
    SyncRequest,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Persisted {
    policy: PolicyRecord,
    allocations: BTreeMap<String, StorageAllocation>,
    counters: BTreeMap<String, u64>,
}

pub struct StoragePlanStore {
    path: Option<PathBuf>,
    inner: Mutex<Persisted>,
}

impl StoragePlanStore {
    pub fn load() -> Self {
        Self::load_at(
            allmystuff_protocol::myownmesh_state_dir()
                .map(|dir| dir.join("allmystuff-fleet-storage-plan.json")),
        )
    }

    fn load_at(path: Option<PathBuf>) -> Self {
        let mut inner: Persisted = path
            .as_ref()
            .map(|path| crate::persist::load_json(path))
            .unwrap_or_default();
        if !valid_policy_record(&inner.policy) {
            inner.policy = PolicyRecord::default();
        }
        inner
            .allocations
            .retain(|id, allocation| id == &allocation.id && valid_allocation(allocation));
        if inner.allocations.len() > MAX_ALLOCATIONS {
            inner.allocations = inner
                .allocations
                .into_iter()
                .take(MAX_ALLOCATIONS)
                .collect();
        }
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    pub fn snapshot(&self) -> StoragePlanSnapshot {
        let inner = self.inner.lock();
        StoragePlanSnapshot {
            policy: inner.policy.clone(),
            allocations: inner.allocations.values().cloned().collect(),
        }
    }

    pub fn digest(&self) -> String {
        let inner = self.inner.lock();
        let bytes = serde_json::to_vec(&*inner).unwrap_or_default();
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{hash:016x}")
    }

    pub fn set_policy(&self, actor: &str, value: StoragePolicy) -> Result<PolicyRecord, String> {
        validate_policy(&value)?;
        let mut inner = self.inner.lock();
        let stamp = next_stamp(&mut inner, actor)?;
        let previous = inner.policy.clone();
        inner.policy = PolicyRecord { value, stamp };
        if let Err(error) = persist(&self.path, &inner) {
            inner.policy = previous;
            return Err(error);
        }
        Ok(inner.policy.clone())
    }

    pub fn set_allocation(
        &self,
        actor: &str,
        device: String,
        volume: String,
        quota_bytes: u64,
        enabled: bool,
    ) -> Result<StorageAllocation, String> {
        let id = allocation_id(&device, &volume)?;
        let mut inner = self.inner.lock();
        if !inner.allocations.contains_key(&id) && inner.allocations.len() >= MAX_ALLOCATIONS {
            return Err("the fleet storage plan has too many allocations".into());
        }
        let stamp = next_stamp(&mut inner, actor)?;
        let allocation = StorageAllocation {
            id: id.clone(),
            device,
            volume,
            quota_bytes,
            enabled,
            stamp,
        };
        if !valid_allocation(&allocation) {
            return Err("invalid storage allocation".into());
        }
        let previous = inner.allocations.insert(id.clone(), allocation.clone());
        if let Err(error) = persist(&self.path, &inner) {
            match previous {
                Some(previous) => {
                    inner.allocations.insert(id, previous);
                }
                None => {
                    inner.allocations.remove(&id);
                }
            }
            return Err(error);
        }
        Ok(allocation)
    }

    /// Merge an authenticated peer patch. A manager may author fleet policy or
    /// any allocation; an ordinary member may author only its own device.
    pub fn merge(
        &self,
        sender: &str,
        sender_may_manage: bool,
        policy: Option<PolicyRecord>,
        allocations: Vec<StorageAllocation>,
    ) -> bool {
        if sender.is_empty() || allocations.len() > MAX_ALLOCATIONS {
            return false;
        }
        let mut inner = self.inner.lock();
        let previous = inner.clone();
        let mut changed = false;
        if sender_may_manage {
            if let Some(policy) = policy.filter(valid_policy_record) {
                // Managers may relay records; members must remain original authors.
                if policy.stamp > inner.policy.stamp {
                    inner.policy = policy;
                    changed = true;
                }
            }
        }
        for allocation in allocations {
            let authorized = sender_may_manage
                || (allocation.device == sender && allocation.stamp.actor == sender);
            if !authorized || !valid_allocation(&allocation) {
                continue;
            }
            let newer = inner
                .allocations
                .get(&allocation.id)
                .is_none_or(|current| allocation.stamp > current.stamp);
            if newer
                && (inner.allocations.contains_key(&allocation.id)
                    || inner.allocations.len() < MAX_ALLOCATIONS)
            {
                inner
                    .counters
                    .entry(allocation.stamp.actor.clone())
                    .and_modify(|counter| *counter = (*counter).max(allocation.stamp.counter))
                    .or_insert(allocation.stamp.counter);
                inner.allocations.insert(allocation.id.clone(), allocation);
                changed = true;
            }
        }
        if changed && persist(&self.path, &inner).is_err() {
            *inner = previous;
            return false;
        }
        changed
    }

    #[cfg(test)]
    fn memory() -> Self {
        Self::load_at(None)
    }
}

fn next_stamp(inner: &mut Persisted, actor: &str) -> Result<PlanStamp, String> {
    if actor.is_empty() || actor.len() > 512 {
        return Err("invalid storage-plan actor".into());
    }
    let observed = inner
        .allocations
        .values()
        .map(|allocation| allocation.stamp.counter)
        .chain(std::iter::once(inner.policy.stamp.counter))
        .max()
        .unwrap_or_default();
    let counter = inner
        .counters
        .get(actor)
        .copied()
        .unwrap_or_default()
        .max(observed)
        .checked_add(1)
        .ok_or("storage-plan clock exhausted")?;
    inner.counters.insert(actor.into(), counter);
    Ok(PlanStamp {
        counter,
        actor: actor.into(),
    })
}

fn allocation_id(device: &str, volume: &str) -> Result<String, String> {
    if device.is_empty()
        || volume.is_empty()
        || device.len() > 512
        || volume.len() > 512
        || device.contains('\0')
        || volume.contains('\0')
    {
        return Err("invalid storage resource identity".into());
    }
    Ok(format!("{}:{device}{volume}", device.len()))
}

fn valid_allocation(allocation: &StorageAllocation) -> bool {
    allocation_id(&allocation.device, &allocation.volume).is_ok_and(|id| id == allocation.id)
        && allocation.quota_bytes > 0
        && allocation.stamp.counter > 0
        && !allocation.stamp.actor.is_empty()
        && allocation.stamp.actor.len() <= 512
}

fn valid_policy_record(policy: &PolicyRecord) -> bool {
    validate_policy(&policy.value).is_ok()
        && (policy.stamp == PlanStamp::default()
            || (policy.stamp.counter > 0
                && !policy.stamp.actor.is_empty()
                && policy.stamp.actor.len() <= 512))
}

fn validate_policy(policy: &StoragePolicy) -> Result<(), String> {
    if !(1..=8).contains(&policy.replicas)
        || !(5..=50).contains(&policy.reserve_percent)
        || policy.version_retention_days > 3650
        || policy.rebalance_gib_per_day > 10_000
    {
        return Err("storage policy is outside its safe bounds".into());
    }
    Ok(())
}

fn persist(path: &Option<PathBuf>, value: &Persisted) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    let parent = path.parent().ok_or("storage-plan path has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create storage-plan directory: {error}"))?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("serialize fleet storage plan: {error}"))?;
    crate::persist::write_atomic(path, &bytes)
        .map_err(|error| format!("save fleet storage plan: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_and_allocations_are_separate_bounded_records() {
        let store = StoragePlanStore::memory();
        let policy = store
            .set_policy(
                "owner",
                StoragePolicy {
                    replicas: 3,
                    ..StoragePolicy::default()
                },
            )
            .unwrap();
        let allocation = store
            .set_allocation("owner", "desk".into(), "disk-1".into(), 1_000, true)
            .unwrap();
        let snapshot = store.snapshot();
        assert_eq!(snapshot.policy, policy);
        assert_eq!(snapshot.allocations, vec![allocation]);
    }

    #[test]
    fn member_cannot_edit_another_device_or_policy() {
        let store = StoragePlanStore::memory();
        let forged_policy = PolicyRecord {
            value: StoragePolicy::default(),
            stamp: PlanStamp {
                counter: 3,
                actor: "member".into(),
            },
        };
        let forged = StorageAllocation {
            id: allocation_id("other", "disk").unwrap(),
            device: "other".into(),
            volume: "disk".into(),
            quota_bytes: 1,
            enabled: true,
            stamp: PlanStamp {
                counter: 4,
                actor: "member".into(),
            },
        };
        let relayed = StorageAllocation {
            id: allocation_id("owner", "archive").unwrap(),
            device: "owner".into(),
            volume: "archive".into(),
            quota_bytes: 1,
            enabled: true,
            stamp: PlanStamp {
                counter: 5,
                actor: "owner".into(),
            },
        };
        assert!(!store.merge("member", false, Some(forged_policy), vec![forged, relayed]));
        assert!(store.snapshot().allocations.is_empty());
        assert_eq!(store.snapshot().policy, PolicyRecord::default());
    }

    #[test]
    fn manager_can_relay_records_and_older_patch_is_ignored() {
        let source = StoragePlanStore::memory();
        let policy = source
            .set_policy("owner", StoragePolicy::default())
            .unwrap();
        let allocation = source
            .set_allocation("owner", "laptop".into(), "ssd".into(), 500, true)
            .unwrap();
        let target = StoragePlanStore::memory();
        assert!(target.merge(
            "controller",
            true,
            Some(policy.clone()),
            vec![allocation.clone()]
        ));
        assert!(!target.merge("controller", true, Some(policy), vec![allocation]));
    }

    #[test]
    fn replica_and_reserve_bounds_prevent_nonsensical_policy() {
        let store = StoragePlanStore::memory();
        let bad = StoragePolicy {
            replicas: 0,
            reserve_percent: 0,
            ..StoragePolicy::default()
        };
        assert!(store.set_policy("owner", bad).is_err());
    }

    #[test]
    fn legacy_replica_policy_migrates_to_one_copy_count() {
        let legacy = serde_json::json!({
            "ordinaryReplicas": 4,
            "criticalReplicas": 6,
            "reservePercent": 10,
            "versionRetentionDays": 30,
            "rebalanceGibPerDay": 50,
            "pauseOnMetered": true
        });

        let policy: StoragePolicy = serde_json::from_value(legacy).unwrap();
        assert_eq!(policy.replicas, 4);

        let current = serde_json::to_value(policy).unwrap();
        assert_eq!(current["replicas"], 4);
        assert!(current.get("ordinaryReplicas").is_none());
        assert!(current.get("criticalReplicas").is_none());
    }

    #[test]
    fn allocation_identity_cannot_collide_on_colons() {
        assert_ne!(
            allocation_id("device:volume", "tail").unwrap(),
            allocation_id("device", "volume:tail").unwrap()
        );
    }
}
