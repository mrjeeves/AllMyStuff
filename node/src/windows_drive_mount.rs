//! Drive-letter aliases for native Cloud Files sync roots. No WebDAV client,
//! filesystem driver, mount executable, or machine-wide registry tuning.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::Storage::FileSystem::{
    DefineDosDeviceW, DDD_EXACT_MATCH_ON_REMOVE, DDD_RAW_TARGET_PATH, DDD_REMOVE_DEFINITION,
};

use crate::mesh::Mesh;
use crate::windows_cloud_files::{wide, CloudConnection};

static LEASE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
struct Lease {
    mount: String,
    root: PathBuf,
    mapping: String,
}

fn id(mapping: &str) -> String {
    format!("{:x}", Sha256::digest(mapping.as_bytes()))
}

fn root(mapping: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("ALLMYSTUFF_USER_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or("the signed-in user's home folder is unavailable")?;
    Ok(home.join("AllMyStuff Drives").join(id(mapping)))
}

fn lease_file() -> Result<PathBuf, String> {
    Ok(allmystuff_protocol::myownmesh_state_dir()
        .ok_or("AllMyStuff state is unavailable")?
        .join("allmystuff-cloud-drives.json"))
}

fn leases() -> Result<Vec<Lease>, String> {
    match std::fs::read(lease_file()?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("read native drive ownership: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn save(leases: &[Lease]) -> Result<(), String> {
    crate::persist::write_atomic(
        &lease_file()?,
        &serde_json::to_vec(leases).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn target(root: &Path) -> String {
    let root = root.to_string_lossy();
    format!(r"\??\{}", root.strip_prefix(r"\\?\").unwrap_or(&root))
}

pub(super) fn alias(mount: &str, root: &Path, remove: bool) -> Result<bool, String> {
    let mount = mount.to_string();
    let expected = target(root);
    crate::win_privilege::interactive_user_call("ams-cloud-drive-alias", move || {
        let targets = crate::win_privilege::dos_device_targets(&mount)?;
        if remove
            && !targets
                .iter()
                .any(|value| value.eq_ignore_ascii_case(&expected))
        {
            return Ok(false);
        }
        if !remove && !targets.is_empty() {
            return Err(format!(
                "{mount} is already assigned; its existing drive was preserved"
            ));
        }
        let flags = DDD_RAW_TARGET_PATH
            | if remove {
                DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE
            } else {
                0
            };
        if unsafe { DefineDosDeviceW(flags, wide(&mount).as_ptr(), wide(&expected).as_ptr()) } == 0
        {
            return Err(format!(
                "Windows couldn't {} the native drive alias: {}",
                if remove { "remove" } else { "create" },
                std::io::Error::last_os_error()
            ));
        }
        Ok(true)
    })
}

pub(super) fn remove_known(mount: &str) -> Result<bool, String> {
    let _guard = LEASE_LOCK.lock();
    let mut saved = leases()?;
    let mut removed = false;
    for lease in &saved {
        if lease.mount.eq_ignore_ascii_case(mount) && lease.root == root(&lease.mapping)? {
            removed |= alias(&lease.mount, &lease.root, true)?;
        }
    }
    saved.retain(|lease| !lease.mount.eq_ignore_ascii_case(mount));
    save(&saved)?;
    Ok(removed)
}

pub(super) fn cleanup() -> Result<(), String> {
    for lease in leases()? {
        remove_known(&lease.mount)?;
    }
    Ok(())
}

pub(super) struct NativeCloudDrive {
    connection: Option<CloudConnection>,
    mount: String,
}

impl NativeCloudDrive {
    pub(super) async fn start(
        mesh: Arc<Mesh>,
        route: String,
        mount: String,
        label: String,
        mapping: &str,
    ) -> Result<Self, String> {
        if mapping.is_empty() {
            return Err("a native drive requires a stable mapping identity".into());
        }
        let root = root(mapping)?;
        let connection = CloudConnection::connect(
            mesh,
            root.clone(),
            Some(route),
            format!("Drive-{}", id(mapping)),
            label.clone(),
        )
        .await?;
        {
            let _guard = LEASE_LOCK.lock();
            let mut saved = leases()?;
            saved.retain(|lease| !lease.mount.eq_ignore_ascii_case(&mount));
            saved.push(Lease {
                mount: mount.clone(),
                root: root.clone(),
                mapping: mapping.to_string(),
            });
            save(&saved)?;
        }
        alias(&mount, &root, false)?;
        let drive = Self {
            connection: Some(connection),
            mount: mount.clone(),
        };
        let key = format!(
            r"{}\Software\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\{}\DefaultLabel",
            super::drive_registry_root()?,
            mount.trim_end_matches(':')
        );
        let _ = crate::child_process::command("reg.exe")
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", &label, "/f"])
            .output()
            .await;
        tracing::info!(%mount,root=%root.display(),"native Cloud Files drive mounted");
        Ok(drive)
    }
}

impl Drop for NativeCloudDrive {
    fn drop(&mut self) {
        if let Err(error) = remove_known(&self.mount) {
            tracing::warn!(%error,"couldn't remove native Cloud Files drive alias");
        }
        self.connection.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alias_uses_exact_native_path_and_stable_mapping_identity() {
        assert_eq!(
            target(Path::new(r"C:\Users\Chris\AllMyStuff Drives\one")),
            r"\??\C:\Users\Chris\AllMyStuff Drives\one"
        );
        assert_eq!(id("same mapping"), id("same mapping"));
        assert_ne!(id("same mapping"), id("another mapping"));
    }
}
