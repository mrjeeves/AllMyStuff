//! Shared folders — the durable answer to "which folders of mine may other
//! people open, and where do they live?"
//!
//! Sharing a *folder* is the file half of a share done properly. The whole-
//! machine `…:files` console is owner/fleet-only and stays that way; what a
//! person outside the fleet gets is one named folder, and nothing else on the
//! disk.
//!
//! The load-bearing rule is that **the path never crosses the wire**. A peer
//! opening a shared folder names its *id* — minted here, meaningless anywhere
//! else — and this registry is the only thing that turns that id back into a
//! path, on the machine that owns the disk. A receiver that could name a root
//! could name `/`, which is exactly the difference between a folder share and
//! handing over the machine.
//!
//! (Contrast the existing mapped-drive *pull*, where the receiver does supply
//! a root: that path is owner/fleet-gated — you're pulling from your own
//! machine — so naming a path is no more than you could already do.)
//!
//! Ids are minted, not derived from the path, so a folder keeps its identity
//! when it's renamed and a grant over it survives the rename with it. It also
//! means a path can't be guessed back out of an id someone saw.

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// One folder this machine has shared.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedFolder {
    /// Minted, opaque, and the only name for this folder that ever leaves the
    /// machine — it's what a capability id (`<node>:folder:<id>`) carries and
    /// what a grant is pinned to.
    pub id: String,
    /// Absolute path on *this* machine. Never sent to a peer.
    pub path: PathBuf,
    /// What the other side sees in its file manager.
    pub label: String,
}

/// The durable part of the record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    folders: Vec<SharedFolder>,
}

/// The live registry. Cheap to share behind an `Arc`.
pub struct Folders {
    path: Option<PathBuf>,
    inner: Mutex<Vec<SharedFolder>>,
}

impl Folders {
    /// Load the record from disk (or start blank).
    pub fn load() -> Self {
        Self::load_at(store_path())
    }

    /// Load from an explicit path (`None` = in-memory, every save a no-op
    /// "ok"). The seam the tests use.
    fn load_at(path: Option<PathBuf>) -> Self {
        let persisted: Persisted = path
            .as_ref()
            .map(|p| crate::persist::load_json(p))
            .unwrap_or_default();
        Folders {
            path,
            inner: Mutex::new(persisted.folders),
        }
    }

    /// Every shared folder — what the GUI lists, and what a share builder
    /// picks from.
    pub fn list(&self) -> Vec<SharedFolder> {
        self.inner.lock().clone()
    }

    /// Share `path` under `label`, returning the record.
    ///
    /// Sharing the same path twice returns the **existing** folder rather than
    /// minting a second id for it: a folder has one identity, and re-picking it
    /// in the builder must not orphan the grants already pinned to the first id.
    /// A non-empty label refreshes.
    pub fn share(&self, path: PathBuf, label: String) -> SharedFolder {
        let mut folders = self.inner.lock();
        if let Some(existing) = folders.iter_mut().find(|f| f.path == path) {
            if !label.trim().is_empty() {
                existing.label = label;
            }
            let out = existing.clone();
            drop(folders);
            self.persist();
            return out;
        }
        let label = if label.trim().is_empty() {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Shared folder".into())
        } else {
            label
        };
        let folder = SharedFolder {
            id: mint_id(),
            path,
            label,
        };
        folders.push(folder.clone());
        drop(folders);
        self.persist();
        folder
    }

    /// Stop sharing a folder. The grants pinned to its capability are the
    /// caller's to revoke — dropping the folder alone already denies every
    /// open, since [`Self::root_for`] is the only way back to a path.
    pub fn unshare(&self, id: &str) -> bool {
        let mut folders = self.inner.lock();
        let before = folders.len();
        folders.retain(|f| f.id != id);
        let removed = folders.len() != before;
        drop(folders);
        if removed {
            self.persist();
        }
        removed
    }

    /// The path behind a folder id — the *only* id→path resolution there is,
    /// and it runs on the machine that owns the disk.
    ///
    /// A folder whose path has since gone (unplugged, deleted, renamed
    /// out from under us) resolves to `None` rather than to a stale path, so
    /// an open fails cleanly instead of rooting a session somewhere that no
    /// longer means what the user shared.
    pub fn root_for(&self, id: &str) -> Option<PathBuf> {
        let path = self
            .inner
            .lock()
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.path.clone())?;
        path.is_dir().then_some(path)
    }

    fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let persisted = Persisted {
            folders: self.inner.lock().clone(),
        };
        match serde_json::to_string_pretty(&persisted) {
            Ok(json) => {
                if crate::persist::write_atomic(path, json.as_bytes()).is_err() {
                    tracing::error!("couldn't save the shared-folder record");
                }
            }
            Err(e) => tracing::error!("couldn't encode the shared-folder record: {e}"),
        }
    }
}

/// The capability id a shared folder is offered under. Shape-as-contract, the
/// way `…:files` and `…:terminal` already are — [`folder_id_of`] is the only
/// reader.
pub fn folder_capability(node: &str, id: &str) -> String {
    format!("{node}:folder:{id}")
}

