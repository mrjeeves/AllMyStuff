//! Linux user mounts and their exact kernel-reported ownership.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Child;

const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MountLease {
    mount: PathBuf,
    port: u16,
    mount_id: u64,
    source: String,
}

struct MountEntry {
    id: u64,
    mount: String,
    filesystem: String,
    source: String,
}

fn unescape_mount_field(field: &str) -> Option<String> {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let octal = bytes.get(index + 1..index + 4)?;
            if !octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                return None;
            }
            let value = (u16::from(octal[0] - b'0') << 6)
                | (u16::from(octal[1] - b'0') << 3)
                | u16::from(octal[2] - b'0');
            decoded.push(u8::try_from(value).ok()?);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn mount_entry(line: &str) -> Option<MountEntry> {
    let fields: Vec<_> = line.split_whitespace().collect();
    let separator = fields.iter().position(|field| *field == "-")?;
    if separator < 6 {
        return None;
    }
    Some(MountEntry {
        id: fields.first()?.parse().ok()?,
        mount: unescape_mount_field(fields.get(4)?)?,
        filesystem: fields.get(separator + 1)?.to_string(),
        source: unescape_mount_field(fields.get(separator + 2)?)?,
    })
}

fn loopback_port(source: &str) -> Option<u16> {
    let url = reqwest::Url::parse(source).ok()?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.port_or_known_default().filter(|port| *port != 0)
}

pub(super) fn is_mounted(mountinfo: &str, mount: &Path) -> bool {
    mountinfo
        .lines()
        .filter_map(mount_entry)
        .any(|entry| Path::new(&entry.mount) == mount)
}

pub(super) fn matches_port(mountinfo: &str, mount: &Path, port: u16) -> bool {
    let mut matching = mountinfo
        .lines()
        .filter_map(mount_entry)
        .filter(|entry| Path::new(&entry.mount) == mount);
    let Some(entry) = matching.next() else {
        return false;
    };
    // An overmount can hide a different filesystem. Never unmount an
    // ambiguous stack merely because one of its entries belongs to us.
    matching.next().is_none()
        && matches!(
            entry.filesystem.as_str(),
            "fuse.rclone" | "fuse.davfs" | "davfs" | "fuse"
        )
        && loopback_port(&entry.source) == Some(port)
}

fn single_entry(mountinfo: &str, mount: &Path) -> Option<MountEntry> {
    let mut entries = mountinfo
        .lines()
        .filter_map(mount_entry)
        .filter(|entry| Path::new(&entry.mount) == mount);
    let entry = entries.next()?;
    entries.next().is_none().then_some(entry)
}

fn lease_matches(mountinfo: &str, mount: &Path, port: u16, lease: &MountLease) -> bool {
    lease.mount == mount
        && lease.port == port
        && single_entry(mountinfo, mount).is_some_and(|entry| {
            entry.id == lease.mount_id
                && entry.filesystem == "fuse.rclone"
                && entry.source == lease.source
        })
}

fn state_root() -> Result<PathBuf, String> {
    allmystuff_protocol::myownmesh_state_dir()
        .map(|state| state.join("linux-drives"))
        .ok_or_else(|| "couldn't find the AllMyStuff state folder".into())
}

fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn lease_path(mount: &Path) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    Ok(state_root()?
        .join("leases")
        .join(format!("{}.json", digest(mount.as_os_str().as_bytes()))))
}

