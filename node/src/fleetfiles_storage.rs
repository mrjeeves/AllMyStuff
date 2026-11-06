//! Local allocation setup. Failure retries are rate-limited across file traffic;
//! storage discovery must never run the full hardware inventory probe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::storage_plan::StorageAllocation;
use allmystuff_protocol::StorageSummary;

const INITIAL_RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(300);

pub(crate) const DEFAULT_CONTRIBUTION_BYTES: u64 = 1 << 30;
const MIN_DEFAULT_FREE_BYTES: u64 = 5 << 30;

struct DefaultVolume {
    id: String,
    mount: PathBuf,
    filesystem: String,
    removable: bool,
    read_only: bool,
}

/// Choose one local volume already advertised by this device. Enumeration
/// reads mount metadata only; capacity is queried freshly on the selected
/// backing filesystem, never on every mounted network/adapter volume.
///
/// The caller must successfully materialize the allocation root before saving
/// the automatic allocation. That write also establishes actual writability;
/// filesystem flags alone cannot account for ACLs or a read-only bind mount.
pub(crate) fn choose_default_volume(volumes: &[StorageSummary], state: &Path) -> Option<String> {
    if volumes.is_empty() {
        return None;
    }
    let state = state.canonicalize().ok()?;
    let disks =
        sysinfo::Disks::new_with_refreshed_list_specifics(sysinfo::DiskRefreshKind::nothing());
    let candidates: Vec<_> = disks
        .iter()
        .map(|disk| DefaultVolume {
            id: format!("disk:{}", disk.mount_point().to_string_lossy()),
            mount: disk.mount_point().to_path_buf(),
            filesystem: disk.file_system().to_string_lossy().into_owned(),
            removable: disk.is_removable(),
            read_only: disk.is_read_only(),
        })
        .collect();
    choose_default_volume_with(volumes, &state, &candidates, same_filesystem, |_, state| {
        Some((
            fs2::total_space(state).ok()?,
            fs2::available_space(state).ok()?,
        ))
    })
}

fn choose_default_volume_with(
    volumes: &[StorageSummary],
    state: &Path,
    candidates: &[DefaultVolume],
    mut backs_state: impl FnMut(&Path, &Path) -> bool,
    capacity: impl FnOnce(&DefaultVolume, &Path) -> Option<(u64, u64)>,
) -> Option<String> {
    // A known network/virtual mount nested under a physical disk must not be
    // mistaken for its parent. Reject it before touching the state's metadata.
    let deepest_mount = candidates
        .iter()
        .filter_map(|volume| mount_specificity(state, &volume.mount).map(|depth| (depth, volume)))
        .max_by_key(|(depth, _)| *depth);
    if deepest_mount.is_some_and(|(_, volume)| !local_filesystem(&volume.filesystem)) {
        return None;
    }

    // Identify the backing mount before applying eligibility rules. In
    // particular an ineligible nested mount cannot fall back to an eligible
    // parent. st_dev recognizes macOS Data-volume firmlinks even though their
    // mount path is not a lexical prefix of the user's state directory.
    let selected = candidates
        .iter()
        .filter(|volume| local_filesystem(&volume.filesystem))
        .filter(|volume| backs_state(&volume.mount, state))
        .max_by(|a, b| {
            mount_specificity(state, &a.mount)
                .cmp(&mount_specificity(state, &b.mount))
                .then_with(|| {
                    a.mount
                        .components()
                        .count()
                        .cmp(&b.mount.components().count())
                })
                .then_with(|| b.id.cmp(&a.id))
        })?;
    if selected.removable || selected.read_only {
        return None;
    }
    let mut summaries = volumes.iter().filter(|volume| volume.id == selected.id);
    let summary = summaries.next()?;
    if summaries.next().is_some()
        || summary.removable
        || summary.kind.eq_ignore_ascii_case("removable")
    {
        return None;
    }
    let (total, available) = capacity(selected, state)?;
    can_afford_default(total, available).then(|| selected.id.clone())
}

fn can_afford_default(total: u64, available: u64) -> bool {
    if total == 0 || available > total {
        return false;
    }
    let reserve = MIN_DEFAULT_FREE_BYTES.max(total.div_ceil(10));
    DEFAULT_CONTRIBUTION_BYTES
        .checked_add(reserve)
        .is_some_and(|required| available >= required)
}

