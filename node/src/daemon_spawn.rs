//! Daemon lifecycle. AllMyStuff drives a `myownmesh serve` daemon as its
//! mesh sidecar; if one isn't already listening, we spawn it and hold the
//! handle for the app's lifetime (killing the GUI kills the child via
//! `Drop`). This mirrors the MyOwnMesh GUI's daemon spawner, minus the
//! source-checkout dev path — AllMyStuff ships against an installed
//! `myownmesh` (pinned in `.myownmesh-rev`), found on `$PATH` or via the
//! `MYOWNMESH_BIN` override. A binary that's fallen behind the pin is
//! asked to update itself (`myownmesh update`, the same thing the
//! installer invokes) before we start it, so the mesh comes up with the
//! features this app was built against.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::control_client::{ControlClient, Request};

/// The old CEC support area and its corrected replacement deliberately share
/// a well-known network id. Legacy clients therefore announce `Open/0` while
/// current clients announce `Silent/0`. MyOwnMesh reports that immutable
/// mismatch on every signaling heartbeat even though neither zero-transition
/// state can converge into the other. Keep the first diagnostic per peer, but
/// do not let the same compatibility fact consume the AllMyStuff log forever.
fn stable_open_silent_drift_key(line: &str) -> Option<String> {
    let (_, drift) = line.split_once("governance: governance drift with ")?;
    let legacy_mismatch = drift.ends_with("local Silent/0 vs theirs Open/0")
        || drift.ends_with("local Open/0 vs theirs Silent/0");
    legacy_mismatch.then(|| drift.to_string())
}

fn should_emit_daemon_line(
    line: &str,
    seen_open_silent_drift: &std::sync::Mutex<std::collections::HashSet<String>>,
) -> bool {
    let Some(key) = stable_open_silent_drift_key(line) else {
        return true;
    };
    // A poisoned diagnostic cache must never hide logs; fail open.
    seen_open_silent_drift
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(true)
}

/// Forward a spawned daemon's output to the node's inherited stream while
/// applying the narrow compatibility dedupe above. The mesh control protocol
/// uses its local socket, not stdio, so piping the human log cannot affect it.
fn pump_daemon_output<R: std::io::Read + Send + 'static>(
    reader: R,
    stderr: bool,
    seen_open_silent_drift: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        use std::io::Write as _;
        let persist = crate::diagnostics::debug_logging_enabled();
        for line in std::io::BufReader::new(reader)
            .lines()
            .map_while(Result::ok)
        {
            if !should_emit_daemon_line(&line, &seen_open_silent_drift) {
                continue;
            }
            if persist {
                // File logging is a tracing layer, not a tee of process stdout.
                // Keep the original timestamp/level in this bounded record;
                // inherited Windows GUI stdout alone would lose this evidence.
                crate::diagnostics::record_daemon_line(&line, stderr);
            }
            if stderr {
                // Keep draining even if our own stderr disappeared. Dropping
                // this reader would eventually fill the child's pipe and let
                // human-facing logging stall the mesh daemon.
                let _ = writeln!(std::io::stderr(), "{line}");
            } else {
                let _ = writeln!(std::io::stdout(), "{line}");
            }
        }
    });
}

/// Stop a supervised child gracefully: on unix send `SIGTERM` and give it
/// up to ~2s to exit on its own — a clean shutdown that lets the daemon
/// cascade its own teardown — then `SIGKILL` whatever's left. On Windows
/// there's no graceful signal in std, but the job-object tie
/// ([`tie_daemon_lifetime`]) already cascades a kill-on-close, so a plain
/// `Child::kill` is the right (and only) move.
///
/// Marked `pub(crate)` so [`crate::node_control::NodeChild`] can reuse the
/// exact same teardown for the `allmystuff-serve` child it supervises.
pub(crate) fn graceful_kill(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` with a pid we own; an ESRCH (already-reaped)
        // just no-ops. We never reuse a pid we haven't `wait`ed.
        let pid = child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // The unix branch falls through here for the still-alive case (SIGKILL);
    // Windows takes this path directly.
    let _ = child.kill();
}

/// Ask the orphaned daemon at `pid` to shut down. On unix that's a plain
/// `SIGTERM` (the daemon's clean-shutdown signal); on Windows std has no
/// signal, so we go through sysinfo's `Process::kill` (which is also how we
/// verified the pid is myownmesh, so the lookup is cheap and the gate the
/// same). Only ever called for a pid we've already confirmed is *our*
/// myownmesh orphan.
fn stop_orphan(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)`; an ESRCH (already gone) just no-ops.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let spid = Pid::from_u32(pid);
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        if let Some(proc) = sys.process(spid) {
            proc.kill();
        }
    }
}

