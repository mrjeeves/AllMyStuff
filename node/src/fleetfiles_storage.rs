//! Local allocation setup. Failure retries are rate-limited across file traffic;
//! storage discovery must never run the full hardware inventory probe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::storage_plan::StorageAllocation;

const INITIAL_RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(300);

struct FailedRoot {
    allocation: StorageAllocation,
    error: String,
    delay: Duration,
    retry_at: Instant,
}

#[derive(Default)]
pub(crate) struct StorageRootRetries {
    failed: HashMap<String, FailedRoot>,
}

impl StorageRootRetries {
    pub(crate) fn retain(&mut self, allocations: &[StorageAllocation]) {
        self.failed
            .retain(|id, _| allocations.iter().any(|a| a.enabled && a.id == *id));
    }

    /// Only new/changed errors are returned for logging. An unchanged failure
    /// backs off to five minutes; a changed allocation gets an immediate try.
    pub(crate) fn resolve(
        &mut self,
        allocation: &StorageAllocation,
        now: Instant,
        materialize: impl FnOnce() -> Result<PathBuf, String>,
    ) -> Result<Option<PathBuf>, String> {
        let previous = self
            .failed
            .get(&allocation.id)
            .filter(|failure| failure.allocation == *allocation);
        if previous.is_some_and(|failure| now < failure.retry_at) {
            return Ok(None);
        }
        let result = materialize();
        match result {
            Ok(root) => {
                if self.failed.remove(&allocation.id).is_some() {
                    tracing::info!("Fleetfiles storage allocation {} recovered", allocation.id);
                }
                Ok(Some(root))
            }
            Err(error) => {
                let report = previous.is_none_or(|failure| failure.error != error);
                let delay =
                    previous.map_or(INITIAL_RETRY, |failure| (failure.delay * 2).min(MAX_RETRY));
                self.failed.insert(
                    allocation.id.clone(),
                    FailedRoot {
                        allocation: allocation.clone(),
                        error: error.clone(),
                        delay,
                        retry_at: now + delay,
                    },
                );
                if report {
                    Err(error)
                } else {
                    Ok(None)
                }
            }
        }
    }
}

pub(crate) fn storage_mounts() -> HashMap<String, PathBuf> {
    sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .map(|disk| {
            let mount = disk.mount_point();
            (
                format!("disk:{}", mount.to_string_lossy()),
                mount.to_path_buf(),
            )
        })
        .collect()
}

pub(crate) fn materialize_root(mount: &Path, state: Option<&Path>) -> Result<PathBuf, String> {
    materialize_root_with(mount, state, |path| std::fs::create_dir_all(path))
}

fn materialize_root_with(
    mount: &Path,
    state: Option<&Path>,
    mut create: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<PathBuf, String> {
    let preferred = mount.join(".allmystuff-fleetfiles");
    let root = match create(&preferred) {
        Ok(()) => preferred,
        Err(primary_error) => {
            let fallback = state
                .filter(|state| same_filesystem(mount, state))
                .map(|state| state.join(".allmystuff-fleetfiles"))
                .ok_or_else(|| format!("create Fleetfiles storage root: {primary_error}"))?;
            create(&fallback).map_err(|fallback_error| {
                format!(
                    "create Fleetfiles storage root: {primary_error}; fallback: {fallback_error}"
                )
            })?;
            fallback
        }
    };
    crate::files::mark_internal_staging_hidden(&root)
        .map_err(|error| format!("hide Fleetfiles storage root: {error}"))?;
    Ok(root)
}

// macOS firmlinks expose /System/Volumes/Data/Users as /Users. Canonicalizing
// or comparing prefixes cannot identify the backing volume. st_dev also
// rejects a home on a different volume even if it is nested beneath mount.
#[cfg(unix)]
fn same_filesystem(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_filesystem(left: &Path, right: &Path) -> bool {
    // Preserve the existing fallback on platforms without Unix device IDs.
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => right.starts_with(left),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_plan::PlanStamp;

    fn allocation() -> StorageAllocation {
        StorageAllocation {
            id: "local-volume".into(),
            device: "local".into(),
            volume: "disk:test".into(),
            quota_bytes: 1024,
            enabled: true,
            stamp: PlanStamp::default(),
        }
    }

    #[test]
    fn repeated_failure_backs_off_and_logs_only_changes() {
        let mut retries = StorageRootRetries::default();
        let a = allocation();
        let now = Instant::now();
        assert_eq!(
            retries.resolve(&a, now, || Err("denied".into())),
            Err("denied".into())
        );
        assert_eq!(
            retries.resolve(&a, now + Duration::from_secs(4), || panic!(
                "retried too early"
            )),
            Ok(None)
        );
        assert_eq!(
            retries.resolve(&a, now + INITIAL_RETRY, || Err("denied".into())),
            Ok(None)
        );
        assert_eq!(
            retries.resolve(&a, now + Duration::from_secs(14), || panic!(
                "backoff did not grow"
            )),
            Ok(None)
        );
        assert_eq!(
            retries.resolve(
                &a,
                now + Duration::from_secs(15),
                || Err("unmounted".into())
            ),
            Err("unmounted".into())
        );
    }

    #[test]
    fn allocation_change_and_recovery_reset_failure_state() {
        let mut retries = StorageRootRetries::default();
        let mut a = allocation();
        let now = Instant::now();
        assert!(retries.resolve(&a, now, || Err("denied".into())).is_err());
        a.stamp.counter += 1;
        assert_eq!(
            retries.resolve(&a, now, || Ok(PathBuf::from("usable"))),
            Ok(Some(PathBuf::from("usable")))
        );
        assert!(retries.failed.is_empty());
        assert!(retries.resolve(&a, now, || Err("denied".into())).is_err());
        a.enabled = false;
        retries.retain(&[a]);
        assert!(retries.failed.is_empty());
    }

    #[test]
    fn retries_remain_bounded_and_recover_without_a_plan_change() {
        let mut retries = StorageRootRetries::default();
        let a = allocation();
        let mut now = Instant::now();
        for _ in 0..20 {
            let _ = retries.resolve(&a, now, || Err("denied".into()));
            let failure = &retries.failed[&a.id];
            assert!(failure.delay <= MAX_RETRY);
            now = failure.retry_at;
        }
        assert_eq!(retries.failed[&a.id].delay, MAX_RETRY);
        assert!(retries
            .resolve(&a, now, || Ok(PathBuf::from("usable")))
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn denied_root_falls_back_to_same_device_without_path_prefix() {
        let base = std::env::temp_dir().join(format!("amst-storage-{}", std::process::id()));
        let mount = base.join("mount");
        let state = base.join("home/state");
        std::fs::create_dir_all(&mount).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        assert!(!state.starts_with(&mount));
        let root = materialize_root_with(&mount, Some(&state), |path| {
            if path == mount.join(".allmystuff-fleetfiles") {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                std::fs::create_dir_all(path)
            }
        })
        .unwrap();
        assert_eq!(root, state.join(".allmystuff-fleetfiles"));
        assert!(root.is_dir());
        assert!(!same_filesystem(&mount, &base.join("missing")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_data_volume_and_home_have_matching_device_ids() {
        assert!(same_filesystem(
            Path::new("/System/Volumes/Data"),
            Path::new("/Users")
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn fallback_rejects_another_filesystem() {
        let state = std::env::temp_dir();
        let mount = Path::new("/dev");
        assert!(!same_filesystem(mount, &state));
        let mut attempts = 0;
        assert!(materialize_root_with(mount, Some(&state), |_| {
            attempts += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .is_err());
        assert_eq!(attempts, 1, "must not create fallback on the wrong volume");
    }
}