fn local_filesystem(filesystem: &str) -> bool {
    // Automatic enrollment is conservative. Unknown disk *kind* is fine (VMs
    // commonly report it), but an unknown filesystem needs manual enrollment.
    // Exact names exclude WebDAV, SMB/NFS, FUSE/WinFsp, RAM and adapter mounts.
    matches!(
        filesystem.to_ascii_lowercase().as_str(),
        "apfs"
            | "hfs"
            | "hfs+"
            | "hfsplus"
            | "ext2"
            | "ext3"
            | "ext4"
            | "btrfs"
            | "xfs"
            | "zfs"
            | "ufs"
            | "ufs2"
            | "f2fs"
            | "jfs"
            | "reiserfs"
            | "nilfs2"
            | "bcachefs"
            | "ntfs"
            | "refs"
            | "fat"
            | "fat12"
            | "fat16"
            | "fat32"
            | "vfat"
            | "msdos"
            | "exfat"
    )
}

fn mount_specificity(state: &Path, mount: &Path) -> Option<usize> {
    #[cfg(windows)]
    {
        windows_mount_specificity(&state.to_string_lossy(), &mount.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        state.starts_with(mount).then(|| mount.components().count())
    }
}

#[cfg(any(windows, test))]
fn windows_mount_specificity(state: &str, mount: &str) -> Option<usize> {
    fn normalize(path: &str) -> String {
        let path = path.replace('/', "\\");
        path.strip_prefix(r"\\?\")
            .unwrap_or(&path)
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }
    let state = normalize(state);
    let mount = normalize(mount);
    if mount.is_empty() {
        return None;
    }
    (state == mount
        || state
            .strip_prefix(&mount)
            .is_some_and(|tail| tail.starts_with('\\')))
    .then(|| mount.split('\\').filter(|part| !part.is_empty()).count())
}

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
    // Mount identities need no capacity probes. In particular, avoid macOS
    // querying available space on our own WebDAV mount during enumeration.
    sysinfo::Disks::new_with_refreshed_list_specifics(sysinfo::DiskRefreshKind::nothing())
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
    // Preserve the mount-prefix fallback on platforms without Unix device IDs.
    // Windows final paths may differ in drive-letter case or verbatim prefix.
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => {
            #[cfg(windows)]
            {
                windows_mount_specificity(&right.to_string_lossy(), &left.to_string_lossy())
                    .is_some()
            }
            #[cfg(not(windows))]
            {
                right.starts_with(left)
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_plan::PlanStamp;

    fn default_candidate(id: &str, mount: &str, filesystem: &str) -> DefaultVolume {
        DefaultVolume {
            id: id.into(),
            mount: mount.into(),
            filesystem: filesystem.into(),
            removable: false,
            read_only: false,
        }
    }

    fn storage_summary(id: &str) -> StorageSummary {
        StorageSummary {
            id: id.into(),
            name: "Local disk".into(),
            total_bytes: 100 << 30,
            available_bytes: 100 << 30,
            removable: false,
            // VM disks need not identify themselves as HDD or SSD.
            kind: "unknown".into(),
        }
    }

    #[test]
    fn default_contribution_leaves_both_absolute_and_relative_headroom() {
        let gib = 1 << 30;
        assert_eq!(DEFAULT_CONTRIBUTION_BYTES, gib);
        assert!(can_afford_default(20 * gib, 6 * gib));
        assert!(!can_afford_default(20 * gib, 6 * gib - 1));
        assert!(can_afford_default(100 * gib, 11 * gib));
        assert!(!can_afford_default(100 * gib, 11 * gib - 1));
        // Round reserve upward so fractions of a byte never erode ten percent.
        assert!(!can_afford_default(100 * gib + 1, 11 * gib));
        assert!(can_afford_default(100 * gib + 1, 11 * gib + 1));
        assert!(!can_afford_default(0, 0));
        assert!(!can_afford_default(5 * gib, 5 * gib));
        assert!(!can_afford_default(20 * gib, 21 * gib));
        assert!(!can_afford_default(u64::MAX, 0));
    }

    #[test]
    fn selector_uses_fresh_capacity_once_instead_of_the_advertised_free_space() {
        let candidates = [
            default_candidate("local", "disk", "ext4"),
            default_candidate("network", "network", "nfs4"),
        ];
        let summaries = [storage_summary("local"), storage_summary("network")];
        let mut queried = Vec::new();
        let selected = choose_default_volume_with(
            &summaries,
            Path::new("disk/state"),
            &candidates,
            |mount, _| mount == Path::new("disk"),
            |disk, state| {
                queried.push(disk.id.clone());
                assert_eq!(state, Path::new("disk/state"));
                Some((100 << 30, 2 << 30))
            },
        );
        assert_eq!(
            selected, None,
            "stale advertised capacity must not enroll a full disk"
        );
        assert_eq!(queried, ["local"]);
        let mut stale_full = storage_summary("local");
        stale_full.available_bytes = 0;
        assert_eq!(
            choose_default_volume_with(
                &[stale_full],
                Path::new("disk/state"),
                &candidates,
                |mount, _| mount == Path::new("disk"),
                |_, _| Some((100 << 30, 11 << 30)),
            ),
            Some("local".into()),
            "fresh free space and an unknown disk kind are eligible"
        );
    }

    #[test]
    fn selector_does_not_fall_back_from_an_ineligible_nested_volume() {
        let mut nested = default_candidate("nested", "disk/mounted", "ext4");
        nested.removable = true;
        let candidates = [default_candidate("parent", "disk", "ext4"), nested];
        let summaries = [storage_summary("parent"), storage_summary("nested")];
        assert_eq!(
            choose_default_volume_with(
                &summaries,
                Path::new("disk/mounted/state"),
                &candidates,
                |_, _| true,
                |_, _| panic!("must not query capacity on an ineligible volume"),
            ),
            None
        );
        let mut read_only = candidates;
        read_only[1].removable = false;
        read_only[1].read_only = true;
        assert_eq!(
            choose_default_volume_with(
                &summaries,
                Path::new("disk/mounted/state"),
                &read_only,
                |_, _| true,
                |_, _| panic!("must not query capacity on a read-only volume"),
            ),
            None
        );
    }

    #[test]
    fn network_and_adapter_backing_mounts_are_rejected_before_probing() {
        for filesystem in [
            "nfs4",
            "cifs",
            "webdav",
            "fuse.davfs",
            "WinFsp",
            "tmpfs",
            "",
        ] {
            let candidates = [
                default_candidate("parent", "disk", "ext4"),
                default_candidate("remote", "disk/mounted", filesystem),
            ];
            assert_eq!(
                choose_default_volume_with(
                    &[storage_summary("parent"), storage_summary("remote")],
                    Path::new("disk/mounted/state"),
                    &candidates,
                    |_, _| panic!("must not stat network-backed state for {filesystem}"),
                    |_, _| panic!("must not query network capacity for {filesystem}"),
                ),
                None
            );
        }
        for filesystem in [
            "APFS", "hfs+", "NTFS", "ReFS", "FAT32", "exFAT", "btrfs", "xfs",
        ] {
            assert!(local_filesystem(filesystem));
        }
    }

    #[test]
    fn selector_handles_firmlinked_state_without_a_mount_path_prefix() {
        let candidates = [
            default_candidate("system", "/", "apfs"),
            default_candidate("data", "/System/Volumes/Data", "apfs"),
        ];
        assert_eq!(
            choose_default_volume_with(
                &[storage_summary("system"), storage_summary("data")],
                Path::new("/Users/person/.myownmesh"),
                &candidates,
                |mount, _| mount == Path::new("/System/Volumes/Data"),
                |disk, _| {
                    assert_eq!(disk.id, "data");
                    Some((100 << 30, 11 << 30))
                },
            ),
            Some("data".into())
        );
    }

    #[test]
    fn missing_duplicate_or_removable_summary_cannot_enroll_a_volume() {
        let candidates = [default_candidate("local", "disk", "ext4")];
        let mut removable = storage_summary("local");
        removable.removable = true;
        for summaries in [
            vec![],
            vec![storage_summary("other")],
            vec![storage_summary("local"), storage_summary("local")],
            vec![removable],
        ] {
            assert_eq!(
                choose_default_volume_with(
                    &summaries,
                    Path::new("disk/state"),
                    &candidates,
                    |_, _| true,
                    |_, _| panic!("unadvertised or ineligible volume must not be probed"),
                ),
                None
            );
        }
    }

    #[test]
    fn windows_mount_matching_ignores_case_and_honors_component_boundaries() {
        let state = r"\\?\C:\Users\Person\Mounted\state";
        assert_eq!(windows_mount_specificity(state, r"c:\"), Some(1));
        assert_eq!(
            windows_mount_specificity(state, r"c:\users\person\mounted\"),
            Some(4)
        );
        assert_eq!(
            windows_mount_specificity(state, r"C:\Users\Person\Mount"),
            None
        );
        assert_eq!(windows_mount_specificity(state, r"D:\"), None);
        assert_eq!(
            windows_mount_specificity(r"C:/Users/state", r"c:\users"),
            Some(2)
        );
    }

    fn allocation() -> StorageAllocation {
        StorageAllocation {
            id: "local-volume".into(),
            device: "local".into(),
            volume: "disk:test".into(),
            quota_bytes: 1024,
            enabled: true,
            automatic: false,
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
