//! Native SMB mounts using the OS client. Credentials exist only in memory and
//! the one-shot helper's stdin; saved leases identify mounts, not logins.
use allmystuff_session::SmbEndpoint;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::io::Read;
#[cfg(test)]
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

const HELPER: &str = "--ams-native-smb-helper";
static LEASE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Lease {
    mount: PathBuf,
    server: Ipv4Addr,
    share: String,
    port: u16,
    uid: u32,
    gid: u32,
    generation: String,
    identity: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Request {
    lease: Lease,
    password: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct HelperReply {
    identity: Option<String>,
    error: Option<String>,
}

fn valid_endpoint(lease: &Lease) -> bool {
    lease.server.is_loopback()
        && lease.port != 0
        && lease.share.starts_with("ams")
        && lease.share.len() == 19
        && lease.share[3..].bytes().all(|b| b.is_ascii_hexdigit())
        && lease.mount.is_absolute()
        && lease.mount.parent().is_some()
}

fn canonical_mount(mount: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(mount) {
        Ok(path) => Ok(path),
        #[cfg(target_os = "macos")]
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && mount.parent() == Some(Path::new("/Volumes")) =>
        {
            Ok(mount.to_path_buf())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn state_file() -> Result<PathBuf, String> {
    allmystuff_protocol::myownmesh_state_dir()
        .map(|root| root.join("allmystuff-smb-mounts.json"))
        .ok_or_else(|| "the AMS state directory is unavailable".into())
}

fn leases() -> Result<Vec<Lease>, String> {
    match std::fs::read(state_file()?) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

fn save(entries: &[Lease]) -> Result<(), String> {
    crate::persist::write_atomic(
        &state_file()?,
        &serde_json::to_vec(entries).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn remember(lease: &Lease) -> Result<(), String> {
    let _guard = LEASE_LOCK.lock();
    let mut entries = leases()?;
    entries.retain(|entry| entry.mount != lease.mount);
    entries.push(lease.clone());
    save(&entries)
}

fn current_lease(expected: &Lease) -> Result<bool, String> {
    Ok(leases()?.iter().any(|entry| entry == expected))
}

fn forget(expected: &Lease) -> Result<(), String> {
    let _guard = LEASE_LOCK.lock();
    let mut entries = leases()?;
    entries.retain(|entry| entry != expected);
    save(&entries)
}

#[cfg(target_os = "linux")]
pub(crate) fn ensure_supported() -> Result<(), String> {
    // ProtectSystem/PrivateMounts create a service-only mount namespace. A
    // successful mount there would still be invisible to desktop applications.
    let own = std::fs::read_link("/proc/self/ns/mnt").map_err(|e| e.to_string())?;
    let init = std::fs::read_link("/proc/1/ns/mnt").map_err(|e| e.to_string())?;
    if own != init {
        return Err("Native SMB drives need the desktop mount namespace. Run AMS in your user session; a hardened system service cannot expose this mount to desktop applications.".into());
    }
    if unsafe { libc::geteuid() } != 0 && !Path::new("/usr/bin/pkexec").is_file() {
        return Err("Native Linux SMB drives need mount authorization. Install your distribution's polkit/pkexec package, then map from the AMS user session.".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_supported() -> Result<(), String> {
    Ok(())
}

async fn helper(request: &Request) -> Result<Option<String>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    #[cfg(test)]
    let exe = std::env::var_os("ALLMYSTUFF_TEST_NATIVE_SMB_HELPER")
        .map(PathBuf::from)
        .unwrap_or(exe);
    let mut command;
    #[cfg(target_os = "linux")]
    {
        command = if unsafe { libc::geteuid() } == 0 {
            crate::child_process::command(&exe)
        } else {
            let mut elevated = crate::child_process::command("/usr/bin/pkexec");
            elevated.arg(&exe);
            elevated
        };
    }
    #[cfg(target_os = "macos")]
    {
        command = crate::child_process::command(&exe);
    }
    let mut child = command
        .arg(HELPER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("start native SMB mount helper: {e}"))?;
    use tokio::io::AsyncWriteExt;
    child
        .stdin
        .take()
        .ok_or("the native SMB helper has no input pipe")?
        .write_all(&serde_json::to_vec(request).map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;
    let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .map_err(|_| "Native SMB mount authorization or connection timed out. Its recovery lease was retained.".to_string())?
        .map_err(|e| e.to_string())?;
    let reply = serde_json::from_slice::<HelperReply>(&output.stdout).ok();
    if let Some(error) = reply.as_ref().and_then(|reply| reply.error.as_ref()) {
        let error = request.password.as_ref().map_or_else(
            || error.clone(),
            |password| error.replace(password, "[redacted]"),
        );
        return Err(format!("Native SMB mount: {error}"));
    }
    if !output.status.success() {
        // Do not relay pkexec/OS output, which can include login information.
        return Err(format!(
            "Native SMB mount authorization or connection failed ({}). Use the desktop user session and authorize the OS mount request. The recovery lease was retained.",
            output.status
        ));
    }
    reply
        .map(|reply| reply.identity)
        .ok_or_else(|| "invalid native SMB helper response".into())
}

pub(crate) struct NativeSmbMount {
    lease: Lease,
    cleanup_on_drop: bool,
}

impl NativeSmbMount {
    pub(crate) async fn start(
        mount: &Path,
        address: SocketAddr,
        endpoint: SmbEndpoint,
    ) -> Result<Self, String> {
        ensure_supported()?;
        let std::net::IpAddr::V4(server) = address.ip() else {
            return Err("the SMB tunnel must have an IPv4 loopback identity".into());
        };
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).map_err(|e| e.to_string())?;
        let mut lease = Lease {
            mount: canonical_mount(mount)?,
            server,
            share: endpoint.share,
            port: address.port(),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            generation: random.iter().map(|b| format!("{b:02x}")).collect(),
            identity: None,
        };
        if !valid_endpoint(&lease) || endpoint.username != "ams" {
            return Err("invalid private SMB endpoint".into());
        }
        // Never replace a disconnected mount. Inspect the OS, not root.exists().
        if observed(&lease)?.is_some() {
            return Err("the native mount point is already assigned; it was preserved".into());
        }
        remember(&lease)?;
        let result = helper(&Request {
            lease: lease.clone(),
            password: Some(endpoint.password),
        })
        .await;
        match result {
            Ok(Some(identity)) => {
                lease.identity = Some(identity);
                remember(&lease)?;
                Ok(Self {
                    lease,
                    cleanup_on_drop: true,
                })
            }
            Ok(None) => Err("the OS did not report a native SMB mount".into()),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn stop(mut self) -> Result<(), String> {
        // An explicit stop already owns recovery/error reporting. In
        // particular, do not prompt for OS authorization again after Cancel.
        self.cleanup_on_drop = false;
        remove_exact(&self.lease).await
    }
}

impl Drop for NativeSmbMount {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        let lease = self.lease.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = remove_exact(&lease).await {
                    tracing::warn!(%error, "native SMB mount recovery lease retained");
                }
            });
        }
    }
}

async fn remove_exact(expected: &Lease) -> Result<(), String> {
    if !current_lease(expected)? {
        return Ok(());
    }
    helper(&Request {
        lease: expected.clone(),
        password: None,
    })
    .await?;
    forget(expected)
}

pub(crate) async fn remove_known(mount: &Path) -> Result<bool, String> {
    let Some(lease) = leases()?.into_iter().find(|entry| entry.mount == mount) else {
        return Ok(false);
    };
    remove_exact(&lease).await?;
    Ok(true)
}

pub(crate) async fn cleanup() -> Result<(), String> {
    for lease in leases()? {
        remove_exact(&lease).await?;
    }
    Ok(())
}

/// Called before logging/mesh startup in the existing node executable. No new
/// binary, setuid file, broad privileged daemon, or persistent policy is added.
pub(crate) fn run_helper_if_requested() -> Option<ExitCode> {
    if std::env::args_os().skip(1).collect::<Vec<_>>() != [std::ffi::OsString::from(HELPER)] {
        return None;
    }
    let result = (|| {
        let mut input = Vec::new();
        std::io::stdin()
            .take(16385)
            .read_to_end(&mut input)
            .map_err(|_| "couldn't read native SMB request".to_string())?;
        if input.len() > 16384 {
            return Err("native SMB request is too large".into());
        }
        let request: Request =
            serde_json::from_slice(&input).map_err(|_| "invalid native SMB request".to_string())?;
        execute(&request)
    })();
    let (reply, status) = match result {
        Ok(identity) => (
            HelperReply {
                identity,
                error: None,
            },
            ExitCode::SUCCESS,
        ),
        Err(error) => (
            HelperReply {
                identity: None,
                error: Some(error),
            },
            ExitCode::FAILURE,
        ),
    };
    Some(
        if serde_json::to_writer(std::io::stdout(), &reply).is_ok() {
            status
        } else {
            ExitCode::FAILURE
        },
    )
}

fn execute(request: &Request) -> Result<Option<String>, String> {
    let lease = &request.lease;
    if !valid_endpoint(lease) {
        return Err("invalid SMB mount request".into());
    }
    let caller = unsafe { libc::geteuid() };
    #[cfg(target_os = "linux")]
    let caller = if caller == 0 {
        if let Some(uid) = std::env::var_os("PKEXEC_UID") {
            uid.to_str()
                .and_then(|uid| uid.parse().ok())
                .ok_or("invalid mount caller")?
        } else {
            caller
        }
    } else {
        caller
    };
    if caller != lease.uid {
        return Err("the mount request does not belong to its caller".into());
    }
    let existing = observed(lease)?;
    if let Some(password) = &request.password {
        if canonical_mount(&lease.mount)? != lease.mount {
            return Err("the mount directory changed before setup".into());
        }
        if existing.is_some() {
            return Err("the mount point is already assigned".into());
        }
        match std::fs::symlink_metadata(&lease.mount) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || metadata.uid() != caller
                    || std::fs::read_dir(&lease.mount)
                        .map_err(|e| e.to_string())?
                        .next()
                        .is_some()
                {
                    return Err(
                        "the mount point must be an empty directory owned by the caller".into(),
                    );
                }
            }
            #[cfg(target_os = "macos")]
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && lease.mount.parent() == Some(Path::new("/Volumes")) => {}
            Err(error) => return Err(error.to_string()),
        }
        if password.len() != 64 || !password.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid SMB credential".into());
        }
        mount_native(lease, password)?;
        let mounted = observed(lease)?.ok_or("the OS did not report its new mount")?;
        if !mounted.1 {
            return Err("the mounted endpoint is different".into());
        }
        Ok(Some(mounted.0))
    } else {
        let Some((identity, matches)) = existing else {
            return Ok(None);
        };
        if !matches
            || lease
                .identity
                .as_ref()
                .is_some_and(|expected| expected != &identity)
        {
            // A different or replacement mount is never ours to remove.
            return Ok(None);
        }
        unmount_native(lease)?;
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn observed(lease: &Lease) -> Result<Option<(String, bool)>, String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| e.to_string())?;
    Ok(linux_observed(&info, lease))
}

#[cfg(target_os = "linux")]
fn linux_observed(info: &str, lease: &Lease) -> Option<(String, bool)> {
    info.lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let fields = before.split_whitespace().collect::<Vec<_>>();
            let kind = after.split_whitespace().collect::<Vec<_>>();
            let mount = fields
                .get(4)?
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\");
            if Path::new(&mount) != lease.mount {
                return None;
            }
            let expected = format!("//{}/{}", lease.server, lease.share);
            // Linux commonly omits the TCP port from mountinfo. The random
            // per-route share plus kernel mount ID identify our instance;
            // when a kernel does report a port, it must match too.
            let port_matches = kind.get(2).is_some_and(|options| {
                options
                    .split(',')
                    .filter_map(|value| value.strip_prefix("port="))
                    .all(|port| port.parse::<u16>().ok() == Some(lease.port))
            });
            let matches = kind.first() == Some(&"cifs")
                && kind.get(1) == Some(&expected.as_str())
                && port_matches;
            Some((fields[0].to_string(), matches))
        })
        .next_back()
}

#[cfg(target_os = "linux")]
fn mount_native(lease: &Lease, password: &str) -> Result<(), String> {
    let source = CString::new(format!("//{}/{}", lease.server, lease.share)).unwrap();
    let target =
        CString::new(lease.mount.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let kind = CString::new("cifs").unwrap();
    let options = CString::new(format!(
        "ip={},port={},vers=3.0,sec=ntlmssp,seal,sign,username=ams,password={},uid={},gid={},forceuid,forcegid,file_mode=0600,dir_mode=0700,nounix,nosharesock,cache=none,actimeo=0,soft",
        lease.server, lease.port, password, lease.uid, lease.gid
    )).unwrap();
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    } != 0
    {
        return Err(format!("CIFS mount: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unmount_native(lease: &Lease) -> Result<(), String> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    // Pin the original vfsmount so a path replacement after inspection cannot
    // redirect umount to another application's mount. O_PATH performs no I/O
    // against an offline SMB root; fdinfo exposes the kernel mount instance.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&lease.mount)
        .map_err(|e| e.to_string())?;
    let fd = file.as_raw_fd();
    let info =
        std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).map_err(|e| e.to_string())?;
    let pinned_id = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:").map(str::trim))
        .ok_or("the pinned mount has no kernel identity")?;
    let current = observed(lease)?.ok_or("the native mount was already removed")?;
    if !current.1
        || pinned_id != current.0
        || lease
            .identity
            .as_ref()
            .is_some_and(|expected| expected != pinned_id)
    {
        return Err("the mount instance changed; it was preserved".into());
    }
    let target = CString::new(format!("/proc/self/fd/{fd}")).unwrap();
    if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(format!("CIFS unmount: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn observed(lease: &Lease) -> Result<Option<(String, bool)>, String> {
    // getfsstat(MNT_NOWAIT) uses the kernel's cached mount inventory instead of
    // issuing statfs I/O to a sleeping/disconnected SMB source.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut entries = vec![std::mem::MaybeUninit::<libc::statfs>::uninit(); count as usize + 16];
    let bytes = (entries.len() * std::mem::size_of::<libc::statfs>())
        .try_into()
        .map_err(|_| "mount inventory is too large")?;
    let filled = unsafe { libc::getfsstat(entries.as_mut_ptr().cast(), bytes, libc::MNT_NOWAIT) };
    if filled < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if filled as usize >= entries.len() {
        return Err("mount inventory changed during inspection; retry".into());
    }
    let text = |chars: &[libc::c_char]| {
        unsafe { std::ffi::CStr::from_ptr(chars.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    let Some(stat) = entries
        .into_iter()
        .take(filled as usize)
        .map(|entry| unsafe { entry.assume_init() })
        .find(|stat| Path::new(&text(&stat.f_mntonname)) == lease.mount)
    else {
        return Ok(None);
    };
    let source = text(&stat.f_mntfromname);
    let with_port = format!("//ams@{}:{}/{}", lease.server, lease.port, lease.share);
    let without_port = format!("//ams@{}/{}", lease.server, lease.share);
    let matches =
        text(&stat.f_fstypename) == "smbfs" && (source == with_port || source == without_port);
    // Darwin's fsid_t is two signed 32-bit values; keep the opaque mount
    // instance identity without accessing libc's private struct fields.
    let fsid: [i32; 2] = unsafe { std::mem::transmute(stat.f_fsid) };
    Ok(Some((format!("{fsid:?}"), matches)))
}

#[cfg(target_os = "macos")]
fn mount_native(lease: &Lease, password: &str) -> Result<(), String> {
    use core_foundation::{
        array::CFArrayRef,
        dictionary::CFMutableDictionaryRef,
        string::CFStringRef,
        url::{CFURLCreateWithString, CFURLRef},
    };
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        dictionary::CFMutableDictionary,
        string::CFString,
        url::CFURL,
    };
    #[link(name = "NetFS", kind = "framework")]
    extern "C" {
        fn NetFSMountURLSync(
            url: CFURLRef,
            mount: CFURLRef,
            user: CFStringRef,
            password: CFStringRef,
            open: CFMutableDictionaryRef,
            options: CFMutableDictionaryRef,
            result: *mut CFArrayRef,
        ) -> i32;
    }
    let address = CFString::new(&format!(
        "smb://{}:{}/{}",
        lease.server, lease.port, lease.share
    ));
    let raw_url = unsafe {
        CFURLCreateWithString(
            std::ptr::null(),
            address.as_concrete_TypeRef(),
            std::ptr::null(),
        )
    };
    if raw_url.is_null() {
        return Err("invalid SMB mount URL".into());
    }
    let url: CFURL = unsafe { TCFType::wrap_under_create_rule(raw_url) };
    // The exact mount directory must not acquire a trailing URL separator.
    let mount = CFURL::from_path(&lease.mount, false).ok_or("invalid SMB mount path")?;
    let user = CFString::new("ams");
    let secret = CFString::new(password);
    let yes = CFBoolean::true_value().as_CFType();
    let open: CFMutableDictionary<CFString, CFType> = CFMutableDictionary::from_CFType_pairs(&[
        (CFString::new("AllowLoopback"), yes.clone()),
        (CFString::new("ForceNewSession"), yes.clone()),
        (CFString::new("NoUserPreferences"), yes.clone()),
        (CFString::new("UIOption"), CFString::new("NoUI").as_CFType()),
    ]);
    let options: CFMutableDictionary<CFString, CFType> = CFMutableDictionary::from_CFType_pairs(&[
        (CFString::new("MountAtMountDir"), yes),
        (
            CFString::new("SoftMount"),
            CFBoolean::true_value().as_CFType(),
        ),
        (CFString::new("UIOption"), CFString::new("NoUI").as_CFType()),
    ]);
    let mut result = std::ptr::null();
    let status = unsafe {
        NetFSMountURLSync(
            url.as_concrete_TypeRef(),
            mount.as_concrete_TypeRef(),
            user.as_concrete_TypeRef(),
            secret.as_concrete_TypeRef(),
            open.as_concrete_TypeRef(),
            options.as_concrete_TypeRef(),
            &mut result,
        )
    };
    if !result.is_null() {
        unsafe { core_foundation::base::CFRelease(result.cast()) };
    }
    if status != 0 {
        return Err(format!("native macOS SMB mount failed ({status})"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn unmount_native(lease: &Lease) -> Result<(), String> {
    let target =
        CString::new(lease.mount.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    if unsafe { libc::unmount(target.as_ptr(), libc::MNT_FORCE) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn native_operation(request: Request) -> Result<Option<String>, String> {
        if std::env::var_os("ALLMYSTUFF_TEST_NATIVE_SMB_HELPER").is_some() {
            return helper(&request).await;
        }
        #[cfg(target_os = "macos")]
        return Err("set ALLMYSTUFF_TEST_NATIVE_SMB_HELPER to the built node executable; NetFS must run in its main-thread helper".into());
        #[cfg(target_os = "linux")]
        tokio::task::spawn_blocking(move || execute(&request))
            .await
            .map_err(|e| e.to_string())?
    }
    fn lease() -> Lease {
        Lease {
            mount: PathBuf::from("/home/user/AMS share"),
            server: Ipv4Addr::new(127, 65, 1, 2),
            share: "ams0123456789abcdef".into(),
            port: 34567,
            uid: 1000,
            gid: 1000,
            generation: "first".into(),
            identity: Some("42".into()),
        }
    }
    #[test]
    fn native_smb_leases_are_private_and_generation_specific() {
        let first = lease();
        assert!(valid_endpoint(&first));
        let mut second = first.clone();
        second.generation = "replacement".into();
        assert!(first != second);
        second.server = Ipv4Addr::new(192, 168, 1, 1);
        assert!(!valid_endpoint(&second));
        second.server = first.server;
        second.share = "public".into();
        assert!(!valid_endpoint(&second));
        assert!(!serde_json::to_string(&first).unwrap().contains("password"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn native_smb_ownership_requires_exact_mount_source_port_and_instance() {
        let lease = lease();
        let row = "42 1 0:5 / /home/user/AMS\\040share rw - cifs //127.65.1.2/ams0123456789abcdef rw,port=34567,uid=1000";
        assert_eq!(linux_observed(row, &lease), Some(("42".into(), true)));
        assert_eq!(
            linux_observed(&row.replace("34567", "3456"), &lease),
            Some(("42".into(), false))
        );
        assert_eq!(
            linux_observed(&row.replace(",port=34567", ""), &lease),
            Some(("42".into(), true))
        );
        assert_eq!(
            linux_observed(&row.replace("cifs", "fuse.rclone"), &lease),
            Some(("42".into(), false))
        );
        assert_eq!(
            linux_observed(&row.replace("ams0123456789abcdef", "different"), &lease),
            Some(("42".into(), false))
        );
        assert_eq!(
            linux_observed(&row.replace("42 1", "43 1"), &lease),
            Some(("43".into(), true))
        );
        assert_eq!(
            linux_observed(&row.replace("AMS\\040share", "other"), &lease),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "uses the OS SMB client; Linux requires mount privileges and CIFS support"]
    async fn native_smb_client_reads_and_writes_beyond_four_gib() {
        use std::io::{Seek, SeekFrom};
        #[cfg(target_os = "linux")]
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "run this native CIFS test as root"
        );
        struct Cleanup {
            root: PathBuf,
            lease: Lease,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                match observed(&self.lease) {
                    Ok(Some((_, true))) => {
                        if unmount_native(&self.lease).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    _ => return, // Preserve a replacement or uninspectable mount.
                }
                // NetFS may leave its now-empty, user-owned volume directory.
                #[cfg(target_os = "macos")]
                let _ = std::fs::remove_dir(&self.lease.mount);
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
        // Source data stays in a disposable user-owned tree on both platforms.
        #[cfg(target_os = "macos")]
        let fixture_parent = dirs::home_dir().expect("native mount user's home");
        #[cfg(not(target_os = "macos"))]
        let fixture_parent = std::env::temp_dir();
        let root = fixture_parent.join(format!(
            "ams-native-smb-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        #[cfg(target_os = "macos")]
        let mount = Path::new("/Volumes").join(format!(
            "{} native mount",
            root.file_name().unwrap().to_string_lossy()
        ));
        #[cfg(target_os = "linux")]
        let mount = root.join("native mount");
        std::fs::create_dir_all(&source).unwrap();
        #[cfg(target_os = "linux")]
        std::fs::create_dir(&mount).unwrap();
        let mut cleanup = Cleanup {
            root: root.clone(),
            lease: Lease {
                mount: canonical_mount(&mount).unwrap(),
                ..lease()
            },
        };
        let hosts = crate::smb_drives::SmbHosts::default();
        let endpoint = hosts.start("native-client-test", &source).await.unwrap();
        // Use the application's private client identity and TCP bridge rather
        // than sharing macOS's localhost credential/session namespace.
        struct Tunnel(tokio::task::JoinHandle<()>);
        impl Drop for Tunnel {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let listener = crate::smb_drives::client_listener(&root.to_string_lossy())
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server_port = endpoint.port;
        let tunnel = Tunnel(tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let Ok((mut client, _)) = incoming else { break; };
                        connections.spawn(async move {
                            if let Ok(mut source) = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, server_port)).await {
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut source).await;
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        }));
        let mut spec = lease();
        spec.mount = canonical_mount(&mount).unwrap();
        spec.server = match address.ip() {
            std::net::IpAddr::V4(ip) => ip,
            _ => unreachable!(),
        };
        spec.port = address.port();
        spec.share = endpoint.share;
        spec.uid = unsafe { libc::geteuid() };
        spec.gid = unsafe { libc::getegid() };
        spec.identity = None;
        cleanup.lease = spec.clone();
        let offset = 4 * 1024 * 1024 * 1024 + 73;
        // Prepare the hole at the source. A native client may zero-fill a
        // remote extension if it does not negotiate sparse-file support,
        // turning an offset test into an unintended 4 GiB bulk transfer.
        let sparse = std::fs::File::create(source.join("large-offset.bin")).unwrap();
        sparse.set_len(offset + 14).unwrap();
        assert!(sparse.metadata().unwrap().blocks() * 512 < 1024 * 1024);
        drop(sparse);
        println!("mounting the isolated native SMB fixture");
        spec.identity = native_operation(Request {
            lease: spec.clone(),
            password: Some(endpoint.password),
        })
        .await
        .unwrap();
        let mounted = spec;
        cleanup.lease = mounted.clone();
        println!("checking native file operations above 4 GiB");
        let source_copy = source.clone();
        let mount_copy = mount.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(mount_copy.join("large-offset.bin"))
                .unwrap();
            file.write_all(b"start").unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(b"above-four-gib").unwrap();
            crate::persist::sync_file(&file).unwrap();
            drop(file);
            let source_path = source_copy.join("large-offset.bin");
            assert_eq!(std::fs::metadata(&source_path).unwrap().len(), offset + 14);
            let mut remote = std::fs::File::open(&source_path).unwrap();
            remote.seek(SeekFrom::Start(offset)).unwrap();
            let mut tail = [0; 14];
            remote.read_exact(&mut tail).unwrap();
            assert_eq!(&tail, b"above-four-gib");
            let mut external = std::fs::File::create(source_copy.join("source-edit.bin")).unwrap();
            external.seek(SeekFrom::Start(offset + 128)).unwrap();
            external.write_all(b"read-back").unwrap();
            external.sync_all().unwrap();
            drop(external);
            let mut mapped = std::fs::File::open(mount_copy.join("source-edit.bin")).unwrap();
            mapped.seek(SeekFrom::Start(offset + 128)).unwrap();
            let mut back = [0; 9];
            mapped.read_exact(&mut back).unwrap();
            assert_eq!(&back, b"read-back");
            drop(mapped);
            let mut small = std::fs::File::create(mount_copy.join("created.txt")).unwrap();
            small.write_all(b"native-create").unwrap();
            crate::persist::sync_file(&small).unwrap();
            drop(small);
            assert_eq!(
                std::fs::read(source_copy.join("created.txt")).unwrap(),
                b"native-create"
            );
            std::fs::remove_file(mount_copy.join("created.txt")).unwrap();
            std::fs::rename(
                mount_copy.join("large-offset.bin"),
                mount_copy.join("renamed.bin"),
            )
            .unwrap();
            assert!(source_copy.join("renamed.bin").is_file());
            std::fs::remove_file(mount_copy.join("renamed.bin")).unwrap();
            assert!(!source_copy.join("renamed.bin").exists());
        })
        .await
        .unwrap();
        println!("unmounting and removing the native SMB fixture");
        native_operation(Request {
            lease: mounted,
            password: None,
        })
        .await
        .unwrap();
        assert!(observed(&cleanup.lease).unwrap().is_none());
        hosts.stop("native-client-test");
        drop(tunnel);
        drop(cleanup);
        assert!(
            !root.exists(),
            "all sparse test files and the test mount were removed"
        );
    }
}
