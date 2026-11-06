//! Small, durable fleet storage policy.
//!
//! This is deliberately separate from the Files canvas: allocations and
//! durability policy are operational authority, not layout metadata. The plan
//! is bounded by fleet devices/volumes. Explicit choices outrank starter
//! defaults; records from the same origin converge as per-record LWW state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const MAX_ALLOCATIONS: usize = 512;
const MAX_DEVICE_INTENTS: usize = 512;
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
    /// A starter contribution yields to every explicit allocation choice on
    /// this device, even when that choice arrives later from another peer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub automatic: bool,
    pub stamp: PlanStamp,
}

fn is_false(value: &bool) -> bool {
    !*value
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceServiceRole {
    #[default]
    Automatic,
    AlwaysOn,
    Personal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceServiceIntent {
    pub device: String,
    pub role: DeviceServiceRole,
    pub stamp: PlanStamp,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoragePlanSnapshot {
    pub policy: PolicyRecord,
    pub allocations: Vec<StorageAllocation>,
    pub device_intents: Vec<DeviceServiceIntent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoragePlanMessage {
    Patch {
        policy: Option<PolicyRecord>,
        allocations: Vec<StorageAllocation>,
        #[serde(default)]
        device_intents: Vec<DeviceServiceIntent>,
    },
    Digest {
        digest: String,
    },
    EffectiveDigest {
        digest: String,
    },
    SyncRequest,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    policy: PolicyRecord,
    allocations: BTreeMap<String, StorageAllocation>,
    device_intents: BTreeMap<String, DeviceServiceIntent>,
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
        inner
            .device_intents
            .retain(|device, intent| device == &intent.device && valid_device_intent(intent));
        if inner.allocations.len() > MAX_ALLOCATIONS {
            inner.allocations = inner
                .allocations
                .into_iter()
                .take(MAX_ALLOCATIONS)
                .collect();
        }
        if inner.device_intents.len() > MAX_DEVICE_INTENTS {
            inner.device_intents = inner
                .device_intents
                .into_iter()
                .take(MAX_DEVICE_INTENTS)
                .collect();
        }
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    pub fn snapshot(&self) -> StoragePlanSnapshot {
        let inner = self.inner.lock();
        let explicit_devices = inner
            .allocations
            .values()
            .filter(|allocation| !allocation.automatic)
            .map(|allocation| allocation.device.as_str())
            .collect::<BTreeSet<_>>();
        StoragePlanSnapshot {
            policy: inner.policy.clone(),
            allocations: inner
                .allocations
                .values()
                .filter(|allocation| {
                    !allocation.automatic || !explicit_devices.contains(allocation.device.as_str())
                })
                .cloned()
                .collect(),
            device_intents: inner.device_intents.values().cloned().collect(),
        }
    }

    pub fn digest(&self) -> String {
        // Only effective records are synchronized. Hidden starter records and
        // local clock history must not keep otherwise identical plans apart.
        plan_digest(&self.snapshot())
    }

    /// Preserve the original wire digest for peers that do not negotiate
    /// effective-plan digests. Its bytes include raw records and clock state.
    pub fn legacy_digest(&self) -> String {
        plan_digest(&*self.inner.lock())
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

    /// Seed at most one contribution on this device. The guard and insertion
    /// share the plan lock so a concurrent explicit edit or peer merge wins
    /// without a check-then-write gap. Disabled records are deliberate choices.
    pub fn ensure_default_allocation(
        &self,
        actor: &str,
        volume: String,
        quota_bytes: u64,
    ) -> Result<Option<StorageAllocation>, String> {
        let id = allocation_id(actor, &volume)?;
        if quota_bytes == 0 {
            return Err("invalid storage allocation".into());
        }
        let mut inner = self.inner.lock();
        if inner
            .allocations
            .values()
            .any(|allocation| allocation.device == actor)
        {
            return Ok(None);
        }
        if inner.allocations.len() >= MAX_ALLOCATIONS {
            return Err("the fleet storage plan has too many allocations".into());
        }
        let previous = inner.clone();
        let stamp = next_stamp(&mut inner, actor)?;
        let allocation = StorageAllocation {
            id: id.clone(),
            device: actor.to_string(),
            volume,
            quota_bytes,
            enabled: true,
            automatic: true,
            stamp,
        };
        inner.allocations.insert(id, allocation.clone());
        if let Err(error) = persist(&self.path, &inner) {
            *inner = previous;
            return Err(error);
        }
        Ok(Some(allocation))
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
            automatic: false,
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
    pub fn set_device_intent(
        &self,
        actor: &str,
        device: String,
        role: DeviceServiceRole,
    ) -> Result<DeviceServiceIntent, String> {
        valid_device(&device)?;
        let mut inner = self.inner.lock();
        if !inner.device_intents.contains_key(&device)
            && inner.device_intents.len() >= MAX_DEVICE_INTENTS
        {
            return Err("the fleet storage plan has too many device roles".into());
        }
        let stamp = next_stamp(&mut inner, actor)?;
        let intent = DeviceServiceIntent {
            device: device.clone(),
            role,
            stamp,
        };
        let previous = inner.device_intents.insert(device.clone(), intent.clone());
        if let Err(error) = persist(&self.path, &inner) {
            match previous {
                Some(previous) => {
                    inner.device_intents.insert(device, previous);
                }
                None => {
                    inner.device_intents.remove(&device);
                }
            }
            return Err(error);
        }
        Ok(intent)
    }

    /// Merge an authenticated peer patch. A manager may author fleet policy or
    /// any allocation; an ordinary member may author only its own device.
    pub fn merge(
        &self,
        sender: &str,
        sender_may_manage: bool,
        policy: Option<PolicyRecord>,
        allocations: Vec<StorageAllocation>,
        device_intents: Vec<DeviceServiceIntent>,
    ) -> bool {
        if sender.is_empty()
            || allocations.len() > MAX_ALLOCATIONS
            || device_intents.len() > MAX_DEVICE_INTENTS
        {
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
            let newer = inner.allocations.get(&allocation.id).is_none_or(|current| {
                match (allocation.automatic, current.automatic) {
                    (false, true) => true,
                    (true, false) => false,
                    _ => allocation.stamp > current.stamp,
                }
            });
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
        for intent in device_intents {
            let authorized =
                sender_may_manage || (intent.device == sender && intent.stamp.actor == sender);
            if !authorized || !valid_device_intent(&intent) {
                continue;
            }
            let newer = inner
                .device_intents
                .get(&intent.device)
                .is_none_or(|current| intent.stamp > current.stamp);
            if newer
                && (inner.device_intents.contains_key(&intent.device)
                    || inner.device_intents.len() < MAX_DEVICE_INTENTS)
            {
                inner
                    .counters
                    .entry(intent.stamp.actor.clone())
                    .and_modify(|counter| *counter = (*counter).max(intent.stamp.counter))
                    .or_insert(intent.stamp.counter);
                inner.device_intents.insert(intent.device.clone(), intent);
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

fn plan_digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn next_stamp(inner: &mut Persisted, actor: &str) -> Result<PlanStamp, String> {
    if actor.is_empty() || actor.len() > 512 {
        return Err("invalid storage-plan actor".into());
    }
    let observed = inner
        .allocations
        .values()
        .map(|allocation| allocation.stamp.counter)
        .chain(
            inner
                .device_intents
                .values()
                .map(|intent| intent.stamp.counter),
        )
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

fn valid_device(device: &str) -> Result<(), String> {
    if device.is_empty() || device.len() > 512 || device.contains('\0') {
        return Err("invalid device identity".into());
    }
    Ok(())
}

fn valid_device_intent(intent: &DeviceServiceIntent) -> bool {
    valid_device(&intent.device).is_ok()
        && intent.stamp.counter > 0
        && !intent.stamp.actor.is_empty()
        && intent.stamp.actor.len() <= 512
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

    const STARTER_BYTES: u64 = 1024 * 1024 * 1024;

    #[test]
    fn legacy_digest_keeps_the_original_raw_persisted_byte_contract() {
        let store = StoragePlanStore::memory();
        store
            .ensure_default_allocation("desktop", "state-volume".into(), STARTER_BYTES)
            .unwrap();
        store
            .set_allocation(
                "manager",
                "desktop".into(),
                "other-volume".into(),
                STARTER_BYTES,
                false,
            )
            .unwrap();
        let bytes = serde_json::to_vec(&*store.inner.lock()).unwrap();
        // This is the digest algorithm used before effective-plan support.
        let expected = bytes
            .into_iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        assert_eq!(store.legacy_digest(), format!("{expected:016x}"));
        assert_ne!(store.legacy_digest(), store.digest());
    }

    #[test]
    fn effective_digest_has_a_distinct_negotiated_wire_kind() {
        let legacy = StoragePlanMessage::Digest {
            digest: "legacy".into(),
        };
        let effective = StoragePlanMessage::EffectiveDigest {
            digest: "effective".into(),
        };
        let legacy = serde_json::to_value(legacy).unwrap();
        let effective = serde_json::to_value(effective).unwrap();
        assert_eq!(legacy["kind"], "digest");
        assert_eq!(effective["kind"], "effective_digest");
        assert!(matches!(
            serde_json::from_value::<StoragePlanMessage>(legacy).unwrap(),
            StoragePlanMessage::Digest { digest } if digest == "legacy",
        ));
        assert!(matches!(
            serde_json::from_value::<StoragePlanMessage>(effective).unwrap(),
            StoragePlanMessage::EffectiveDigest { digest } if digest == "effective",
        ));
    }

    #[test]
    fn starter_is_once_per_device_and_existing_choices_prevent_seeding() {
        let store = StoragePlanStore::memory();
        let automatic = store
            .ensure_default_allocation("desktop", "state-volume".into(), STARTER_BYTES)
            .unwrap()
            .unwrap();
        assert!(automatic.automatic && automatic.enabled);
        assert_eq!(automatic.quota_bytes, STARTER_BYTES);
        assert!(store
            .ensure_default_allocation("desktop", "other-volume".into(), STARTER_BYTES)
            .unwrap()
            .is_none());
        assert_eq!(store.snapshot().allocations, vec![automatic]);

        for enabled in [false, true] {
            let store = StoragePlanStore::memory();
            let explicit = store
                .set_allocation(
                    "manager",
                    "desktop".into(),
                    "other-volume".into(),
                    7 * STARTER_BYTES,
                    enabled,
                )
                .unwrap();
            assert!(store
                .ensure_default_allocation("desktop", "state-volume".into(), STARTER_BYTES)
                .unwrap()
                .is_none());
            assert_eq!(store.snapshot().allocations, vec![explicit]);
        }
    }

    #[test]
    fn simultaneous_starter_attempts_create_only_one_allocation() {
        let store = std::sync::Arc::new(StoragePlanStore::memory());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let attempts = ["first-volume", "second-volume"].map(|volume| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .ensure_default_allocation("desktop", volume.into(), STARTER_BYTES)
                    .unwrap()
            })
        });
        barrier.wait();
        assert_eq!(
            attempts
                .into_iter()
                .filter_map(|attempt| attempt.join().unwrap())
                .count(),
            1,
        );
        assert_eq!(store.snapshot().allocations.len(), 1);
    }

    #[test]
    fn late_explicit_choices_override_starters_in_either_merge_order() {
        for explicit_volume in ["state-volume", "other-volume"] {
            for enabled in [false, true] {
                let source = StoragePlanStore::memory();
                let mut automatic = source
                    .ensure_default_allocation("desktop", "state-volume".into(), STARTER_BYTES)
                    .unwrap()
                    .unwrap();
                automatic.stamp.counter = 100;
                let choices = StoragePlanStore::memory();
                let explicit = choices
                    .set_allocation(
                        "manager",
                        "desktop".into(),
                        explicit_volume.into(),
                        7 * STARTER_BYTES,
                        enabled,
                    )
                    .unwrap();
                assert!(explicit.stamp.counter < automatic.stamp.counter);
                for automatic_first in [false, true] {
                    let store = StoragePlanStore::memory();
                    let records = if automatic_first {
                        [automatic.clone(), explicit.clone()]
                    } else {
                        [explicit.clone(), automatic.clone()]
                    };
                    for record in records {
                        store.merge("manager", true, None, vec![record], Vec::new());
                    }
                    assert_eq!(store.snapshot().allocations, vec![explicit.clone()]);
                    assert_eq!(store.digest(), choices.digest());
                    assert!(store
                        .ensure_default_allocation("desktop", "third-volume".into(), STARTER_BYTES)
                        .unwrap()
                        .is_none());
                    if explicit_volume != automatic.volume {
                        assert_eq!(store.inner.lock().allocations.len(), 2);
                    }
                }
            }
        }
    }

    #[test]
    fn explicit_edit_of_starter_is_manual_and_survives_stale_automatic_patch() {
        let store = StoragePlanStore::memory();
        let mut old_automatic = store
            .ensure_default_allocation("desktop", "state-volume".into(), STARTER_BYTES)
            .unwrap()
            .unwrap();
        let explicit = store
            .set_allocation(
                "desktop",
                "desktop".into(),
                "state-volume".into(),
                STARTER_BYTES / 2,
                false,
            )
            .unwrap();
        assert!(!explicit.automatic);
        old_automatic.stamp.counter = explicit.stamp.counter + 100;
        assert!(!store.merge("desktop", false, None, vec![old_automatic], Vec::new(),));
        assert_eq!(store.snapshot().allocations, vec![explicit]);
    }

    #[test]
    fn starter_and_suppressed_history_survive_reload_without_reseeding() {
        let directory = std::env::temp_dir().join(format!(
            "amst-storage-default-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = directory.join("plan.json");
        let store = StoragePlanStore::load_at(Some(path.clone()));
        let automatic = store
            .ensure_default_allocation("desktop", "state-volume".into(), STARTER_BYTES)
            .unwrap()
            .unwrap();
        drop(store);
        let store = StoragePlanStore::load_at(Some(path.clone()));
        assert_eq!(store.snapshot().allocations, vec![automatic.clone()]);
        assert!(store
            .ensure_default_allocation("desktop", "other-volume".into(), STARTER_BYTES)
            .unwrap()
            .is_none());
        let explicit = store
            .set_allocation(
                "manager",
                "desktop".into(),
                "other-volume".into(),
                STARTER_BYTES,
                false,
            )
            .unwrap();
        drop(store);
        let store = StoragePlanStore::load_at(Some(path));
        assert_eq!(store.snapshot().allocations, vec![explicit]);
        assert_eq!(store.inner.lock().allocations.len(), 2);
        assert!(store
            .ensure_default_allocation("desktop", "third-volume".into(), STARTER_BYTES)
            .unwrap()
            .is_none());
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_allocations_remain_manual_and_omit_the_default_flag() {
        let store = StoragePlanStore::memory();
        let explicit = store
            .set_allocation("desktop", "desktop".into(), "disk".into(), 1, false)
            .unwrap();
        let legacy = serde_json::to_value(&explicit).unwrap();
        assert!(legacy.get("automatic").is_none());
        assert_eq!(
            serde_json::from_value::<StorageAllocation>(legacy).unwrap(),
            explicit,
        );
        let automatic = StoragePlanStore::memory()
            .ensure_default_allocation("desktop", "disk".into(), STARTER_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(automatic).unwrap()["automatic"], true);
    }

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
    fn device_role_is_durable_cleanly_serialized_and_backward_compatible() {
        let store = StoragePlanStore::memory();
        let intent = store
            .set_device_intent("owner", "server".into(), DeviceServiceRole::AlwaysOn)
            .unwrap();
        assert_eq!(store.snapshot().device_intents, vec![intent.clone()]);
        assert_eq!(
            serde_json::to_value(intent.role).unwrap(),
            serde_json::json!("alwaysOn")
        );

        let legacy = serde_json::json!({
            "policy": PolicyRecord::default(),
            "allocations": {},
            "counters": {}
        });
        let persisted: Persisted = serde_json::from_value(legacy).unwrap();
        assert!(persisted.device_intents.is_empty());
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
            automatic: false,
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
            automatic: false,
            stamp: PlanStamp {
                counter: 5,
                actor: "owner".into(),
            },
        };
        assert!(!store.merge(
            "member",
            false,
            Some(forged_policy),
            vec![forged, relayed],
            Vec::new()
        ));
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
        let intent = source
            .set_device_intent("owner", "laptop".into(), DeviceServiceRole::Personal)
            .unwrap();
        let target = StoragePlanStore::memory();
        assert!(target.merge(
            "controller",
            true,
            Some(policy.clone()),
            vec![allocation.clone()],
            vec![intent.clone()]
        ));
        assert!(!target.merge(
            "controller",
            true,
            Some(policy),
            vec![allocation],
            vec![intent.clone()]
        ));
        assert_eq!(target.snapshot().device_intents, vec![intent]);
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