/// Owned wrapper around a spawned `myownmesh serve` child. Dropping it
/// stops the child ([`graceful_kill`]) and, if the daemon pidfile still
/// points at *this* child, removes it — so the next run's self-heal sees a
/// clean slate rather than a stale pid.
pub struct DaemonChild {
    child: Option<Child>,
    /// The pid we recorded in [`daemon_pidfile`] when we spawned, so `drop`
    /// can clear the file — but only while it still names *us* (a daemon
    /// that replaced ours since then owns the file now).
    pid: u32,
}

impl DaemonChild {
    fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            child: Some(child),
            pid,
        }
    }

    /// Whether the supervised daemon has exited, without blocking — the
    /// supervisor's poll. `Some(status)` once it's gone (respawn time),
    /// `None` while it runs. A `try_wait` error reads as still-running:
    /// the control-socket probe is the backstop, and killing a healthy
    /// daemon over a transient wait error would be the worse mistake.
    pub fn try_exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            graceful_kill(&mut c);
            let _ = c.wait();
            // Only clear the pidfile if it still points at us — a fresh
            // daemon (ours or a foreign one) that took the file over since
            // we wrote it must keep its own record.
            if let Some(path) = daemon_pidfile() {
                if read_pidfile(&path).map(|(pid, _)| pid) == Some(self.pid) {
                    let _ = std::fs::remove_file(&path);
                }
            }
            tracing::info!("myownmesh daemon child terminated");
        }
    }
}

/// `<MYOWNMESH_HOME or ~/.myownmesh>/allmystuff-daemon.pid` — the record of
/// which `myownmesh` daemon *we* spawned, honouring `MYOWNMESH_HOME` exactly
/// like [`crate::ownership`]'s store and the control socket. `None` when no
/// home dir resolves (an ephemeral/test environment with neither set).
fn daemon_pidfile() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-daemon.pid"))
}

/// Write `pid` to the daemon pidfile, creating `~/.myownmesh` first. Best
/// effort — a failed write just means the *next* run can't recognise this
/// daemon as ours and will reuse it like a foreign one (never the wrong,
/// destructive direction).
fn write_pidfile(pid: u32, start_time: Option<u64>) {
    let Some(path) = daemon_pidfile() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // `<pid> <start_time>` — the start time is what defeats pid reuse on the
    // self-heal sweep (a process that inherited the pid after ours died has a
    // different start time). Best-effort: if we couldn't read it, write the pid
    // alone, and the sweep falls back to the myownmesh name check.
    let body = match start_time {
        Some(t) => format!("{pid} {t}"),
        None => pid.to_string(),
    };
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!("couldn't record the daemon pid at {}: {e}", path.display());
    }
}

/// Read the recorded daemon pid and its process start time from the pidfile.
/// The start time is `None` for an older single-pid file (still honoured, with
/// the name check alone). `None` overall for a missing or garbage file — both
/// mean "we have no daemon of our own on record", the safe default.
fn read_pidfile(path: &Path) -> Option<(u32, Option<u64>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let start = parts.next().and_then(|s| s.parse().ok());
    Some((pid, start))
}

/// `(is-myownmesh, start-time-secs)` for the live process at `pid`, or `None`
/// if no such process. One refresh of the single pid. The exe basename (falling
/// back to the name) decides `myownmesh`-ness; the start time is the epoch-second
/// process birth time the sweep matches against the pidfile.
fn daemon_identity(pid: u32) -> Option<(bool, u64)> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let spid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let proc = sys.process(spid)?;
    // Prefer the exe basename (stable, full); fall back to the process name
    // (truncated to 15 chars on Linux, but "myownmesh" fits).
    let exe_base = proc
        .exe()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let name = proc.name().to_string_lossy().into_owned();
    let is_myownmesh = exe_base.unwrap_or(name).starts_with("myownmesh");
    Some((is_myownmesh, proc.start_time()))
}