fn read_lease(mount: &Path) -> Option<MountLease> {
    let bytes = std::fs::read(lease_path(mount).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn owns_mount(mountinfo: &str, mount: &Path, port: u16) -> bool {
    read_lease(mount).is_some_and(|lease| lease_matches(mountinfo, mount, port, &lease))
        // Recover mounts created by older AllMyStuff versions, whose kernel
        // source was the complete ephemeral DAV URL rather than a remote name.
        || matches_port(mountinfo, mount, port)
}

pub(super) fn forget_lease(mount: &Path, port: u16) {
    if read_lease(mount).is_some_and(|lease| lease.mount == mount && lease.port == port) {
        if let Ok(path) = lease_path(mount) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("couldn't create private drive state: {error}"))?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("the private drive state folder is not owned by this user".into());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("couldn't protect private drive state: {error}"))
}

struct PreparedMount {
    remote: String,
    config: PathBuf,
    cache: PathBuf,
    log: PathBuf,
    lock: File,
}

fn prepare(root: &Path, mapping: &str, url: &str) -> Result<PreparedMount, String> {
    if mapping.is_empty() || loopback_port(url).is_none() {
        return Err("invalid local drive mapping or WebDAV endpoint".into());
    }
    private_directory(root)?;
    let mapping_id = digest(mapping.as_bytes());
    let directory = root.join(&mapping_id);
    private_directory(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("mount.lock"))
        .map_err(|error| format!("couldn't open the drive cache lock: {error}"))?;
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| {
        "this drive's write cache is already in use; wait for its previous mount to stop"
            .to_string()
    })?;
    let cache = directory.join("cache");
    private_directory(&cache)?;
    let remote = format!("allmystuff_{mapping_id}");
    let config = directory.join("rclone.conf");
    // Keep the remote name and config path stable across route/listener changes.
    // Command-line backend overrides make rclone hash the changing URL into its
    // VFS cache identity, stranding pending writes on the next connection.
    let contents = format!("[{remote}]\ntype = webdav\nurl = {url}\nvendor = other\n");
    crate::persist::write_atomic(&config, contents.as_bytes())
        .map_err(|error| format!("couldn't save the private drive configuration: {error}"))?;
    Ok(PreparedMount {
        remote,
        config,
        cache,
        log: directory.join("mount.log"),
        lock,
    })
}

fn mount_args(prepared: &PreparedMount, mount: &Path) -> Vec<OsString> {
    vec![
        "mount".into(),
        format!("{}:", prepared.remote).into(),
        mount.as_os_str().to_owned(),
        "--config".into(),
        prepared.config.as_os_str().to_owned(),
        "--cache-dir".into(),
        prepared.cache.as_os_str().to_owned(),
        "--vfs-cache-mode".into(),
        "writes".into(),
        "--vfs-write-back".into(),
        "0s".into(),
        "--dir-cache-time".into(),
        "1s".into(),
        "--poll-interval".into(),
        "0".into(),
        "--default-permissions".into(),
        "--file-perms".into(),
        "0600".into(),
        "--dir-perms".into(),
        "0700".into(),
        "--umask".into(),
        "077".into(),
    ]
}

/// Own the foreground helper: cancelling startup or dropping an active mount
/// kills our child, and never signals a PID obtained from a stale file.
pub(super) struct MountedHelper {
    child: Option<Child>,
    lock: Option<File>,
}

async fn reap(child: &mut Child) {
    if tokio::time::timeout(STOP_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(STOP_TIMEOUT, child.wait()).await;
    }
}

impl MountedHelper {
    pub(super) async fn stop(mut self) {
        if let Some(mut child) = self.child.take() {
            reap(&mut child).await;
        }
    }
}

impl Drop for MountedHelper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            // Hold the cache lock until the killed child has actually exited.
            // Tokio also reaps kill_on_drop children if runtime teardown wins.
            let lock = self.lock.take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    reap(&mut child).await;
                    drop(lock);
                });
            }
        }
    }
}

fn failure(log: &Path, detail: &str) -> String {
    let tail = File::open(log)
        .and_then(|mut file| {
            use std::io::{Read, Seek, SeekFrom};
            let length = file.metadata()?.len();
            file.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            Ok(text)
        })
        .unwrap_or_default();
    format!(
        "Linux couldn't mount the drive: {detail}. {} Install rclone and fuse3, and run AllMyStuff in your user session with access to /dev/fuse; a hardened system service may deny FUSE mounting. Pending writes remain in the private drive cache.",
        tail.trim()
    )
}

pub(super) async fn start(mount: &Path, url: &str, mapping: &str) -> Result<MountedHelper, String> {
    let port = loopback_port(url).ok_or_else(|| "invalid local WebDAV endpoint".to_string())?;
    let prepared = prepare(&state_root()?, mapping, url)?;
    start_prepared(mount, port, prepared, OsStr::new("rclone"), START_TIMEOUT).await
}

