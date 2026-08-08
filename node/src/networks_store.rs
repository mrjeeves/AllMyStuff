//! Disabled networks — the persisted half of the network pill's
//! enable/disable toggle.
//!
//! The MyOwnMesh daemon has no notion of a network that's configured but
//! dormant: a network is either in its config.json (joined on every start)
//! or gone. "Disable" therefore means *leave the daemon but keep the
//! ticket*: the full `NetworkConfig` is parked here — under the same
//! `~/.myownmesh` home as the rest of AllMyStuff's state — and re-enabling
//! hands the very same config back to `network_add`. Nothing else is lost
//! in between: the network's roster file lives on disk keyed by network id
//! and survives the daemon-side remove, so approved devices are still
//! approved when the network comes back.

use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The on-disk shape. Additive + `#[serde(default)]` so an older file (or
/// none) still loads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    /// Full daemon `NetworkConfig` values, exactly as `config_show`
    /// reported them at disable time — kept opaque so daemon fields this
    /// build doesn't know survive the round trip.
    #[serde(default)]
    disabled: Vec<Value>,
}

/// The live store. Cheap to share behind the Tauri state.
pub struct DisabledNetworks {
    path: Option<PathBuf>,
    inner: Mutex<Persisted>,
}

impl DisabledNetworks {
    /// Load the parked configs from disk (or start empty). A corrupt file is
    /// quarantined aside and loads empty, loudly — a silent empty here makes
    /// every parked network vanish from the UI (and turns the local claiming
    /// mesh back on, since "off" is exactly its presence in this store).
    pub fn load() -> Self {
        let path = store_path();
        let inner: Persisted = path
            .as_ref()
            .map(|p| crate::persist::load_json(p))
            .unwrap_or_default();
        DisabledNetworks {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// Every parked config, for the pill menu's "disabled" rows.
    pub fn list(&self) -> Vec<Value> {
        self.inner.lock().disabled.clone()
    }

    /// Park a config (keyed by its `id`/`network_id`; parking the same
    /// network twice replaces the older copy). Returns whether the
    /// durable write succeeded — a failed write rolls back and must abort
    /// the disable, or the network would be unrecoverable from the UI.
    pub fn park(&self, config: Value) -> bool {
        let mut inner = self.inner.lock();
        let snapshot = inner.disabled.clone();
        let key = ids_of(&config);
        inner.disabled.retain(|c| ids_of(c) != key);
        inner.disabled.push(config);
        if persist(&self.path, &inner) {
            true
        } else {
            inner.disabled = snapshot;
            false
        }
    }

    /// Whether a config is parked here. `key` matches either id, same as
    /// [`DisabledNetworks::take`].
    pub fn contains(&self, key: &str) -> bool {
        self.inner.lock().disabled.iter().any(|c| {
            let (id, net) = ids_of(c);
            id == key || net == key
        })
    }

    /// Take a parked config back out (for re-enable). `key` may be either
    /// the local config id or the wire-level network id.
    pub fn take(&self, key: &str) -> Option<Value> {
        let mut inner = self.inner.lock();
        let idx = inner.disabled.iter().position(|c| {
            let (id, net) = ids_of(c);
            id == key || net == key
        })?;
        let cfg = inner.disabled.remove(idx);
        if !persist(&self.path, &inner) {
            // Couldn't record the removal — put it back so the store never
            // disagrees with disk in the dangerous direction (a config
            // both live *and* parked is merely cosmetic; one in neither
            // place is lost).
            inner.disabled.insert(idx, cfg);
            return None;
        }
        Some(cfg)
    }
}

fn ids_of(config: &Value) -> (String, String) {
    let get = |k: &str| {
        config
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (get("id"), get("network_id"))
}

fn persist(path: &Option<PathBuf>, value: &Persisted) -> bool {
    let Some(path) = path else {
        return false;
    };
    let Ok(json) = serde_json::to_string_pretty(value) else {
        return false;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    crate::persist::write_atomic(path, json.as_bytes()).is_ok()
}

/// Same home as the rest of AllMyStuff's persisted state (and the mesh
/// identity): `~/.myownmesh`, overridable via `MYOWNMESH_HOME`.
fn store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-networks.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn memory() -> DisabledNetworks {
        DisabledNetworks {
            path: None,
            inner: Mutex::new(Persisted::default()),
        }
    }

    #[test]
    fn park_and_take_round_trip_by_either_id() {
        let dir = std::env::temp_dir().join(format!("ams-netstore-{}", std::process::id()));
        let store = DisabledNetworks {
            path: Some(dir.join("allmystuff-networks.json")),
            inner: Mutex::new(Persisted::default()),
        };
        let cfg = json!({ "id": "net_1", "network_id": "home-abc", "label": "Home" });
        assert!(store.park(cfg.clone()));
        assert_eq!(store.list().len(), 1);
        assert!(store.contains("net_1"));
        assert!(store.contains("home-abc"));
        assert!(!store.contains("elsewhere"));

        // Parking the same network again replaces, never duplicates.
        assert!(store.park(cfg.clone()));
        assert_eq!(store.list().len(), 1);

        // Take by network id works as well as config id.
        let back = store.take("home-abc").expect("parked config comes back");
        assert_eq!(back["label"], "Home");
        assert!(store.list().is_empty());
        assert!(store.take("net_1").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_store_path_fails_park_safely() {
        let store = memory();
        let cfg = json!({ "id": "net_1", "network_id": "home-abc" });
        // No durable home → the park reports failure (and rolls back) so
        // the caller never removes a network it couldn't save.
        assert!(!store.park(cfg));
        assert!(store.list().is_empty());
    }
}