/// True when `pid` is alive, really a `myownmesh` process, **and** its start
/// time matches what we recorded — the gate that lets us reclaim our orphan
/// without ever signalling a process that merely inherited its pid after our
/// daemon died (pid reuse). The start-time match is what closes that window: a
/// reused pid belongs to a process born at a different time. An older pidfile
/// with no recorded start time falls back to the name check alone, as before.
fn pid_is_our_daemon(pid: u32, want_start: Option<u64>) -> bool {
    let Some((is_myownmesh, start)) = daemon_identity(pid) else {
        return false;
    };
    if !is_myownmesh {
        return false;
    }
    match want_start {
        Some(want) => want == start,
        None => true,
    }
}

/// Tie the spawned daemon's lifetime to this process at the OS level —
/// the `Drop` kill above only covers a *clean* exit, and an app that
/// crashes or is force-killed (taskkill, a dev-loop Ctrl-C that doesn't
/// reach us, an OOM kill) orphans the daemon. An orphan is worse than a
/// leak: it keeps this machine's identity live on the mesh and silently
/// swallows control traffic addressed to the dead app, and on Windows it
/// also holds the sidecar exe locked so the *next build* fails copying it.
/// Only ever called for a daemon **we spawned** — an externally-started
/// daemon we merely reuse is never touched.
///
///  * **Windows**: assign the child to a job object with
///    `KILL_ON_JOB_CLOSE`. The job handle is deliberately leaked — the
///    kernel closes it when this process ends (any way at all), and that
///    closure kills the daemon.
///  * **Linux**: `PR_SET_PDEATHSIG(SIGKILL)` (set in `pre_exec` at spawn).
///  * **macOS**: no kernel-level parent-death signal exists; the `Drop`
///    kill remains the cover for clean exits.
#[cfg(windows)]
fn tie_daemon_lifetime(child: &Child) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::warn!("couldn't create a job object for the daemon — a crash may orphan it");
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle() as _) != 0;
        if ok {
            tracing::info!("daemon tied to this process (job object, kill-on-close)");
            // The job handle must live exactly as long as this process:
            // leaking it hands the close — and so the kill — to the kernel.
        } else {
            tracing::warn!("couldn't tie the daemon to this process — a crash may orphan it");
            CloseHandle(job);
        }
    }
}

#[cfg(not(windows))]
fn tie_daemon_lifetime(_child: &Child) {
    // Linux is handled in `pre_exec` (PR_SET_PDEATHSIG); macOS has no
    // kernel-level equivalent.
}

/// True when a daemon is already answering on the control socket.
pub async fn probe(client: &ControlClient) -> bool {
    client.request(&Request::Status).await.is_ok()
}

/// `"v0.2.4"` / `"0.2.4"` / `"0.2.4-rc.1"` → `(0, 2, 4)`. Missing
/// minor/patch fields compare as 0 (the installer's `version_ge` does
/// the same). `None` when the major field isn't numeric — callers gate
/// sha pins out themselves (they don't start with `v`).
pub(crate) fn parse_semverish(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut nums = [None::<u64>; 3];
    for (i, part) in s.splitn(3, '.').enumerate() {
        let end = part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(part.len());
        nums[i] = part[..end].parse().ok();
    }
    Some((nums[0]?, nums[1].unwrap_or(0), nums[2].unwrap_or(0)))
}

/// First line of `myownmesh --version` ("myownmesh 0.2.4") → `(0, 2, 4)`.
pub(crate) fn parse_version_output(out: &str) -> Option<(u64, u64, u64)> {
    parse_semverish(out.lines().next()?.split_whitespace().last()?)
}

pub(crate) fn fmt_ver((a, b, c): (u64, u64, u64)) -> String {
    format!("{a}.{b}.{c}")
}

/// The app's daemon pin, when it's a comparable version tag (`vX.Y.Z`).
/// A sha pin can't be compared, so every version passes then — same
/// rule as the installer's `mesh_min_version`.
fn pinned_version() -> Option<(&'static str, (u64, u64, u64))> {
    let pin = option_env!("MYOWNMESH_PIN")?;
    let want = parse_semverish(pin.strip_prefix('v')?)?;
    Some((pin, want))
}