async fn start_prepared(
    mount: &Path,
    port: u16,
    prepared: PreparedMount,
    program: &OsStr,
    timeout: Duration,
) -> Result<MountedHelper, String> {
    let log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&prepared.log)
        .map_err(|error| format!("couldn't open the drive mount log: {error}"))?;
    let mut command = crate::child_process::command(program);
    command
        .args(mount_args(&prepared, mount))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    let parent_pid = unsafe { libc::getpid() };
    // kill_on_drop cannot run when the node itself is killed. Arrange for the
    // foreground helper to receive SIGTERM from the kernel in that case, and
    // close the fork/prctl race if our parent disappeared before installation.
    // This closure uses only async-signal-safe libc calls before exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|error| failure(&prepared.log, &format!("couldn't launch rclone: {error}")))?;
    let mut helper = MountedHelper {
        child: Some(child),
        lock: Some(prepared.lock),
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mounts = match std::fs::read_to_string("/proc/self/mountinfo") {
            Ok(mounts) => mounts,
            Err(error) => {
                return Err(failure(
                    &prepared.log,
                    &format!("couldn't inspect mounts: {error}"),
                ));
            }
        };
        if let Some(entry) = single_entry(&mounts, mount) {
            if entry.filesystem == "fuse.rclone" && entry.source == format!("{}:", prepared.remote)
            {
                let lease = MountLease {
                    mount: mount.to_path_buf(),
                    port,
                    mount_id: entry.id,
                    source: entry.source,
                };
                let saved = (|| {
                    let path = lease_path(mount)?;
                    private_directory(path.parent().unwrap())?;
                    let bytes = serde_json::to_vec(&lease).map_err(|error| error.to_string())?;
                    crate::persist::write_atomic(&path, &bytes).map_err(|error| error.to_string())
                })();
                if let Err(error) = saved {
                    // We observed this exact child-created mount but could not
                    // persist its proof. Clean it before returning an error.
                    if std::fs::read_to_string("/proc/self/mountinfo")
                        .is_ok_and(|current| lease_matches(&current, mount, port, &lease))
                    {
                        let _ = unmount_matching(mount, |current| {
                            lease_matches(current, mount, port, &lease)
                        })
                        .await;
                    }
                    return Err(failure(
                        &prepared.log,
                        &format!("couldn't save mount ownership: {error}"),
                    ));
                }
                return Ok(helper);
            }
            return Err(failure(
                &prepared.log,
                "the mount point was assigned to another filesystem",
            ));
        }
        match helper.child.as_mut().unwrap().try_wait() {
            Ok(Some(status)) => {
                return Err(failure(
                    &prepared.log,
                    &format!("rclone exited with {status}"),
                ))
            }
            Err(error) => return Err(failure(&prepared.log, &error.to_string())),
            Ok(None) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(failure(
                &prepared.log,
                "rclone did not create the filesystem within 15 seconds",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) async fn unmount_owned(mount: &Path, port: u16) -> Result<(), String> {
    unmount_matching(mount, |mounts| owns_mount(mounts, mount, port)).await
}

async fn unmount_matching(mount: &Path, owns: impl Fn(&str) -> bool) -> Result<(), String> {
    let mut errors = Vec::new();
    for program in ["fusermount3", "fusermount"] {
        // Recheck before each helper, including the fallback. A failed first
        // unmount must not make us remove a replacement mounted in between.
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("couldn't inspect Linux mounts: {error}"))?;
        if !is_mounted(&mounts, mount) {
            return Ok(());
        }
        if !owns(&mounts) {
            return Err(format!(
                "preserved a reassigned mount at {}",
                mount.display()
            ));
        }
        let mut command = crate::child_process::command(program);
        command.args([OsStr::new("-u"), OsStr::new("--"), mount.as_os_str()]);
        command.kill_on_drop(true);
        match tokio::time::timeout(STOP_TIMEOUT, command.output()).await {
            Ok(Ok(output)) if output.status.success() => return Ok(()),
            Ok(Ok(output)) => errors.push(format!(
                "{program}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Ok(Err(error)) => errors.push(format!("{program}: {error}")),
            Err(_) => errors.push(format!("{program} timed out")),
        }
    }
    Err(format!(
        "couldn't unmount {}: {}",
        mount.display(),
        errors.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "allmystuff-linux-drive-test-{}-{suffix}",
                std::process::id()
            ));
            private_directory(&path).unwrap();
            Self(path)
        }

        fn script(&self, contents: &str) -> PathBuf {
            let path = self.0.join("helper");
            std::fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(mount: &str, filesystem: &str, source: &str) -> String {
        format!("42 21 0:48 / {mount} rw,nosuid,nodev shared:1 - {filesystem} {source} rw\n")
    }

    #[test]
    fn owned_mount_requires_exact_loopback_port_and_filesystem() {
        let mount = Path::new("/home/user/drive");
        assert!(matches_port(
            &entry("/home/user/drive", "fuse.rclone", "http://127.0.0.1:1234/"),
            mount,
            1234
        ));
        assert!(!matches_port(
            &entry("/home/user/drive", "fuse.rclone", "http://127.0.0.1:12345/"),
            mount,
            1234
        ));
        for source in [
            "http://remote/127.0.0.1:1234/",
            "http://127.0.0.1:1234/foreign",
            "http://127.0.0.1:1234/?other",
            "http://127.0.0.1:1234/#other",
            "http://user@127.0.0.1:1234/",
            "https://127.0.0.1:1234/",
        ] {
            assert!(!matches_port(
                &entry("/home/user/drive", "fuse.rclone", source),
                mount,
                1234
            ));
        }
        assert!(!matches_port(
            &entry("/home/user/drive", "ext4", "http://127.0.0.1:1234/"),
            mount,
            1234
        ));
        assert!(matches_port(
            &entry("/home/user/drive", "fuse", "http://127.0.0.1:1234/"),
            mount,
            1234
        ));
    }

    #[test]
    fn mount_fields_decode_spaces_tabs_newlines_and_backslashes() {
        let path = "/home/user/AllMyStuff Drives/one\\two\tthree\nfour";
        let encoded = r"/home/user/AllMyStuff\040Drives/one\134two\011three\012four";
        let info = entry(encoded, "fuse.rclone", "http://127.0.0.1:4321/");
        assert!(is_mounted(&info, Path::new(path)));
        assert!(matches_port(&info, Path::new(path), 4321));
        assert!(!is_mounted(&info, Path::new("/home/user/AllMyStuff")));
        for malformed in [r"trailing\", r"bad\09x", r"overflow\777"] {
            assert!(unescape_mount_field(malformed).is_none());
        }
    }

    #[test]
    fn foreign_and_stacked_mounts_are_preserved() {
        let mount = Path::new("/drive");
        let ours = entry("/drive", "fuse.rclone", "http://127.0.0.1:7777/");
        let foreign = entry("/drive", "fuse.sshfs", "user@host:/files");
        assert!(is_mounted(&foreign, mount));
        assert!(!matches_port(&foreign, mount, 7777));
        assert!(!matches_port(&(ours + &foreign), mount, 7777));
        assert!(!matches_port("malformed mount record", mount, 7777));
    }

    #[test]
    fn stable_remote_ownership_requires_the_same_mount_instance_and_port() {
        let mount = Path::new("/drive");
        let source = "allmystuff_mapping:";
        let info = entry("/drive", "fuse.rclone", source);
        let lease = MountLease {
            mount: mount.into(),
            port: 1234,
            mount_id: 42,
            source: source.into(),
        };
        assert!(lease_matches(&info, mount, 1234, &lease));
        assert!(!lease_matches(&info, mount, 12345, &lease));
        assert!(!lease_matches(
            &info.replacen("42 ", "43 ", 1),
            mount,
            1234,
            &lease
        ));
        assert!(!lease_matches(
            &info.replace(source, "allmystuff_other:"),
            mount,
            1234,
            &lease
        ));
        assert!(!lease_matches(
            &info.replace("fuse.rclone", "fuse.sshfs"),
            mount,
            1234,
            &lease
        ));
        assert!(!lease_matches(&(info.clone() + &info), mount, 1234, &lease));
    }

    #[test]
    fn reconnect_changes_only_the_private_endpoint_and_preserves_pending_writes() {
        let root = TestDirectory::new();
        let first = prepare(&root.0, "peer/share/mapping", "http://127.0.0.1:1234/").unwrap();
        let remote = first.remote.clone();
        let cache = first.cache.clone();
        let config = first.config.clone();
        std::fs::write(cache.join("pending-write"), b"not uploaded yet").unwrap();
        assert!(prepare(&root.0, "peer/share/mapping", "http://127.0.0.1:2345/").is_err());
        drop(first);
        let second = prepare(&root.0, "peer/share/mapping", "http://127.0.0.1:2345/").unwrap();
        assert_eq!(second.remote, remote);
        assert_eq!(second.cache, cache);
        assert_eq!(second.config, config);
        assert!(std::fs::read_to_string(&config)
            .unwrap()
            .contains("url = http://127.0.0.1:2345/"));
        assert_eq!(
            std::fs::read(cache.join("pending-write")).unwrap(),
            b"not uploaded yet"
        );
        assert_eq!(
            std::fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let other = prepare(&root.0, "different mapping", "http://127.0.0.1:3456/").unwrap();
        assert_ne!(second.cache, other.cache);
        assert_ne!(second.remote, other.remote);
    }

    #[test]
    fn mount_arguments_support_stock_rclone_without_changing_cache_identity() {
        let root = TestDirectory::new();
        let prepared = prepare(&root.0, "mapping", "http://127.0.0.1:1234/").unwrap();
        let args = mount_args(&prepared, Path::new("/home/user/My Drive"));
        let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert_eq!(args[0], "mount");
        assert_eq!(args[1], format!("{}:", prepared.remote));
        assert_eq!(args[2], "/home/user/My Drive");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--vfs-cache-mode", "writes"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--vfs-write-back", "0s"]));
        assert!(args.iter().any(|arg| arg == "--default-permissions"));
        for forbidden in [
            "--daemon",
            "--daemon-wait",
            "--pid-file",
            "--devname",
            "--allow-other",
            "--webdav-url",
        ] {
            assert!(!args.iter().any(|arg| arg == forbidden));
        }
        assert!(!args.iter().any(|arg| arg.contains("127.0.0.1")));
    }

    #[tokio::test]
    async fn helper_failure_reports_fuse_error_and_retains_pending_cache() {
        let root = TestDirectory::new();
        let program = root.script("printf 'FUSE denied by policy\\n' >&2\nexit 7");
        let prepared = prepare(&root.0, "mapping", "http://127.0.0.1:1234/").unwrap();
        let pending = prepared.cache.join("pending-write");
        std::fs::write(&pending, b"keep me").unwrap();
        let error = start_prepared(
            &root.0.join("mount"),
            1234,
            prepared,
            program.as_os_str(),
            Duration::from_secs(1),
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("FUSE denied by policy"), "{error}");
        assert!(error.contains("user session"), "{error}");
        assert_eq!(std::fs::read(pending).unwrap(), b"keep me");
    }

    async fn wait_for_pid(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(pid) = std::fs::read_to_string(path) {
                if let Ok(pid) = pid.trim().parse() {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test helper did not start");
    }

    async fn assert_reaped(pid: i32) {
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("helper {pid} survived startup cleanup");
    }

    #[tokio::test]
    async fn startup_timeout_kills_and_reaps_our_helper() {
        let root = TestDirectory::new();
        let pid_path = root.0.join("pid");
        let program = root.script(&format!(
            "printf '%s' \"$$\" > '{}'\nexec sleep 60",
            pid_path.display()
        ));
        let prepared = prepare(&root.0, "mapping", "http://127.0.0.1:1234/").unwrap();
        let error = start_prepared(
            &root.0.join("mount"),
            1234,
            prepared,
            program.as_os_str(),
            Duration::from_millis(100),
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("did not create the filesystem"), "{error}");
        assert_reaped(wait_for_pid(&pid_path).await).await;
        assert!(prepare(&root.0, "mapping", "http://127.0.0.1:2345/").is_ok());
    }

    #[tokio::test]
    async fn cancelled_startup_kills_and_reaps_our_helper() {
        let root = TestDirectory::new();
        let pid_path = root.0.join("pid");
        let program = root.script(&format!(
            "printf '%s' \"$$\" > '{}'\nexec sleep 60",
            pid_path.display()
        ));
        let prepared = prepare(&root.0, "mapping", "http://127.0.0.1:1234/").unwrap();
        let mount = root.0.join("mount");
        let task = tokio::spawn(async move {
            start_prepared(
                &mount,
                1234,
                prepared,
                program.as_os_str(),
                Duration::from_secs(15),
            )
            .await
        });
        let pid = wait_for_pid(&pid_path).await;
        task.abort();
        let _ = task.await;
        assert_reaped(pid).await;
        assert!(prepare(&root.0, "mapping", "http://127.0.0.1:2345/").is_ok());
    }
}