/// The folder id inside a `<node>:folder:<id>` capability, if that's what this
/// is. `None` for every other capability shape.
pub fn folder_id_of(capability: &str) -> Option<&str> {
    let (_, rest) = capability.split_once(":folder:")?;
    (!rest.is_empty() && !rest.contains(':')).then_some(rest)
}

/// 128 bits of randomness, hex. Unguessable on purpose: an id is a capability
/// name, and while a grant is still required to open one, an id nobody can
/// guess means a stale grant can't be aimed at a folder its holder was never
/// told about.
fn mint_id() -> String {
    let mut bytes = [0u8; 16];
    // A failed RNG is not survivable for a security-relevant id — better to
    // mint nothing than something predictable, and the caller reports it.
    if getrandom::getrandom(&mut bytes).is_err() {
        tracing::error!("no randomness for a shared-folder id");
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `~/.myownmesh/allmystuff-folders.json`, honouring `MYOWNMESH_HOME` — the
/// same home the identity, ownership and shares records use.
fn store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-folders.json"))
}

/// Whether `path` is inside `root` once both are canonicalized — the check
/// that keeps a shared folder a *folder* rather than a doorway.
///
/// Canonicalizing both sides is what makes it hold: `..` is resolved away, and
/// a symlink inside the share that points outside it lands on its real target
/// and fails here. Comparing the textual paths instead would pass both.
pub fn within_root(root: &Path, path: &Path) -> bool {
    let (Ok(root), Ok(path)) = (root.canonicalize(), path.canonicalize()) else {
        return false;
    };
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Folders {
        Folders::load_at(None)
    }

    #[test]
    fn a_folder_keeps_one_identity_however_often_it_is_shared() {
        let dir = std::env::temp_dir().join(format!("ams-folders-{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = memory();

        let first = f.share(dir.clone(), "Work".into());
        assert!(!first.id.is_empty());
        // Re-sharing the same path returns the same id — a second id would
        // orphan every grant already pinned to the first.
        let again = f.share(dir.clone(), String::new());
        assert_eq!(first.id, again.id);
        assert_eq!(again.label, "Work", "an empty label doesn't clobber");
        // …and a new label refreshes in place.
        let relabelled = f.share(dir.clone(), "Projects".into());
        assert_eq!(relabelled.id, first.id);
        assert_eq!(relabelled.label, "Projects");
        assert_eq!(f.list().len(), 1);

        // Ids are unguessable and distinct per folder.
        let other = dir.join("sub");
        std::fs::create_dir_all(&other).unwrap();
        let second = f.share(other, "Sub".into());
        assert_ne!(first.id, second.id);
        assert_eq!(first.id.len(), 32, "128 bits, hex");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_live_shared_folder_resolves_to_a_path() {
        let dir = std::env::temp_dir().join(format!("ams-folders-{}", unique()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = memory();
        let folder = f.share(dir.clone(), "Work".into());

        assert_eq!(f.root_for(&folder.id).as_deref(), Some(dir.as_path()));
        // An id nobody shared resolves to nothing — this is the whole gate
        // between a folder share and the rest of the disk.
        assert!(f.root_for("deadbeef").is_none());
        assert!(f.root_for("").is_none());

        // Unshared → no path, so every open fails even if a grant lingers.
        assert!(f.unshare(&folder.id));
        assert!(f.root_for(&folder.id).is_none());
        assert!(!f.unshare(&folder.id), "idempotent");

        // A folder whose path has gone doesn't resolve to a stale path.
        let vanished = f.share(dir.join("gone"), "Gone".into());
        assert!(f.root_for(&vanished.id).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_capability_shape_round_trips_and_rejects_everything_else() {
        let cap = folder_capability("node-AB12C", "cafe1234");
        assert_eq!(cap, "node-AB12C:folder:cafe1234");
        assert_eq!(folder_id_of(&cap), Some("cafe1234"));

        // Every other capability shape on the machine must not read as a
        // folder — `:files` especially, since that one is the whole disk.
        assert_eq!(folder_id_of("node:files"), None);
        assert_eq!(folder_id_of("node:terminal"), None);
        assert_eq!(folder_id_of("node:storage-in"), None);
        assert_eq!(folder_id_of("node:folder:"), None);
        // A trailing segment would let a crafted capability smuggle something
        // past a naive id comparison.
        assert_eq!(folder_id_of("node:folder:abc:extra"), None);
    }

    #[test]
    fn within_root_resolves_escapes_rather_than_comparing_text() {
        let dir = std::env::temp_dir().join(format!("ams-folders-{}", unique()));
        let inside = dir.join("inside");
        std::fs::create_dir_all(&inside).unwrap();

        assert!(within_root(&dir, &inside));
        assert!(within_root(&dir, &dir));
        // `..` is resolved away, so a traversal out of the share fails even
        // though the string still starts with the root.
        assert!(!within_root(&inside, &inside.join("..").join("..")));
        // A path that doesn't exist can't be proven inside — fail closed.
        assert!(!within_root(&dir, &dir.join("nope")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unique() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