/// The MyOwnMesh release pin compiled into this AllMyStuff node.
pub fn daemon_pin() -> Option<&'static str> {
    option_env!("MYOWNMESH_PIN")
}

/// Read the version of the daemon binary this process would launch. This is
/// distinct from the version answering the socket; Settings shows both the
/// on-disk repair target and the live daemon through the node status RPC.
pub async fn installed_daemon_version() -> anyhow::Result<Option<String>> {
    let (bin, _) = find_daemon_binary()?;
    Ok(binary_version(&bin).await.map(fmt_ver))
}

/// Ask an installed MyOwnMesh binary to repair/update itself. Development
/// artifacts and explicit overrides are intentionally not overwritten.
pub async fn repair_installed_daemon() -> anyhow::Result<Option<String>> {
    let (bin, source) = find_daemon_binary()?;
    if source != DaemonSource::Installed {
        anyhow::bail!("this MyOwnMesh binary is a development build or explicit override; rebuild or replace it at its configured path")
    }
    let _ = run_daemon_update(&bin).await;
    Ok(binary_version(&bin).await.map(fmt_ver))
}

/// `bin --version`, parsed. `None` when the binary won't answer.
async fn binary_version(bin: &Path) -> Option<(u64, u64, u64)> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

/// `myownmesh update` downloads a release binary, so give it real time —
/// but never wedge mesh bring-up forever on a stalled network.
const DAEMON_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);

/// Run `<bin> update` — the daemon's own self-updater, the same thing
/// the installer invokes — and report whether the binary on disk now
/// satisfies the pin. Its output is folded into our log; failure never
/// propagates (an old daemon still beats no daemon).
async fn run_daemon_update(bin: &Path) -> bool {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("update")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match tokio::time::timeout(DAEMON_UPDATE_TIMEOUT, cmd.output()).await {
        Err(_) => {
            tracing::warn!(
                "myownmesh update didn't finish within {}s — continuing with what's on disk",
                DAEMON_UPDATE_TIMEOUT.as_secs()
            );
        }
        Ok(Err(e)) => tracing::warn!("couldn't run myownmesh update: {e}"),
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let said = stdout.trim();
            if !said.is_empty() {
                tracing::info!("myownmesh update: {said}");
            }
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::warn!("myownmesh update failed: {}", stderr.trim());
            }
        }
    }
    // The re-check is what decides — `update` may have been refused
    // (package-manager install), failed, or landed exactly the pin.
    match (pinned_version(), binary_version(bin).await) {
        (Some((_, want)), Some(have)) => have >= want,
        _ => false,
    }
}

/// When the daemon binary we're about to start is older than the app's
/// pin, update it first. A stale daemon is the #1 way "the app updated
/// but a feature didn't appear" happens — it answers the socket fine,
/// so everything *looks* up, but the newer media lanes (the video track
/// lane screens ride, the Opus audio lane) simply don't exist in it.
async fn ensure_daemon_current(bin: &Path) {
    let Some((pin, want)) = pinned_version() else {
        return;
    };
    match binary_version(bin).await {
        None => tracing::warn!(
            "couldn't read {}'s version to compare against the {pin} pin",
            bin.display()
        ),
        Some(have) if have >= want => {}
        Some(have) => {
            tracing::info!(
                "myownmesh at {} is v{} but this app pins {pin} — asking it to update itself (myownmesh update)…",
                bin.display(),
                fmt_ver(have)
            );
            if run_daemon_update(bin).await {
                tracing::info!("myownmesh is current — starting the updated daemon");
            } else {
                tracing::warn!(
                    "couldn't bring myownmesh up to {pin}; starting the old daemon — the newer mesh features (e.g. the video track lane that screens ride) stay unavailable. Update it by hand: myownmesh update"
                );
            }
        }
    }
}

/// Compare the answering daemon's version against the app's pin and log
/// the verdict — loudly on mismatch. Returns `true` when the daemon is
/// confirmed older than the pin. Only meaningful when the pin is a
/// version tag (`vX.Y.Z`); a sha pin can't be compared.
pub async fn log_daemon_version(client: &ControlClient) -> bool {
    let Some((pin, want)) = pinned_version() else {
        return false;
    };
    let running = client
        .request(&Request::Status)
        .await
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| d.get("version").and_then(|v| v.as_str()).map(String::from));
    match running {
        Some(v) => match parse_semverish(&v) {
            Some(have) if have >= want => {
                tracing::info!("myownmesh daemon v{v} (satisfies the {pin} pin)");
                false
            }
            Some(_) => {
                tracing::warn!(
                    "myownmesh daemon is v{v} but this app pins {pin} — features the newer daemon carries (e.g. the video track lane) will be unavailable. If this is a dev setup, rebuild the sibling MyOwnMesh checkout (or remove its stale binary so build.rs fetches the pinned release) and restart the app."
                );
                true
            }
            None => {
                tracing::warn!(
                    "myownmesh daemon reported an unreadable version ({v}) against the {pin} pin"
                );
                false
            }
        },
        None => {
            tracing::warn!("couldn't read the daemon version to compare against the {pin} pin");
            false
        }
    }
}

/// The target triple `build.rs` bundled the sidecar for. In a dev build
/// Tauri keeps the `-<triple>` suffix on the staged sidecar; a production
/// bundle strips it to plain `myownmesh{.exe}`.
const DAEMON_SIDECAR_TRIPLE: &str = env!("DAEMON_SIDECAR_TRIPLE");

/// True when a path is a real, non-empty binary (not a zero-byte sidecar
/// stub `build.rs` wrote when it couldn't fetch the daemon).
fn usable(p: &std::path::Path) -> bool {
    p.metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Where `find_daemon_binary` found the daemon — decides whether the
/// binary is ours to keep current.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DaemonSource {
    /// Explicit `MYOWNMESH_BIN` override — deliberately pinned, never
    /// touched.
    Override,
    /// Installed for the user: the production bundle's sidecar or a
    /// `myownmesh` on `$PATH` (what the installer drops). Kept current
    /// against the pin by asking it to update itself.
    Installed,
    /// A dev artifact (the dev-staged sidecar, the `build.rs` source
    /// slot, a sibling checkout's target dir) — never touched;
    /// self-updating one would clobber build output with a release
    /// download.
    DevBuild,
}

/// Locate the `myownmesh` daemon. It normally ships *with the app* — bundled
/// as a Tauri sidecar by `build.rs` (see that file) — so this resolves the
/// bundled binary first and only falls back for unusual setups:
///
/// 1. `MYOWNMESH_BIN` override.
/// 2. **Bundled sidecar** next to the app binary — `myownmesh{.exe}`
///    (production) or `myownmesh-<triple>{.exe}` (dev).
/// 3. **Dev source slot** — `gui/src-tauri/binaries/myownmesh-<triple>`
///    (the path `build.rs` writes; reachable in a dev run via the
///    build-time manifest dir).
/// 4. Side-by-side `../MyOwnMesh` source build.
/// 5. `myownmesh` on `$PATH`.
pub fn find_daemon_binary() -> Result<(PathBuf, DaemonSource)> {
    let exe = if cfg!(windows) {
        "myownmesh.exe"
    } else {
        "myownmesh"
    };
    let exe_triple = if cfg!(windows) {
        format!("myownmesh-{DAEMON_SIDECAR_TRIPLE}.exe")
    } else {
        format!("myownmesh-{DAEMON_SIDECAR_TRIPLE}")
    };

    // 1. Override.
    if let Ok(p) = std::env::var("MYOWNMESH_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok((p, DaemonSource::Override));
        }
    }

    // 2. Bundled sidecar next to the running app binary. The plain name
    // is the production bundle; the triple-suffixed one is Tauri's dev
    // staging of the source slot.
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(dir) = exe_path.parent() {
            for (name, source) in [
                (exe, DaemonSource::Installed),
                (exe_triple.as_str(), DaemonSource::DevBuild),
            ] {
                let p = dir.join(name);
                if usable(&p) {
                    return Ok((p, source));
                }
            }
        }
    }

    // 3. Dev source slot written by the GUI's build.rs. That stages the
    // sidecar under `gui/src-tauri/binaries`; this engine crate sits beside
    // `gui/` at the repo root (`node/`), so reach across to it from the
    // build-time manifest dir.
    if let Some(dev_slot) = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // repo root (AllMyStuff/)
        .map(|root| {
            root.join("gui")
                .join("src-tauri")
                .join("binaries")
                .join(&exe_triple)
        })
    {
        if usable(&dev_slot) {
            return Ok((dev_slot, DaemonSource::DevBuild));
        }
    }

    // 4. Side-by-side MyOwnMesh checkout (release first, then debug).
    for profile in ["release", "debug"] {
        if let Some(p) = sibling_myownmesh_path(profile, exe) {
            if p.exists() {
                return Ok((p, DaemonSource::DevBuild));
            }
        }
    }

    // 5. PATH walk (skip stale, non-existent entries).
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.exists() {
                return Ok((candidate, DaemonSource::Installed));
            }
        }
    }
    Err(anyhow!(
        "couldn't find the `myownmesh` daemon — it normally ships bundled with \
         the app; build from source (so build.rs bundles it), put `myownmesh` \
         on PATH, or set MYOWNMESH_BIN"
    ))
}

/// `../MyOwnMesh/target/<profile>/myownmesh` relative to the AllMyStuff repo
/// root. CARGO_MANIFEST_DIR here is `node/`, so the repo root is one parent
/// up and the side-by-side checkout one more.
fn sibling_myownmesh_path(profile: &str, exe: &str) -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Some(
        PathBuf::from(manifest_dir)
            .parent()? // AllMyStuff/
            .parent()? // workspace dir (AllMyStuff + MyOwnMesh side by side)
            .join("MyOwnMesh")
            .join("target")
            .join(profile)
            .join(exe),
    )
}

/// How long we wait for an orphan we just SIGTERM-ed to stop answering the
/// control socket before we give up replacing it. A clean `myownmesh`
/// shutdown is near-instant; this is slack for a wedged one mid-teardown.
const ORPHAN_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Reuse the daemon already answering the socket *as today*: log its version
/// against the pin and, when it's stale and ours-to-keep-current, refresh the
/// binary on disk for the next start. Always returns `Ok(None)` — "a daemon is
/// already running; step aside". Factored out so both reuse paths (foreign
/// daemon, and an orphan we couldn't cleanly replace) share one body.
async fn reuse_running_daemon(client: &ControlClient) -> Result<Option<DaemonChild>> {
    if log_daemon_version(client).await {
        // The running daemon is stale, but it isn't ours to restart (an
        // externally-started daemon, or one we couldn't replace). Refresh the
        // binary on disk so the *next* daemon start runs the pinned features.
        if let Ok((bin, DaemonSource::Installed)) = find_daemon_binary() {
            if run_daemon_update(&bin).await {
                tracing::warn!(
                    "updated myownmesh on disk, but the running daemon keeps the old version until it restarts — quit whatever started it (or reboot) and relaunch the app"
                );
            }
        }
    }
    Ok(None)
}

/// Spawn `myownmesh serve` and wait briefly for its socket. Returns
/// `Ok(None)` when a daemon is already running (we reuse it).
///
/// **Self-heal**: when a daemon is already answering, we normally reuse it —
/// *except* when it's an orphan we started in a previous run (the GUI
/// SIGKILLs the node on exit, so on macOS the node never gets to cascade its
/// own shutdown and the daemon is left behind). A pidfile records which
/// daemon was ours; if the answering daemon is that orphan we SIGTERM it and
/// spawn a fresh one, so a wedged transport from the dead run can't be
/// inherited. A daemon we *didn't* start (a user's own, the MyOwnMesh app's,
/// a `MYOWNMESH_BIN`-pinned one) is never touched.
pub async fn ensure_daemon_running(client: &ControlClient) -> Result<Option<DaemonChild>> {
    if probe(client).await {
        tracing::info!("existing myownmesh daemon found on the control socket");

        // A user-pinned/managed binary (`MYOWNMESH_BIN`) is deliberately out
        // of our hands — never restart whatever it points at.
        if std::env::var_os("MYOWNMESH_BIN").is_some() {
            return reuse_running_daemon(client).await;
        }

        // Is the answering daemon the orphan we started earlier? Only if the
        // pidfile names a live pid that is *actually* a myownmesh process
        // (the sysinfo check guards pid reuse). Anything else — no pidfile, a
        // dead pid, or a live pid that isn't myownmesh — is a daemon we
        // didn't start, so we reuse it untouched.
        let our_orphan = daemon_pidfile()
            .as_deref()
            .and_then(read_pidfile)
            .filter(|&(pid, want_start)| pid_is_our_daemon(pid, want_start))
            .map(|(pid, _)| pid);
        let Some(pid) = our_orphan else {
            return reuse_running_daemon(client).await;
        };

        tracing::warn!(
            "found a stale mesh daemon we started earlier (now orphaned) — restarting it for a clean transport"
        );
        stop_orphan(pid);
        // Wait for it to stop answering, then fall through to spawn a fresh
        // one. If it's *still* answering, we couldn't replace it cleanly —
        // reuse it rather than spawn a conflicting second daemon.
        let deadline = std::time::Instant::now() + ORPHAN_STOP_TIMEOUT;
        loop {
            if !probe(client).await {
                break; // gone — fall through to the spawn path below
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    "the orphaned daemon is still answering after {}s — couldn't replace it cleanly; reusing it",
                    ORPHAN_STOP_TIMEOUT.as_secs()
                );
                return reuse_running_daemon(client).await;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    let (bin, source) = find_daemon_binary().context("locate myownmesh binary")?;
    if source == DaemonSource::Installed {
        ensure_daemon_current(&bin).await;
    }
    tracing::info!(?bin, "spawning myownmesh daemon");

    let mut cmd = Command::new(&bin);
    if crate::diagnostics::debug_logging_enabled() && std::env::var_os("MYOWNMESH_LOG").is_none() {
        // Capture loss attribution at the local fanout, not packet-level RTP
        // chatter. Append the user's extras last so explicit filters still win.
        let extra = std::env::var("MYOWNMESH_LOG_EXTRA").unwrap_or_default();
        cmd.env(
            "MYOWNMESH_LOG_EXTRA",
            format!("myownmesh::ipc::bridge=debug,{extra}"),
        );
    }
    cmd.arg("serve")
        .stdin(Stdio::null())
        // Capture only to forward line-for-line below. This lets AllMyStuff
        // dedupe one known legacy CEC mismatch while preserving every other
        // daemon diagnostic verbatim.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The daemon is a console-subsystem binary and this GUI is windowless,
    // so without CREATE_NO_WINDOW Windows would give the child its own
    // console window, parked on screen for the app's whole lifetime. The
    // inherited stdio handles are unaffected by the flag.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // Linux half of the lifetime tie (see `tie_daemon_lifetime`): SIGKILL
    // the daemon the moment this process dies, however it dies.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    // Record this daemon as ours, so a future run's self-heal can recognise
    // it as our orphan (and `DaemonChild::drop` can clear the record on a
    // clean exit). Pair the pid with its start time so the recogniser can't be
    // fooled by pid reuse. Write before the tie/probe so even an early failure
    // leaves a recoverable pid on disk.
    let start_time = daemon_identity(child.id()).map(|(_, t)| t);
    write_pidfile(child.id(), start_time);
    tie_daemon_lifetime(&child);
    let seen_open_silent_drift =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    if let Some(stdout) = child.stdout.take() {
        pump_daemon_output(stdout, false, seen_open_silent_drift.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        pump_daemon_output(stderr, true, seen_open_silent_drift);
    }
    let handle = DaemonChild::new(child);

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if probe(client).await {
            tracing::info!("myownmesh daemon up");
            log_daemon_version(client).await;
            return Ok(Some(handle));
        }
    }
    tracing::warn!(
        "daemon did not answer within 8s; leaving it running — the event pump will retry"
    );
    Ok(Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semverish_parses_tags_and_bare_versions() {
        assert_eq!(parse_semverish("v0.2.4"), Some((0, 2, 4)));
        assert_eq!(parse_semverish("0.2.4"), Some((0, 2, 4)));
        assert_eq!(parse_semverish("0.10.3"), Some((0, 10, 3)));
        assert_eq!(parse_semverish(" v1.0.0 "), Some((1, 0, 0)));
    }

    #[test]
    fn semverish_missing_fields_compare_as_zero() {
        assert_eq!(parse_semverish("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_semverish("1"), Some((1, 0, 0)));
    }

    #[test]
    fn semverish_ignores_prerelease_suffixes() {
        assert_eq!(parse_semverish("0.2.4-rc.1"), Some((0, 2, 4)));
    }

    #[test]
    fn semverish_rejects_non_versions() {
        assert_eq!(parse_semverish(""), None);
        assert_eq!(parse_semverish("main"), None);
        assert_eq!(parse_semverish("x.2.4"), None);
    }

    #[test]
    fn version_output_takes_the_last_token_of_the_first_line() {
        assert_eq!(parse_version_output("myownmesh 0.2.4\n"), Some((0, 2, 4)));
        assert_eq!(
            parse_version_output("myownmesh 0.2.1\nextra noise\n"),
            Some((0, 2, 1))
        );
        assert_eq!(parse_version_output("garbage"), None);
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn tuple_ordering_matches_semver() {
        // The whole fix rides on this comparison: numeric per-field,
        // not lexicographic on the string ("0.10.0" > "0.2.4").
        assert!((0, 2, 1) < (0, 2, 4));
        assert!((0, 10, 0) > (0, 2, 4));
        assert!((1, 0, 0) > (0, 10, 0));
        assert!((0, 2, 4) >= (0, 2, 4));
    }

    #[test]
    fn legacy_open_silent_drift_logs_once_per_peer() {
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        let first = "2026-08-05T14:28:24Z INFO governance: governance drift with old-peer: local Silent/0 vs theirs Open/0";
        let heartbeat = "2026-08-05T14:31:25Z INFO governance: governance drift with old-peer: local Silent/0 vs theirs Open/0";
        let other_peer = "2026-08-05T14:31:26Z INFO governance: governance drift with another-peer: local Silent/0 vs theirs Open/0";

        assert!(should_emit_daemon_line(first, &seen));
        assert!(!should_emit_daemon_line(heartbeat, &seen));
        assert!(should_emit_daemon_line(other_peer, &seen));
    }

    #[test]
    fn meaningful_governance_drift_is_never_deduped() {
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        let closed =
            "INFO governance: governance drift with peer: local Closed/4 vs theirs Closed/3";
        let transitioned =
            "INFO governance: governance drift with peer: local Silent/1 vs theirs Open/0";

        assert!(should_emit_daemon_line(closed, &seen));
        assert!(should_emit_daemon_line(closed, &seen));
        assert!(should_emit_daemon_line(transitioned, &seen));
        assert!(should_emit_daemon_line(transitioned, &seen));
    }

    #[test]
    fn reverse_open_silent_drift_is_also_the_same_legacy_case() {
        let line = "INFO governance: governance drift with peer: local Open/0 vs theirs Silent/0";
        assert_eq!(
            stable_open_silent_drift_key(line).as_deref(),
            Some("peer: local Open/0 vs theirs Silent/0")
        );
    }

    // The pidfile env reads/writes a process-global (`MYOWNMESH_HOME`), so
    // these tests share one guarded section to stay hermetic under the test
    // runner's threads.
    use std::sync::Mutex;
    static HOME_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn pidfile_resolves_under_myownmesh_home() {
        let _g = HOME_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("ams-pidtest-{}", std::process::id()));
        std::env::set_var("MYOWNMESH_HOME", &tmp);
        let path = daemon_pidfile().expect("a home resolves");
        assert_eq!(
            path,
            tmp.join("allmystuff-daemon.pid"),
            "MYOWNMESH_HOME is the state directory itself"
        );
        std::env::remove_var("MYOWNMESH_HOME");
    }

    #[test]
    fn pidfile_round_trips_and_rejects_garbage() {
        let _g = HOME_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("ams-pidrt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("MYOWNMESH_HOME", &tmp);
        let path = daemon_pidfile().expect("a home resolves");

        // Missing file → None.
        assert_eq!(read_pidfile(&path), None, "no file yet");

        // Write-then-read round-trips the pid *and* its start time.
        write_pidfile(4242, Some(99_887_766));
        assert_eq!(
            read_pidfile(&path),
            Some((4242, Some(99_887_766))),
            "the written pid + start time read back"
        );

        // An older single-pid file still parses, with no start time — the sweep
        // falls back to the name check for it.
        std::fs::write(&path, "4242").unwrap();
        assert_eq!(
            read_pidfile(&path),
            Some((4242, None)),
            "a legacy pid-only file reads back with no start time"
        );

        // Garbage / non-numeric content → None, never a panic.
        std::fs::write(&path, "not-a-pid\n").unwrap();
        assert_eq!(read_pidfile(&path), None, "garbage parses to no pid");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_pidfile(&path), None, "empty file is no pid");

        std::env::remove_var("MYOWNMESH_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
