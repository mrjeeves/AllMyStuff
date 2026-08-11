//! Install / start / stop / uninstall AllMyStuff as a background OS service.
//!
//! Shared by the `allmystuff` CLI (`allmystuff service …`) and the desktop
//! app's "Always On" tab, so both manage the service identically — and so the
//! GUI can do it **in-process** with no separate `allmystuff` binary to find
//! (on unix it calls [`run`] directly; on Windows it re-launches its own
//! binary elevated). Plain function calls, no subprocess to ourselves.
//!
//! This manages the **node process** (`allmystuff-serve`, what `allmystuff
//! serve` runs) under the host init system so it survives logout/reboot. And
//! because the node spawns and supervises a `myownmesh serve` daemon itself,
//! **one service runs both** — the mesh daemon and the AllMyStuff node come
//! up together from a single unit. (`myownmesh` itself is found on `$PATH` /
//! `MYOWNMESH_BIN` / the bundled sidecar, exactly as the desktop app finds
//! it; install it with the AllMyStuff installer or `myownmesh`'s own.)
//!
//! It mirrors `myownmesh service` so the two feel identical. Two scopes:
//!
//! - **user** (default) — a per-user service that needs no root, keeps state
//!   in `~/.myownmesh`, and starts at login. On Linux we also try to enable
//!   lingering so it runs while you're logged out.
//! - **system** (`--system`) — a root-owned service that starts at boot and
//!   runs with its own state under a system directory. Requires root.
//!
//! Three backends, picked by target OS:
//!
//! - **Linux → systemd.** An `allmystuff.service` unit under
//!   `~/.config/systemd/user/` (user) or `/etc/systemd/system/` (system).
//! - **macOS → launchd.** A `com.allmystuff.daemon.plist` under
//!   `~/Library/LaunchAgents/` (user) or `/Library/LaunchDaemons/` (system).
//! - **Windows → the Service Control Manager.** An `AllMyStuff` service
//!   driven through `sc.exe`, running the node binary in service mode
//!   (`allmystuff-serve --service`, which speaks the SCM control protocol).
//!   Windows services are inherently system-wide (LocalSystem, start at
//!   boot), so the `--system`/user split collapses to one service there;
//!   managing it needs an elevated (Administrator) prompt.
//!
//! Almost everything here is a pure function — the unit/plist text, the
//! `sc.exe` argv vectors, the `systemctl`/`launchctl`/`sc` status parsers —
//! so all three backends are unit-tested on every CI runner regardless of
//! host OS. The unix path picks its init system in [`current_manager`]; the
//! Windows path is dispatched up front in [`run`].

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// systemd unit / launchd job names. Stable identifiers — changing them
/// orphans previously-installed services, so they're constants.
const SYSTEMD_UNIT: &str = "allmystuff.service";
const LAUNCHD_LABEL: &str = "com.allmystuff.daemon";

/// What `allmystuff service` can do. Parsed from argv by the CLI (no clap in
/// this crate), so the variants are plain data.
#[derive(Debug, Clone)]
pub enum ServiceCmd {
    /// Install the background service and start it. Also sets it to start on
    /// its own — at login (user service) or at boot (`--system`). `log` bakes
    /// an `ALLMYSTUFF_LOG` filter into the unit.
    Install { log: Option<String> },
    /// Start the installed service now.
    Start,
    /// Stop the running service. It stays installed and will start again on
    /// the next login/boot.
    Stop,
    /// Restart the service.
    Restart,
    /// Show whether the service is installed, enabled, and running.
    Status,
    /// Stop, disable, and remove the service.
    Uninstall,
}

/// Per-user vs system-wide install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    User,
    System,
}

impl Scope {
    fn from_flag(system: bool) -> Self {
        if system {
            Scope::System
        } else {
            Scope::User
        }
    }

    fn label(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }

    /// The `--system` token to suggest in messages (empty for user scope), so
    /// copy-pasteable hints address the right scope.
    fn flag_hint(self) -> &'static str {
        match self {
            Scope::User => "",
            Scope::System => " --system",
        }
    }

    fn other(self) -> Self {
        match self {
            Scope::User => Scope::System,
            Scope::System => Scope::User,
        }
    }
}

/// Which init system this build drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Manager {
    Systemd,
    Launchd,
}

/// Resolved enabled/active words for a status read-out. `None` means
/// "couldn't determine" (e.g. the probe tool returned nothing).
struct ServiceState {
    enabled: Option<String>,
    active: Option<String>,
}

/// Start/stop/restart, sharing one code path.
#[derive(Clone, Copy)]
enum Lifecycle {
    Start,
    Stop,
    Restart,
}

impl Lifecycle {
    fn verb(self) -> &'static str {
        match self {
            Lifecycle::Start => "start",
            Lifecycle::Stop => "stop",
            Lifecycle::Restart => "restart",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Lifecycle::Start => "Started",
            Lifecycle::Stop => "Stopped",
            Lifecycle::Restart => "Restarted",
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(system: bool, cmd: ServiceCmd) -> Result<()> {
    // Windows doesn't have an init system we drive with unit files; it has the
    // Service Control Manager, reached through `sc.exe`. Handle it up front so
    // the rest of this function stays the unix (systemd/launchd) story.
    if cfg!(windows) {
        return win_run(cmd);
    }

    let manager = current_manager()?;
    let scope = Scope::from_flag(system);

    // Fail clearly on a box whose init system we don't speak (e.g. a
    // container or a non-systemd Linux) rather than emitting confusing
    // "command not found" errors mid-operation.
    if !on_path(manager.tool()) {
        bail!(
            "`{tool}` was not found on your PATH.\n\n\
             `allmystuff service` manages {init} services, which this system \
             doesn't appear to use.\nRun the node under your own init system \
             instead, pointing it at:\n  allmystuff serve",
            tool = manager.tool(),
            init = manager.init_name(),
        );
    }

    let home = home_dir()?;
    match cmd {
        ServiceCmd::Install { log } => install(manager, scope, &home, log),
        ServiceCmd::Start => lifecycle(manager, scope, &home, Lifecycle::Start),
        ServiceCmd::Stop => lifecycle(manager, scope, &home, Lifecycle::Stop),
        ServiceCmd::Restart => lifecycle(manager, scope, &home, Lifecycle::Restart),
        ServiceCmd::Status => status(manager, scope, &home),
        ServiceCmd::Uninstall => uninstall(manager, scope, &home),
    }
}

/// Pick the unix init system for the host OS. Windows is handled before this
/// is ever called (see [`run`]), so it only has to choose between systemd and
/// launchd — and refuse the rare third unix that's neither.
fn current_manager() -> Result<Manager> {
    #[cfg(target_os = "linux")]
    {
        Ok(Manager::Systemd)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Manager::Launchd)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(anyhow!(
            "`allmystuff service` supports Linux (systemd), macOS (launchd), and \
             Windows (Service Control Manager).\nOn this platform, run the node \
             under your own init system, pointing it at: allmystuff serve"
        ))
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

fn install(manager: Manager, scope: Scope, home: &Path, log: Option<String>) -> Result<()> {
    ensure_privilege(scope, "install")?;

    // The unit runs the node binary (`allmystuff-serve`), not this CLI —
    // resolve it the same way `allmystuff serve` does.
    let src = find_serve_binary()
        .ok_or_else(|| {
            anyhow!(
                "couldn't find the `allmystuff-serve` node binary to run.\n\n\
                 Re-run the installer (it installs the node), set ALLMYSTUFF_SERVE_BIN,\n\
                 or build it from a source checkout:\n  \
                 cargo build --release --manifest-path node/Cargo.toml"
            )
        })?
        .canonicalize()
        .context("canonicalize the allmystuff-serve path")?;
    // A system service runs as a different (or transient) user that can't
    // reach a binary under someone's home dir, so copy it to a shared
    // location in that case.
    let (exec, copied) = stage_executable(scope, &src)?;

    let (env, state_dir) = compute_env(manager, scope, home, log);

    let unit_path = manager.unit_path(scope, home);
    let replacing = unit_path.exists();
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let contents = manager.render(&exec, scope, &env, home);
    write_unit(&unit_path, &contents)?;

    // launchd writes the node's stdout/stderr to a log file; make sure its
    // directory exists so launchd doesn't refuse to start the job.
    if manager == Manager::Launchd {
        if let Some(parent) = manager.launchd_log_path(scope, home).parent() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    for cmd in manager.install_cmds(scope, &unit_path) {
        run_checked(&cmd)?;
    }

    // A user systemd service dies with your session unless lingering is on —
    // fatal for a background node on a box you SSH into. Best effort.
    if manager == Manager::Systemd && scope == Scope::User {
        try_enable_linger();
    }

    println!(
        "{} AllMyStuff as a {} service.",
        if replacing {
            "Reinstalled"
        } else {
            "Installed"
        },
        scope.label()
    );
    if let Some(dest) = &copied {
        println!(
            "  binary:  {} (copied so the service account can execute it)",
            dest.display()
        );
    }
    println!("  unit:    {}", unit_path.display());
    println!("  state:   {}", state_dir.display());
    println!("  daemon:  the node spawns `myownmesh serve` itself — one service runs both");
    print_state(manager, scope, home);
    Ok(())
}

/// Return the path to bake into the service and, if we copied the binary,
/// where to. User scope runs as the invoking user, so the current path is
/// fine. System scope needs a path readable by the service account; if the
/// binary already lives in a system prefix we use it, otherwise we copy it
/// into `/usr/local/lib/allmystuff/`.
fn stage_executable(scope: Scope, src: &Path) -> Result<(PathBuf, Option<PathBuf>)> {
    if scope == Scope::User || is_system_path(src) {
        return Ok((src.to_path_buf(), None));
    }
    let dir = PathBuf::from("/usr/local/lib/allmystuff");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let dest = dir.join("allmystuff-serve");
    std::fs::copy(src, &dest)
        .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("make {} executable", dest.display()))?;
    }
    Ok((dest.clone(), Some(dest)))
}

/// Environment baked into the unit, plus the state directory (for display).
///
/// - **System scope** pins a fixed state dir (a transient/root user has no
///   usable `$HOME`) for both the node and the `myownmesh` daemon it spawns
///   (`MYOWNMESH_HOME`/`ALLMYSTUFF_HOME`), and disables self-update, since the
///   in-process updater can't rewrite a root-owned binary.
/// - **User scope** inherits the defaults: it relies on `$HOME` unless the
///   caller already runs with a custom home, which we carry over so the
///   service uses the same state they do.
///
/// `ALLMYSTUFF_LOG` is only set when `--log` is given; otherwise the node
/// applies its own default filter.
fn compute_env(
    _manager: Manager,
    scope: Scope,
    home: &Path,
    log: Option<String>,
) -> (Vec<(String, String)>, PathBuf) {
    let mut env = Vec::new();
    let state_dir = match scope {
        Scope::System => {
            let dir = _manager.system_state_dir();
            let d = dir.to_string_lossy().into_owned();
            // Both the node's mesh state and the spawned daemon's state live
            // here; pinning MYOWNMESH_HOME makes the two agree on the control
            // socket and roster.
            env.push(("MYOWNMESH_HOME".into(), d.clone()));
            env.push(("ALLMYSTUFF_HOME".into(), d.clone()));
            env.push(("MYOWNMESH_AUTOUPDATE".into(), "0".into()));
            env.push(("ALLMYSTUFF_AUTOUPDATE".into(), "0".into()));
            dir
        }
        Scope::User => {
            if let Some(custom) = env_var_nonempty("MYOWNMESH_HOME") {
                env.push(("MYOWNMESH_HOME".into(), custom));
            }
            if let Some(custom) = env_var_nonempty("ALLMYSTUFF_HOME") {
                env.push(("ALLMYSTUFF_HOME".into(), custom));
            }
            env_var_nonempty("MYOWNMESH_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".myownmesh"))
        }
    };
    if let Some(filter) = log {
        env.push(("ALLMYSTUFF_LOG".into(), filter));
    }
    (env, state_dir)
}

// ---------------------------------------------------------------------------
// start / stop / restart
// ---------------------------------------------------------------------------

fn lifecycle(manager: Manager, scope: Scope, home: &Path, life: Lifecycle) -> Result<()> {
    ensure_privilege(scope, life.verb())?;
    let unit_path = manager.unit_path(scope, home);
    if !unit_path.exists() {
        bail!(
            "the {} service isn't installed.\nRun `allmystuff service{} install` first.",
            scope.label(),
            scope.flag_hint()
        );
    }
    for cmd in manager.lifecycle_cmds(scope, life) {
        run_checked(&cmd)?;
    }
    println!("{} the {} service.", life.past(), scope.label());
    print_state(manager, scope, home);
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status(manager: Manager, scope: Scope, home: &Path) -> Result<()> {
    let unit_path = manager.unit_path(scope, home);
    println!("AllMyStuff ({} service)", scope.label());

    if !unit_path.exists() {
        println!("  status:  not installed");
        if manager.unit_path(scope.other(), home).exists() {
            println!(
                "  note:    a {} service is installed; query it with \
                 `allmystuff service{} status`",
                scope.other().label(),
                scope.other().flag_hint()
            );
        }
        println!("  install: allmystuff service{} install", scope.flag_hint());
        return Ok(());
    }

    println!("  unit:    {}", unit_path.display());
    print_state(manager, scope, home);
    Ok(())
}

/// Shared status read-out used by install/start/stop/restart/status.
fn print_state(manager: Manager, scope: Scope, home: &Path) {
    let state = manager.probe_state(scope);
    if let Some(enabled) = state.enabled {
        println!("  enabled: {enabled}");
    }
    if let Some(active) = state.active {
        println!("  active:  {active}");
    }
    println!("  logs:    {}", manager.logs_hint(scope, home));
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

fn uninstall(manager: Manager, scope: Scope, home: &Path) -> Result<()> {
    ensure_privilege(scope, "uninstall")?;
    let unit_path = manager.unit_path(scope, home);
    if !unit_path.exists() {
        println!(
            "No {} service installed — nothing to remove.",
            scope.label()
        );
        return Ok(());
    }

    // Stop + disable before deleting the file. Best-effort.
    for cmd in manager.pre_uninstall_cmds(scope, &unit_path) {
        run_quiet(&cmd);
    }
    std::fs::remove_file(&unit_path).with_context(|| format!("remove {}", unit_path.display()))?;
    for cmd in manager.post_uninstall_cmds(scope) {
        run_quiet(&cmd);
    }

    // Drop the binary copy we made for a system install, if any.
    if scope == Scope::System {
        let copied = PathBuf::from("/usr/local/lib/allmystuff");
        if copied.exists() {
            std::fs::remove_dir_all(&copied).ok();
        }
    }

    println!("Uninstalled the {} service.", scope.label());
    Ok(())
}

// ---------------------------------------------------------------------------
// Manager backends — pure path / text / argv builders + status probes
// ---------------------------------------------------------------------------

impl Manager {
    fn tool(self) -> &'static str {
        match self {
            Manager::Systemd => "systemctl",
            Manager::Launchd => "launchctl",
        }
    }

    fn init_name(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd",
            Manager::Launchd => "launchd",
        }
    }

    /// Absolute path of the unit/plist file for a scope.
    fn unit_path(self, scope: Scope, home: &Path) -> PathBuf {
        match (self, scope) {
            (Manager::Systemd, Scope::User) => home.join(".config/systemd/user").join(SYSTEMD_UNIT),
            (Manager::Systemd, Scope::System) => {
                PathBuf::from("/etc/systemd/system").join(SYSTEMD_UNIT)
            }
            (Manager::Launchd, Scope::User) => home
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            (Manager::Launchd, Scope::System) => {
                PathBuf::from("/Library/LaunchDaemons").join(format!("{LAUNCHD_LABEL}.plist"))
            }
        }
    }

    /// Fixed state directory for a system-scope service.
    fn system_state_dir(self) -> PathBuf {
        match self {
            // Matches `StateDirectory=allmystuff` (relative to /var/lib).
            Manager::Systemd => PathBuf::from("/var/lib/allmystuff"),
            Manager::Launchd => PathBuf::from("/Library/Application Support/AllMyStuff"),
        }
    }

    /// Where launchd should write the node's stdout/stderr.
    fn launchd_log_path(self, scope: Scope, home: &Path) -> PathBuf {
        match scope {
            Scope::User => home.join("Library/Logs/allmystuff.log"),
            Scope::System => PathBuf::from("/Library/Logs/allmystuff.log"),
        }
    }

    fn render(self, exec: &Path, scope: Scope, env: &[(String, String)], home: &Path) -> String {
        match self {
            Manager::Systemd => render_systemd_unit(exec, scope, env),
            Manager::Launchd => {
                render_launchd_plist(exec, env, &self.launchd_log_path(scope, home))
            }
        }
    }

    /// Commands to run after writing the unit: reload + enable + start.
    fn install_cmds(self, scope: Scope, unit_path: &Path) -> Vec<Vec<String>> {
        match self {
            Manager::Systemd => vec![
                systemctl(scope, &["daemon-reload"]),
                systemctl(scope, &["enable", "--now", SYSTEMD_UNIT]),
            ],
            Manager::Launchd => vec![launchctl(&["load", "-w", &path_arg(unit_path)])],
        }
    }

    fn lifecycle_cmds(self, scope: Scope, life: Lifecycle) -> Vec<Vec<String>> {
        match (self, life) {
            (Manager::Systemd, Lifecycle::Start) => {
                vec![systemctl(scope, &["start", SYSTEMD_UNIT])]
            }
            (Manager::Systemd, Lifecycle::Stop) => vec![systemctl(scope, &["stop", SYSTEMD_UNIT])],
            (Manager::Systemd, Lifecycle::Restart) => {
                vec![systemctl(scope, &["restart", SYSTEMD_UNIT])]
            }
            (Manager::Launchd, Lifecycle::Start) => vec![launchctl(&["start", LAUNCHD_LABEL])],
            (Manager::Launchd, Lifecycle::Stop) => vec![launchctl(&["stop", LAUNCHD_LABEL])],
            // launchd has no atomic restart for a loaded job; stop then start.
            (Manager::Launchd, Lifecycle::Restart) => vec![
                launchctl(&["stop", LAUNCHD_LABEL]),
                launchctl(&["start", LAUNCHD_LABEL]),
            ],
        }
    }

    fn pre_uninstall_cmds(self, scope: Scope, unit_path: &Path) -> Vec<Vec<String>> {
        match self {
            Manager::Systemd => vec![systemctl(scope, &["disable", "--now", SYSTEMD_UNIT])],
            Manager::Launchd => vec![launchctl(&["unload", "-w", &path_arg(unit_path)])],
        }
    }

    fn post_uninstall_cmds(self, scope: Scope) -> Vec<Vec<String>> {
        match self {
            Manager::Systemd => vec![systemctl(scope, &["daemon-reload"])],
            Manager::Launchd => vec![],
        }
    }

    fn logs_hint(self, scope: Scope, home: &Path) -> String {
        match self {
            Manager::Systemd => match scope {
                Scope::User => "journalctl --user -u allmystuff -f".to_string(),
                Scope::System => "journalctl -u allmystuff -f".to_string(),
            },
            Manager::Launchd => format!("tail -f {}", self.launchd_log_path(scope, home).display()),
        }
    }

    /// Query the live enabled/active state.
    fn probe_state(self, scope: Scope) -> ServiceState {
        match self {
            Manager::Systemd => {
                let (_, enabled_out, _) = capture(&systemctl(scope, &["is-enabled", SYSTEMD_UNIT]));
                let (_, active_out, _) = capture(&systemctl(scope, &["is-active", SYSTEMD_UNIT]));
                ServiceState {
                    enabled: parse_systemctl_word(&enabled_out),
                    active: parse_systemctl_word(&active_out),
                }
            }
            Manager::Launchd => {
                let (code, out, _) = capture(&launchctl(&["list", LAUNCHD_LABEL]));
                let (loaded, running) = parse_launchctl_list(code, &out);
                ServiceState {
                    enabled: Some(if loaded { "loaded" } else { "not loaded" }.to_string()),
                    active: Some(if running { "running" } else { "stopped" }.to_string()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit / plist rendering (pure)
// ---------------------------------------------------------------------------

fn render_systemd_unit(exec: &Path, scope: Scope, env: &[(String, String)]) -> String {
    let system = scope == Scope::System;
    let mut s = String::new();

    s.push_str("[Unit]\n");
    s.push_str("Description=AllMyStuff mesh node (serves this machine over the mesh)\n");
    s.push_str("Documentation=https://github.com/mrjeeves/AllMyStuff\n");
    if system {
        s.push_str("After=network-online.target\n");
        s.push_str("Wants=network-online.target\n");
    }

    s.push_str("\n[Service]\n");
    s.push_str("Type=simple\n");
    // The node binary *is* the whole behaviour — it spawns the myownmesh
    // daemon itself, so the unit needs no subcommand argument.
    s.push_str(&format!(
        "ExecStart={}\n",
        systemd_quote(&exec.to_string_lossy())
    ));
    s.push_str("Restart=on-failure\n");
    s.push_str("RestartSec=5\n");
    // The node handles SIGTERM for a clean shutdown (and kills the daemon it
    // spawned) — systemd's default stop signal, stated here for clarity.
    s.push_str("KillSignal=SIGTERM\n");
    s.push_str("TimeoutStopSec=20\n");

    if system {
        s.push('\n');
        s.push_str("# Run unprivileged under a systemd-managed transient user; StateDirectory\n");
        s.push_str("# gives it a stable, owned home at /var/lib/allmystuff across restarts.\n");
        s.push_str("DynamicUser=yes\n");
        s.push_str("StateDirectory=allmystuff\n");
    }

    for (key, value) in env {
        s.push_str(&format!("Environment={}\n", systemd_env(key, value)));
    }

    if system {
        s.push('\n');
        s.push_str("# Hardening\n");
        s.push_str("NoNewPrivileges=yes\n");
        s.push_str("ProtectSystem=strict\n");
        s.push_str("ProtectHome=yes\n");
        s.push_str("PrivateTmp=yes\n");
        s.push_str("ProtectKernelTunables=yes\n");
        s.push_str("ProtectControlGroups=yes\n");
        s.push_str("RestrictSUIDSGID=yes\n");
        s.push_str("RestrictRealtime=yes\n");
    }

    s.push_str("\n[Install]\n");
    s.push_str(if system {
        "WantedBy=multi-user.target\n"
    } else {
        "WantedBy=default.target\n"
    });
    s
}

/// Quote an `ExecStart` program path if it contains whitespace (systemd
/// splits unquoted command lines on spaces).
fn systemd_quote(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// Render a single `Environment=` assignment, quoting the whole `KEY=value`
/// when the value contains whitespace.
fn systemd_env(key: &str, value: &str) -> String {
    if value.contains(char::is_whitespace) {
        format!("\"{key}={value}\"")
    } else {
        format!("{key}={value}")
    }
}

fn render_launchd_plist(exec: &Path, env: &[(String, String)], log_path: &Path) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    s.push_str("<plist version=\"1.0\">\n<dict>\n");

    s.push_str("    <key>Label</key>\n");
    s.push_str(&format!("    <string>{LAUNCHD_LABEL}</string>\n"));

    s.push_str("    <key>ProgramArguments</key>\n    <array>\n");
    s.push_str(&format!(
        "        <string>{}</string>\n",
        xml_escape(&exec.to_string_lossy())
    ));
    s.push_str("    </array>\n");

    // Start at load/login/boot, and keep it alive — but not after a clean
    // (SIGTERM) shutdown, so `stop` actually stops it.
    s.push_str("    <key>RunAtLoad</key>\n    <true/>\n");
    s.push_str("    <key>KeepAlive</key>\n    <dict>\n");
    s.push_str("        <key>SuccessfulExit</key>\n        <false/>\n");
    s.push_str("    </dict>\n");

    let log = xml_escape(&log_path.to_string_lossy());
    s.push_str("    <key>StandardOutPath</key>\n");
    s.push_str(&format!("    <string>{log}</string>\n"));
    s.push_str("    <key>StandardErrorPath</key>\n");
    s.push_str(&format!("    <string>{log}</string>\n"));

    if !env.is_empty() {
        s.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (key, value) in env {
            s.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            ));
        }
        s.push_str("    </dict>\n");
    }

    s.push_str("</dict>\n</plist>\n");
    s
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// argv builders (pure)
// ---------------------------------------------------------------------------

/// `systemctl [--user] <args...>`.
fn systemctl(scope: Scope, args: &[&str]) -> Vec<String> {
    let mut cmd = vec!["systemctl".to_string()];
    if scope == Scope::User {
        cmd.push("--user".to_string());
    }
    cmd.extend(args.iter().map(|a| a.to_string()));
    cmd
}

/// `launchctl <args...>`.
fn launchctl(args: &[&str]) -> Vec<String> {
    let mut cmd = vec!["launchctl".to_string()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    cmd
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// status parsers (pure)
// ---------------------------------------------------------------------------

/// `systemctl is-enabled`/`is-active` print the state word on stdout even
/// when they exit non-zero. Empty stdout means the probe couldn't run.
fn parse_systemctl_word(stdout: &str) -> Option<String> {
    let word = stdout.lines().next().unwrap_or("").trim();
    if word.is_empty() {
        None
    } else {
        Some(word.to_string())
    }
}

/// `launchctl list <label>` exits 0 with a dict when the job is loaded, and
/// the dict carries a `"PID" = N;` line only while it's running.
fn parse_launchctl_list(exit_code: i32, stdout: &str) -> (bool, bool) {
    let loaded = exit_code == 0;
    let running = loaded
        && stdout
            .lines()
            .any(|line| line.trim_start().starts_with("\"PID\""));
    (loaded, running)
}

// ---------------------------------------------------------------------------
// process + filesystem helpers
// ---------------------------------------------------------------------------

/// Build the `Command` for one of our argv vectors, suppressing the console
/// window Windows would otherwise flash for each spawn.
///
/// The desktop app drives this crate **in-process** — the "Always On" tab
/// reads service status by calling straight into [`status_value`], which shells
/// out to `sc.exe`. AllMyStuff is a windowed GUI with no console of its own, so
/// without `CREATE_NO_WINDOW` every status read pops a stack of `sc.exe`
/// console windows for a frame (the reported flicker). This mirrors the flag
/// the inventory / terminal / node crates already set on their Windows spawns.
/// A no-op off Windows — `systemctl` / `launchctl` never open a window.
fn command(argv: &[String]) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Run a command, surfacing its stdout/stderr, and fail if it does.
fn run_checked(argv: &[String]) -> Result<()> {
    let status = command(argv).status().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            anyhow!(
                "`{}` not found — is it installed and on your PATH?",
                argv[0]
            )
        } else {
            anyhow!("failed to run `{}`: {e}", argv.join(" "))
        }
    })?;
    if !status.success() {
        bail!(
            "`{}` failed (exit {})",
            argv.join(" "),
            status_code(&status)
        );
    }
    Ok(())
}

/// Run a command, ignoring failure and output. For teardown steps that are
/// fine to be no-ops (already stopped/unloaded).
fn run_quiet(argv: &[String]) {
    let _ = command(argv).output();
}

/// Run a command and capture (exit code, stdout, stderr).
fn capture(argv: &[String]) -> (i32, String, String) {
    match command(argv).output() {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(_) => (-1, String::new(), String::new()),
    }
}

fn status_code(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

fn write_unit(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|e| {
        if e.kind() == ErrorKind::PermissionDenied {
            anyhow!(
                "permission denied writing {} — re-run with sudo for a --system service",
                path.display()
            )
        } else {
            anyhow!("write {}: {e}", path.display())
        }
    })
}

/// Best-effort `loginctl enable-linger <user>` so a user service keeps
/// running while logged out. Reports either way; never fatal.
fn try_enable_linger() {
    let Some(user) = current_username() else {
        println!(
            "  note:    run `sudo loginctl enable-linger <you>` to keep it \
             running while logged out"
        );
        return;
    };
    let (code, _, _) = capture(&[
        "loginctl".to_string(),
        "enable-linger".to_string(),
        user.clone(),
    ]);
    if code == 0 {
        println!("  linger:  enabled (keeps running while you're logged out)");
    } else {
        println!(
            "  note:    run `sudo loginctl enable-linger {user}` to keep it \
             running while logged out"
        );
    }
}

fn current_username() -> Option<String> {
    if let Some(user) = env_var_nonempty("USER").or_else(|| env_var_nonempty("LOGNAME")) {
        return Some(user);
    }
    let (code, out, _) = capture(&["id".to_string(), "-un".to_string()]);
    if code == 0 {
        let name = out.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// A system-scope operation must be root; fail fast with a copy-pasteable
/// sudo line rather than partway through.
fn ensure_privilege(scope: Scope, verb: &str) -> Result<()> {
    match privilege_hint(scope, is_root(), verb) {
        Some(msg) => Err(anyhow!(msg)),
        None => Ok(()),
    }
}

/// Pure decision behind [`ensure_privilege`]. Split out so the branch is
/// unit-testable without a real euid.
fn privilege_hint(scope: Scope, is_root: bool, verb: &str) -> Option<String> {
    if scope == Scope::System && !is_root {
        Some(format!(
            "managing the system service requires root.\n\nRe-run with sudo:\n  \
             sudo allmystuff service --system {verb}"
        ))
    } else {
        None
    }
}

/// Effective-uid 0? Asked via `id -u` so this crate stays `unsafe`-free
/// (it forbids `unsafe_code`), instead of calling `geteuid` directly.
fn is_root() -> bool {
    if !cfg!(unix) {
        return false;
    }
    let (code, out, _) = capture(&["id".to_string(), "-u".to_string()]);
    code == 0 && out.trim() == "0"
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not resolve your home directory")
}

fn env_var_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// True when `exe` lives in a system prefix readable by other accounts.
fn is_system_path(exe: &Path) -> bool {
    let path = exe.to_string_lossy();
    ["/usr/", "/opt/", "/bin/", "/sbin/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Whether an executable is reachable on PATH.
fn on_path(exe: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(exe).exists())
}

// ---------------------------------------------------------------------------
// Windows backend — the Service Control Manager, driven through `sc.exe`
// ---------------------------------------------------------------------------
//
// Unlike systemd/launchd there's no unit file: the service *is* an SCM record,
// created/queried/deleted with `sc.exe`. The node binary carries its own SCM
// dispatcher (`allmystuff-serve --service`), so the only thing baked into the
// service is its command line. Windows services run as LocalSystem and start
// at boot, so the user/system scope split doesn't apply — there's one service.
//
// As elsewhere, the argv builders and the `sc` output parsers are pure and
// tested on every runner; only the orchestration shells out (and only ever
// runs on Windows).

/// The SCM service name. **Must** match what the node binary passes to the
/// service dispatcher and control handler (`allmystuff-node`'s `serve` bin),
/// or the service can't report its status to the SCM. Stable identifier.
const WINDOWS_SERVICE_NAME: &str = "AllMyStuff";
const WINDOWS_DISPLAY_NAME: &str = "AllMyStuff Mesh Node";
const WINDOWS_DESCRIPTION: &str =
    "Runs this machine on the MyOwnMesh network (presence, plus screen / camera \
     / audio / input / terminal / files routes), spawning and supervising the \
     myownmesh daemon itself.";

/// Entry point for the Windows path (mirrors the unix [`run`] match). Windows
/// services are system-wide, so there's no scope argument.
fn win_run(cmd: ServiceCmd) -> Result<()> {
    match cmd {
        ServiceCmd::Install { log } => win_install(log),
        ServiceCmd::Start => win_lifecycle(Lifecycle::Start),
        ServiceCmd::Stop => win_lifecycle(Lifecycle::Stop),
        ServiceCmd::Restart => win_lifecycle(Lifecycle::Restart),
        ServiceCmd::Status => win_status(),
        ServiceCmd::Uninstall => win_uninstall(),
    }
}

fn win_install(log: Option<String>) -> Result<()> {
    // The service runs the node binary in service mode: resolve its source the
    // same way `allmystuff serve` does, then stage a protected LocalSystem copy
    // below ProgramData together with the daemon it supervises.
    let exec = find_serve_binary().ok_or_else(|| {
        anyhow!(
            "couldn't find the `allmystuff-serve` node binary to run.\n\n\
             Re-run the installer (it installs the node), set ALLMYSTUFF_SERVE_BIN,\n\
             or build it from a source checkout:\n  \
             cargo build --release --manifest-path node/Cargo.toml"
        )
    })?;
    // Make it absolute, but *don't* canonicalize: Windows canonicalization adds
    // the `\\?\` extended-length prefix, which would poison the service's
    // ImagePath. find_serve_binary already returns absolute paths in practice.
    let exec = win_absolute(&exec)?;

    // Resolve the mesh payload before touching an existing service.  The
    // desktop NSIS install does not necessarily leave `myownmesh.exe` beside
    // `allmystuff-serve.exe` (and an older self-update may have refreshed only
    // the four AllMyStuff executables).  Deleting a working service and only
    // then discovering that its daemon cannot be staged leaves the GUI saying
    // Always On is enabled while the replacement can never join a mesh.
    let mesh_source = find_mesh_binary_for_service(&exec).ok_or_else(|| {
        anyhow!(
            "couldn't find the `myownmesh` daemon required by the Always On service.\n\n\
             Re-run the AllMyStuff installer, install MyOwnMesh, or set \
             MYOWNMESH_BIN before enabling Always On."
        )
    })?;

    // Never point a LocalSystem service at the current-user install directory:
    // that account could replace the executable and gain SYSTEM on reboot.
    // Prepare both replacements beneath ProgramData *before* stopping an old
    // service. If either source is missing/unreadable, the existing host keeps
    // running instead of being deleted halfway through a repair.
    let service_dir = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("AllMyStuff")
        .join("service");
    std::fs::create_dir_all(&service_dir)
        .with_context(|| format!("create {}", service_dir.display()))?;
    let staged_exec = service_dir.join("allmystuff-serve.exe");
    let staged_mesh = service_dir.join("myownmesh.exe");
    let next_exec = service_dir.join("allmystuff-serve.next.exe");
    let next_mesh = service_dir.join("myownmesh.next.exe");
    std::fs::copy(&exec, &next_exec)
        .with_context(|| format!("prepare {} as {}", exec.display(), next_exec.display()))?;
    std::fs::copy(&mesh_source, &next_mesh).with_context(|| {
        format!(
            "prepare {} as {}",
            mesh_source.display(),
            next_mesh.display()
        )
    })?;
    win_harden_service_dir(&service_dir)?;

    // `sc create` refuses to clobber an existing service. Only after a complete
    // replacement payload is safely prepared do we stop and remove its record,
    // promote the files, and recreate it with the current arguments.
    let replacing = win_installed();
    if replacing {
        let _ = capture(&sc_stop());
        win_wait_stopped(Duration::from_secs(10));
        win_run_checked(&sc_delete())?;
        win_wait_absent(Duration::from_secs(5));
    }
    win_promote_payload(&next_exec, &staged_exec)?;
    win_promote_payload(&next_mesh, &staged_mesh)?;

    let profile_home = std::env::var_os("ALLMYSTUFF_SERVICE_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow!("couldn't resolve the installing user's profile directory"))?;
    let client_sid = std::env::var("ALLMYSTUFF_SERVICE_CLIENT_SID").ok();
    let binpath = win_binpath(
        &staged_exec,
        log.as_deref(),
        &profile_home,
        client_sid.as_deref(),
        Some(staged_mesh.as_path()),
    );
    win_run_checked(&sc_create(&binpath))?;
    // Description + automatic restart-on-failure are nice-to-haves: best-effort
    // so a quirky `sc` build can't fail the install over cosmetics. The
    // restart-on-failure also doubles as the node's self-update relaunch hook —
    // an updated node exits and the SCM brings the new one straight back.
    let _ = capture(&sc_description());
    let _ = capture(&sc_failure());
    win_run_checked(&sc_start())?;

    println!(
        "{} AllMyStuff as a Windows service (LocalSystem, starts at boot).",
        if replacing {
            "Reinstalled"
        } else {
            "Installed"
        }
    );
    println!("  binary:  {}", staged_exec.display());
    println!("  service: {WINDOWS_SERVICE_NAME}  (Service Control Manager)");
    println!("  daemon:  the node spawns `myownmesh serve` itself — one service runs both");
    println!("  manage:  services.msc, or `sc query {WINDOWS_SERVICE_NAME}`");
    win_print_state();
    Ok(())
}

fn win_promote_payload(prepared: &Path, live: &Path) -> Result<()> {
    match std::fs::remove_file(live) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("replace {}", live.display())),
    }
    std::fs::rename(prepared, live)
        .with_context(|| format!("promote {} as {}", prepared.display(), live.display()))
}

fn win_lifecycle(life: Lifecycle) -> Result<()> {
    if !win_installed() {
        bail!(
            "the Windows service isn't installed.\nRun `allmystuff service install` first \
             (from an elevated/Administrator prompt)."
        );
    }
    match life {
        // `sc start` on an already-running service exits 1056; `sc stop` on an
        // already-stopped one exits 1062. Treat those as success.
        Lifecycle::Start => win_run_tolerant(&sc_start(), &[1056])?,
        Lifecycle::Stop => win_run_tolerant(&sc_stop(), &[1062])?,
        Lifecycle::Restart => {
            let _ = capture(&sc_stop());
            win_wait_stopped(Duration::from_secs(10));
            win_run_tolerant(&sc_start(), &[1056])?;
        }
    }
    println!("{} the Windows service.", life.past());
    win_print_state();
    Ok(())
}

fn win_status() -> Result<()> {
    println!("AllMyStuff (Windows service)");
    if !win_installed() {
        println!("  status:  not installed");
        println!("  install: allmystuff service install  (run as Administrator)");
        return Ok(());
    }
    println!("  service: {WINDOWS_SERVICE_NAME}");
    win_print_state();
    Ok(())
}

fn win_uninstall() -> Result<()> {
    if !win_installed() {
        println!("No Windows service installed — nothing to remove.");
        return Ok(());
    }
    let _ = capture(&sc_stop());
    win_wait_stopped(Duration::from_secs(10));
    win_run_checked(&sc_delete())?;
    // The binary runs in place (the installer owns it), so there's nothing of
    // ours to delete — uninstall just removes the SCM record.
    println!("Uninstalled the Windows service.");
    Ok(())
}

/// Print the live enabled (auto-start) + running words for the SCM service.
fn win_print_state() {
    let (_, q_out, _) = capture(&sc_query());
    if let Some(state) = parse_sc_state(&q_out) {
        println!("  active:  {}", state.to_lowercase());
    }
    let (_, c_out, _) = capture(&sc_qc());
    if let Some(start) = parse_sc_start_type(&c_out) {
        let on = sc_autostart(&c_out);
        println!(
            "  enabled: {} ({})",
            if on { "enabled" } else { "disabled" },
            start.to_lowercase()
        );
    }
    println!("  logs:    Event Viewer, or run in a console: allmystuff serve");
}

/// Resolve `exe` to an absolute path *without* canonicalizing (which on
/// Windows would prepend the `\\?\` extended-length prefix that `sc`/CreateProcess
/// mishandle). Relative inputs are joined onto the current directory.
fn win_absolute(exe: &Path) -> Result<PathBuf> {
    if exe.is_absolute() {
        return Ok(exe.to_path_buf());
    }
    let cwd = std::env::current_dir().context("resolve current directory")?;
    Ok(cwd.join(exe))
}

/// The command line baked into the service. The exe is quoted so a spaced path
/// still parses, and `--service` flips the node into SCM-dispatcher mode; a
/// `--log` filter rides along when the installer was given one.
fn win_binpath(
    exec: &Path,
    log: Option<&str>,
    profile_home: &Path,
    client_sid: Option<&str>,
    mesh_bin: Option<&Path>,
) -> String {
    let mut s = format!(
        "\"{}\" --service --state-home {}",
        exec.display(),
        win_quote_arg(&profile_home.to_string_lossy())
    );
    if let Some(sid) = client_sid {
        s.push_str(" --client-sid ");
        s.push_str(&win_quote_arg(sid));
    }
    if let Some(mesh_bin) = mesh_bin {
        s.push_str(" --mesh-bin ");
        s.push_str(&win_quote_arg(&mesh_bin.to_string_lossy()));
    }
    if let Some(filter) = log {
        s.push_str(" --log ");
        s.push_str(&win_quote_arg(filter));
    }
    s
}

fn win_harden_service_dir(path: &Path) -> Result<()> {
    let path = path.to_string_lossy().into_owned();
    let argv = vec![
        "icacls".to_string(),
        path,
        "/inheritance:r".to_string(),
        "/grant:r".to_string(),
        "SYSTEM:(OI)(CI)F".to_string(),
        "Administrators:(OI)(CI)F".to_string(),
        "Users:(OI)(CI)RX".to_string(),
    ];
    win_run_checked(&argv)
}

fn win_quote_arg(value: &str) -> String {
    // These values are paths, SIDs and log filters; reject an embedded quote
    // rather than emitting an ambiguous SCM ImagePath command line.
    let value = value.replace('"', "");
    format!("\"{value}\"")
}

// ---- `sc.exe` argv builders (pure) ----------------------------------------

fn sc(args: &[&str]) -> Vec<String> {
    let mut cmd = vec!["sc".to_string()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    cmd
}

/// `sc create <name> binPath= "<...>" start= auto DisplayName= "<...>"`. Note
/// the SCM-mandated space after each `key=` — the value is its own token.
fn sc_create(binpath: &str) -> Vec<String> {
    sc(&[
        "create",
        WINDOWS_SERVICE_NAME,
        "binPath=",
        binpath,
        "start=",
        "auto",
        "DisplayName=",
        WINDOWS_DISPLAY_NAME,
    ])
}

fn sc_description() -> Vec<String> {
    sc(&["description", WINDOWS_SERVICE_NAME, WINDOWS_DESCRIPTION])
}

/// Restart the service on crash (mirrors systemd `Restart=on-failure`): three
/// 5 s-delayed restarts, counter reset after a day up.
fn sc_failure() -> Vec<String> {
    sc(&[
        "failure",
        WINDOWS_SERVICE_NAME,
        "reset=",
        "86400",
        "actions=",
        "restart/5000/restart/5000/restart/5000",
    ])
}

fn sc_start() -> Vec<String> {
    sc(&["start", WINDOWS_SERVICE_NAME])
}

fn sc_stop() -> Vec<String> {
    sc(&["stop", WINDOWS_SERVICE_NAME])
}

fn sc_delete() -> Vec<String> {
    sc(&["delete", WINDOWS_SERVICE_NAME])
}

fn sc_query() -> Vec<String> {
    sc(&["query", WINDOWS_SERVICE_NAME])
}

fn sc_qc() -> Vec<String> {
    sc(&["qc", WINDOWS_SERVICE_NAME])
}

// ---- `sc.exe` output parsers (pure) ---------------------------------------

/// The `STATE` word from `sc query` output (`RUNNING`, `STOPPED`,
/// `STOP_PENDING`, …). The state line reads `STATE : 4  RUNNING`, so the
/// trailing token is the word.
fn parse_sc_state(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find(|l| l.trim_start().starts_with("STATE"))
        .and_then(|l| l.split_whitespace().last())
        .map(str::to_string)
}

/// Whether `sc query` output reports the service as running.
fn sc_running(stdout: &str) -> bool {
    parse_sc_state(stdout).as_deref() == Some("RUNNING")
}

/// The `START_TYPE` word from `sc qc` output (`AUTO_START`, `DEMAND_START`, …).
fn parse_sc_start_type(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find(|l| l.trim_start().starts_with("START_TYPE"))
        .and_then(|l| l.split_whitespace().last())
        .map(str::to_string)
}

/// Whether `sc qc` output reports the service as starting automatically (at
/// boot).
fn sc_autostart(stdout: &str) -> bool {
    parse_sc_start_type(stdout)
        .map(|s| s.contains("AUTO_START"))
        .unwrap_or(false)
}

// ---- Windows process helpers ----------------------------------------------

/// `sc query <name>` exits 0 when the service exists (even stopped) and 1060
/// (`ERROR_SERVICE_DOES_NOT_EXIST`) when it doesn't. Querying status needs no
/// elevation, so the GUI and a plain shell can both read it.
fn win_installed() -> bool {
    capture(&sc_query()).0 == 0
}

/// Run an `sc` command, mapping the access-denied exit (5) to an actionable
/// "run elevated" message and surfacing any other failure with `sc`'s text.
fn win_run_checked(argv: &[String]) -> Result<()> {
    win_run_tolerant(argv, &[])
}

/// Like [`win_run_checked`] but treats the given extra exit codes as success
/// (e.g. "already running"/"already stopped").
fn win_run_tolerant(argv: &[String], tolerate: &[i32]) -> Result<()> {
    let (code, _out, err) = capture(argv);
    if code == 0 || tolerate.contains(&code) {
        return Ok(());
    }
    if code == 5 {
        bail!(
            "access denied running `{}`.\n\n\
             Managing a Windows service needs an elevated prompt. Right-click \
             Command Prompt or PowerShell and choose \"Run as administrator\", \
             then run the command again.",
            argv.join(" ")
        );
    }
    let detail = err.trim();
    if detail.is_empty() {
        bail!("`{}` failed (exit {code})", argv.join(" "));
    }
    bail!("`{}` failed (exit {code}): {detail}", argv.join(" "));
}

/// Poll `sc query` until the service reports `STOPPED` (or the deadline).
fn win_wait_stopped(timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let (_, out, _) = capture(&sc_query());
        if parse_sc_state(&out).as_deref() == Some("STOPPED") {
            return;
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Poll `sc query` until the service is gone (exit 1060) or the deadline.
fn win_wait_absent(timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !win_installed() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// Machine-readable status (the GUI's "Always On" tab reads this JSON)
// ---------------------------------------------------------------------------

/// Print the service status as a single JSON line (for `service status
/// --json`). Shape is stable and additive — see [`status_value`].
pub fn print_status_json(system: bool) -> Result<()> {
    let v = status_value(system)?;
    println!(
        "{}",
        serde_json::to_string(&v).unwrap_or_else(|_| "{}".into())
    );
    Ok(())
}

/// Structured service status: `{ platform, supported, manager, scope,
/// installed, enabled, running, needs_privilege, … }`. `enabled`/`running` are
/// booleans (null when not installed / indeterminate). On a platform we don't
/// support, `supported` is false and the rest is omitted.
pub fn status_value(system: bool) -> Result<Value> {
    if cfg!(windows) {
        return Ok(win_status_value());
    }
    let scope = Scope::from_flag(system);
    let manager = match current_manager() {
        Ok(m) => m,
        Err(_) => {
            return Ok(json!({
                "platform": std::env::consts::OS,
                "supported": false,
            }))
        }
    };
    let home = home_dir()?;
    let installed = manager.unit_path(scope, &home).exists();
    let state = if installed {
        manager.probe_state(scope)
    } else {
        ServiceState {
            enabled: None,
            active: None,
        }
    };
    Ok(json!({
        "platform": std::env::consts::OS,
        "supported": true,
        "manager": manager.init_name(),
        "scope": scope.label(),
        "installed": installed,
        "enabled": state.enabled.as_deref().map(enabled_is_on),
        "running": state.active.as_deref().map(active_is_running),
        "enabled_detail": state.enabled,
        "running_detail": state.active,
        "needs_privilege": scope == Scope::System,
    }))
}

fn win_status_value() -> Value {
    let installed = win_installed();
    let running = installed && {
        let (_, out, _) = capture(&sc_query());
        sc_running(&out)
    };
    let config = if installed {
        let (_, out, _) = capture(&sc_qc());
        Some(out)
    } else {
        None
    };
    let enabled = config.as_deref().map(sc_autostart);
    let service_dir = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("AllMyStuff")
        .join("service");
    let payload_version = win_binary_version(&service_dir.join("allmystuff-serve.exe"));
    let payload_current = payload_version
        .as_deref()
        .is_some_and(|version| version_at_least(version, env!("CARGO_PKG_VERSION")));
    // A responsive SCM parent is not enough: without the staged mesh daemon
    // the console-session node binds its local pipe but remains invisible to
    // every peer.  Older builds only checked `--state-home`, accepting exactly
    // that half-installed state as "current" and therefore never repairing it.
    let privileged_host_current = config.as_deref().is_some_and(|out| {
        win_privileged_host_config_current(out)
            && runnable(&service_dir.join("allmystuff-serve.exe"))
            && runnable(&service_dir.join("myownmesh.exe"))
    });
    json!({
        "platform": "windows",
        "supported": true,
        "manager": "windows-service",
        "scope": "system",
        "installed": installed,
        "enabled": enabled,
        "running": running,
        "privileged_host_current": privileged_host_current,
        "payload_version": payload_version,
        "payload_current": payload_current,
        "needs_privilege": true,
    })
}

fn win_binary_version(path: &Path) -> Option<String> {
    if !runnable(path) {
        return None;
    }
    let argv = vec![path.to_string_lossy().into_owned(), "--version".into()];
    let output = command(&argv).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .split_whitespace()
        .last()
        .map(|version| version.trim_start_matches('v').to_string())
}

fn version_at_least(have: &str, want: &str) -> bool {
    fn tuple(version: &str) -> Option<(u64, u64, u64)> {
        let core = version
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some((major, minor, patch))
    }

    matches!((tuple(have), tuple(want)), (Some(have), Some(want)) if have >= want)
}

fn win_privileged_host_config_current(config: &str) -> bool {
    config.contains("--state-home") && config.contains("--mesh-bin")
}

/// systemd reports `active`; launchd, `running`. Either means "up".
fn active_is_running(word: &str) -> bool {
    matches!(word, "active" | "running")
}

/// "Starts on its own" — systemd's enabled-ish states, or launchd's `loaded`.
fn enabled_is_on(word: &str) -> bool {
    matches!(
        word,
        "enabled" | "enabled-runtime" | "static" | "alias" | "indirect" | "loaded"
    )
}

// ---------------------------------------------------------------------------
// Locating the node binary the service runs
// ---------------------------------------------------------------------------

fn serve_exe_name() -> &'static str {
    if cfg!(windows) {
        "allmystuff-serve.exe"
    } else {
        "allmystuff-serve"
    }
}

/// Locate the `allmystuff-serve` node binary — the program the service runs.
/// Used by `allmystuff serve` (which execs it), `service install` (which bakes
/// its path into the unit/`sc` ImagePath), and the desktop app's in-process
/// installer.
///
/// Order: `ALLMYSTUFF_SERVE_BIN` → next to the running binary (the installer's
/// layout, and where the GUI bundles it as a sidecar) → `$PATH` → the
/// installer's well-known destinations → the dev workspace target. The
/// well-known destinations matter because a GUI launched from Finder/Dock or a
/// desktop launcher inherits a minimal `PATH` that excludes `/usr/local/bin`
/// and `~/.local/bin`, where the installer drops the node binary — so a
/// `PATH`-only search would miss it. Zero-byte files are skipped, so a failed
/// sidecar bundle (which stamps an empty stub) falls through rather than
/// "finding" an unrunnable binary.
pub fn find_serve_binary() -> Option<PathBuf> {
    let exe = serve_exe_name();

    if let Some(p) = std::env::var_os("ALLMYSTUFF_SERVE_BIN") {
        let p = PathBuf::from(p);
        if runnable(&p) {
            return Some(p);
        }
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(candidate) = current.parent().map(|dir| dir.join(exe)) {
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for dir in install_dirs() {
        let candidate = dir.join(exe);
        if runnable(&candidate) {
            return Some(candidate);
        }
    }
    for profile in ["release", "debug"] {
        if let Some(p) = workspace_serve_path(profile, exe) {
            if runnable(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// Locate the MyOwnMesh daemon that must be copied into the Windows Always On
/// payload. This mirrors the node runtime's discovery order, with one extra
/// hand-off variable used by the desktop process to carry its resolved binary
/// through the UAC boundary (an elevated process can inherit a different
/// PATH).
fn find_mesh_binary_for_service(serve: &Path) -> Option<PathBuf> {
    for key in ["ALLMYSTUFF_SERVICE_MESH_BIN", "MYOWNMESH_BIN"] {
        if let Some(path) = std::env::var_os(key).map(PathBuf::from) {
            if runnable(&path) {
                return Some(path);
            }
        }
    }

    let exe = if cfg!(windows) {
        "myownmesh.exe"
    } else {
        "myownmesh"
    };
    if let Some(candidate) = serve.parent().map(|dir| dir.join(exe)) {
        if runnable(&candidate) {
            return Some(candidate);
        }
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(candidate) = current.parent().map(|dir| dir.join(exe)) {
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }

    // GUI launches are not guaranteed to inherit the user's shell PATH.
    // Search both products' installer homes explicitly.
    #[cfg(windows)]
    let mut candidates = install_dirs();
    #[cfg(not(windows))]
    let candidates = install_dirs();
    #[cfg(windows)]
    {
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join("AppData")
                    .join("Local")
                    .join("Programs")
                    .join("MyOwnMesh"),
            );
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local).join("Programs").join("MyOwnMesh"));
        }
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join("MyOwnMesh"));
        }
    }
    candidates
        .into_iter()
        .map(|dir| dir.join(exe))
        .find(|candidate| runnable(candidate))
}

/// A binary candidate is usable only if it's a non-empty file — skips the
/// zero-byte placeholder the GUI sidecar bundling stamps when it can't stage
/// the real binary (so discovery falls through to PATH / the install dirs).
fn runnable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// The standard locations the AllMyStuff installer writes its binaries to (see
/// install.sh / install.ps1), searched when the node binary isn't beside the
/// running app or on `PATH`.
fn install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
        #[cfg(windows)]
        dirs.push(
            home.join("AppData")
                .join("Local")
                .join("Programs")
                .join("AllMyStuff"),
        );
    }
    #[cfg(windows)]
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(la).join("Programs").join("AllMyStuff"));
    }
    #[cfg(unix)]
    {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin")); // Apple-silicon Homebrew
        dirs.push(PathBuf::from("/usr/bin"));
    }
    dirs
}

fn workspace_serve_path(profile: &str, exe: &str) -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR = crates/allmystuff-service; repo root is two up, and
    // the node engine's build output lives under `node/target/<profile>/`.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Some(
        PathBuf::from(manifest_dir)
            .parent()? // crates/
            .parent()? // repo root
            .join("node")
            .join("target")
            .join(profile)
            .join(exe),
    )
}

// ---------------------------------------------------------------------------
// Locating the desktop-app binary (the GUI a bare `allmystuff` opens, and what
// `amst` boots when no node is running so the node has a visible GUI owner)
// ---------------------------------------------------------------------------

fn gui_exe_name() -> &'static str {
    if cfg!(windows) {
        "allmystuff-gui.exe"
    } else {
        "allmystuff-gui"
    }
}

/// Locate the `allmystuff-gui` desktop-app binary — the program a bare
/// `allmystuff` opens, and what `amst` boots when no node is running (so the
/// node it brings up has a visible GUI owner rather than running silently
/// headless). Shared by the CLI's bare-invocation launcher and `amst` so both
/// find the app the same way.
///
/// Order mirrors [`find_serve_binary`]: `ALLMYSTUFF_GUI_BIN` → next to the
/// running binary (the installer's layout) → `$PATH` → the installer's
/// well-known destinations → the dev workspace target
/// (`gui/src-tauri/target/{release,debug}`). The well-known destinations matter
/// for the same reason they do for the node binary: a process launched from a
/// desktop launcher inherits a minimal `PATH` that misses `/usr/local/bin` and
/// `~/.local/bin`. Zero-byte files are skipped (see [`runnable`]), so a failed
/// sidecar stub falls through instead of "finding" an unrunnable binary.
pub fn find_gui_binary() -> Option<PathBuf> {
    let exe = gui_exe_name();

    if let Some(p) = std::env::var_os("ALLMYSTUFF_GUI_BIN") {
        let p = PathBuf::from(p);
        if runnable(&p) {
            return Some(p);
        }
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(candidate) = current.parent().map(|dir| dir.join(exe)) {
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for dir in install_dirs() {
        let candidate = dir.join(exe);
        if runnable(&candidate) {
            return Some(candidate);
        }
    }
    for profile in ["release", "debug"] {
        if let Some(p) = workspace_gui_path(profile, exe) {
            if runnable(&p) {
                return Some(p);
            }
        }
    }
    None
}

fn workspace_gui_path(profile: &str, exe: &str) -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR = crates/allmystuff-service; repo root is two up, and
    // the Tauri app's build output lives under `gui/src-tauri/target/<profile>/`.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Some(
        PathBuf::from(manifest_dir)
            .parent()? // crates/
            .parent()? // repo root
            .join("gui")
            .join("src-tauri")
            .join("target")
            .join(profile)
            .join(exe),
    )
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn runnable_skips_empty_and_missing() {
        let dir = std::env::temp_dir().join(format!("ams-serve-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("stub");
        let real = dir.join("real");
        std::fs::write(&stub, b"").unwrap(); // the zero-byte sidecar placeholder
        std::fs::write(&real, b"\x7fELF...").unwrap();
        assert!(!runnable(&stub), "an empty stub must be skipped");
        assert!(runnable(&real), "a non-empty binary is usable");
        assert!(!runnable(&dir.join("nope")), "a missing path is skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- systemd unit rendering ----

    #[test]
    fn systemd_user_unit_is_minimal() {
        let unit = render_systemd_unit(
            Path::new("/home/u/.local/bin/allmystuff-serve"),
            Scope::User,
            &[],
        );
        assert!(unit.contains("ExecStart=/home/u/.local/bin/allmystuff-serve\n"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("KillSignal=SIGTERM"));
        // No subcommand argument — the node binary is the whole behaviour.
        assert!(!unit.contains("allmystuff-serve serve"));
        // User scope must not carry system-only directives.
        assert!(!unit.contains("DynamicUser"));
        assert!(!unit.contains("network-online.target"));
        assert!(!unit.contains("multi-user.target"));
    }

    #[test]
    fn systemd_system_unit_is_hardened() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/lib/allmystuff/allmystuff-serve"),
            Scope::System,
            &env(&[
                ("MYOWNMESH_HOME", "/var/lib/allmystuff"),
                ("ALLMYSTUFF_HOME", "/var/lib/allmystuff"),
                ("MYOWNMESH_AUTOUPDATE", "0"),
            ]),
        );
        assert!(unit.contains("DynamicUser=yes"));
        assert!(unit.contains("StateDirectory=allmystuff"));
        assert!(unit.contains("Environment=MYOWNMESH_HOME=/var/lib/allmystuff"));
        assert!(unit.contains("Environment=ALLMYSTUFF_HOME=/var/lib/allmystuff"));
        assert!(unit.contains("Environment=MYOWNMESH_AUTOUPDATE=0"));
        assert!(unit.contains("After=network-online.target"));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(unit.contains("ProtectSystem=strict"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn systemd_quotes_paths_and_env_with_spaces() {
        let unit = render_systemd_unit(
            Path::new("/opt/My Apps/allmystuff-serve"),
            Scope::User,
            &env(&[("MYOWNMESH_HOME", "/home/u/My Mesh")]),
        );
        assert!(unit.contains("ExecStart=\"/opt/My Apps/allmystuff-serve\"\n"));
        assert!(unit.contains("Environment=\"MYOWNMESH_HOME=/home/u/My Mesh\""));
    }

    #[test]
    fn systemd_bakes_the_log_filter() {
        let unit = render_systemd_unit(
            Path::new("/usr/local/lib/allmystuff/allmystuff-serve"),
            Scope::User,
            &env(&[("ALLMYSTUFF_LOG", "debug")]),
        );
        assert!(unit.contains("Environment=ALLMYSTUFF_LOG=debug"));
    }

    // ---- launchd plist rendering ----

    #[test]
    fn launchd_user_plist_has_no_env_block_when_empty() {
        let plist = render_launchd_plist(
            Path::new("/Users/u/.local/bin/allmystuff-serve"),
            &[],
            Path::new("/Users/u/Library/Logs/allmystuff.log"),
        );
        assert!(plist.contains("<string>com.allmystuff.daemon</string>"));
        assert!(plist.contains("<string>/Users/u/.local/bin/allmystuff-serve</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<string>/Users/u/Library/Logs/allmystuff.log</string>"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[test]
    fn launchd_system_plist_carries_env() {
        let plist = render_launchd_plist(
            Path::new("/usr/local/lib/allmystuff/allmystuff-serve"),
            &env(&[
                ("MYOWNMESH_HOME", "/Library/Application Support/AllMyStuff"),
                ("MYOWNMESH_AUTOUPDATE", "0"),
            ]),
            Path::new("/Library/Logs/allmystuff.log"),
        );
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>MYOWNMESH_HOME</key>"));
        assert!(plist.contains("<string>/Library/Application Support/AllMyStuff</string>"));
        assert!(plist.contains("<key>MYOWNMESH_AUTOUPDATE</key>"));
    }

    #[test]
    fn launchd_plist_escapes_xml() {
        let plist = render_launchd_plist(
            Path::new("/Users/a&b/allmystuff-serve"),
            &[],
            Path::new("/tmp/log"),
        );
        assert!(plist.contains("/Users/a&amp;b/allmystuff-serve"));
        assert!(!plist.contains("a&b/"));
    }

    #[test]
    fn xml_escape_covers_all_specials() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    // ---- path resolution ----

    #[test]
    fn unit_paths_per_scope() {
        let home = Path::new("/home/u");
        assert_eq!(
            Manager::Systemd.unit_path(Scope::User, home),
            Path::new("/home/u/.config/systemd/user/allmystuff.service")
        );
        assert_eq!(
            Manager::Systemd.unit_path(Scope::System, home),
            Path::new("/etc/systemd/system/allmystuff.service")
        );
        assert_eq!(
            Manager::Launchd.unit_path(Scope::User, home),
            Path::new("/home/u/Library/LaunchAgents/com.allmystuff.daemon.plist")
        );
        assert_eq!(
            Manager::Launchd.unit_path(Scope::System, home),
            Path::new("/Library/LaunchDaemons/com.allmystuff.daemon.plist")
        );
    }

    #[test]
    fn launchd_log_paths_per_scope() {
        let home = Path::new("/Users/u");
        assert_eq!(
            Manager::Launchd.launchd_log_path(Scope::User, home),
            Path::new("/Users/u/Library/Logs/allmystuff.log")
        );
        assert_eq!(
            Manager::Launchd.launchd_log_path(Scope::System, home),
            Path::new("/Library/Logs/allmystuff.log")
        );
    }

    #[test]
    fn is_system_path_classification() {
        assert!(is_system_path(Path::new(
            "/usr/local/lib/allmystuff/allmystuff-serve"
        )));
        assert!(is_system_path(Path::new(
            "/opt/homebrew/bin/allmystuff-serve"
        )));
        assert!(!is_system_path(Path::new(
            "/home/u/.local/bin/allmystuff-serve"
        )));
        assert!(!is_system_path(Path::new(
            "/home/u/AllMyStuff/node/target/release/allmystuff-serve"
        )));
    }

    // ---- argv builders ----

    #[test]
    fn systemctl_argv_threads_user_flag() {
        assert_eq!(
            systemctl(Scope::User, &["daemon-reload"]),
            vec!["systemctl", "--user", "daemon-reload"]
        );
        assert_eq!(
            systemctl(Scope::System, &["enable", "--now", SYSTEMD_UNIT]),
            vec!["systemctl", "enable", "--now", "allmystuff.service"]
        );
    }

    #[test]
    fn install_cmds_per_backend() {
        let path = Path::new("/etc/systemd/system/allmystuff.service");
        assert_eq!(
            Manager::Systemd.install_cmds(Scope::System, path),
            vec![
                vec!["systemctl", "daemon-reload"],
                vec!["systemctl", "enable", "--now", "allmystuff.service"],
            ]
        );
        let plist = Path::new("/Users/u/Library/LaunchAgents/com.allmystuff.daemon.plist");
        assert_eq!(
            Manager::Launchd.install_cmds(Scope::User, plist),
            vec![vec![
                "launchctl",
                "load",
                "-w",
                "/Users/u/Library/LaunchAgents/com.allmystuff.daemon.plist"
            ]]
        );
    }

    #[test]
    fn lifecycle_cmds_per_backend() {
        assert_eq!(
            Manager::Systemd.lifecycle_cmds(Scope::User, Lifecycle::Restart),
            vec![vec!["systemctl", "--user", "restart", "allmystuff.service"]]
        );
        assert_eq!(
            Manager::Launchd.lifecycle_cmds(Scope::User, Lifecycle::Restart),
            vec![
                vec!["launchctl", "stop", "com.allmystuff.daemon"],
                vec!["launchctl", "start", "com.allmystuff.daemon"],
            ]
        );
    }

    #[test]
    fn uninstall_cmds_per_backend() {
        let path = Path::new("/etc/systemd/system/allmystuff.service");
        assert_eq!(
            Manager::Systemd.pre_uninstall_cmds(Scope::System, path),
            vec![vec!["systemctl", "disable", "--now", "allmystuff.service"]]
        );
        assert_eq!(
            Manager::Systemd.post_uninstall_cmds(Scope::System),
            vec![vec!["systemctl", "daemon-reload"]]
        );
        let plist = Path::new("/Library/LaunchDaemons/com.allmystuff.daemon.plist");
        assert_eq!(
            Manager::Launchd.pre_uninstall_cmds(Scope::System, plist),
            vec![vec![
                "launchctl",
                "unload",
                "-w",
                "/Library/LaunchDaemons/com.allmystuff.daemon.plist"
            ]]
        );
        assert!(Manager::Launchd
            .post_uninstall_cmds(Scope::System)
            .is_empty());
    }

    // ---- status parsers ----

    #[test]
    fn parse_systemctl_word_takes_first_line() {
        assert_eq!(parse_systemctl_word("active\n"), Some("active".to_string()));
        assert_eq!(
            parse_systemctl_word("inactive"),
            Some("inactive".to_string())
        );
        assert_eq!(parse_systemctl_word(""), None);
        assert_eq!(parse_systemctl_word("\n"), None);
    }

    #[test]
    fn parse_launchctl_list_detects_loaded_and_running() {
        let running = "{\n\t\"PID\" = 4321;\n\t\"Label\" = \"com.allmystuff.daemon\";\n};\n";
        assert_eq!(parse_launchctl_list(0, running), (true, true));

        let loaded_idle = "{\n\t\"Label\" = \"com.allmystuff.daemon\";\n};\n";
        assert_eq!(parse_launchctl_list(0, loaded_idle), (true, false));

        assert_eq!(
            parse_launchctl_list(113, "Could not find service\n"),
            (false, false)
        );
    }

    // ---- scope helpers ----

    #[test]
    fn scope_helpers() {
        assert_eq!(Scope::from_flag(true), Scope::System);
        assert_eq!(Scope::from_flag(false), Scope::User);
        assert_eq!(Scope::System.flag_hint(), " --system");
        assert_eq!(Scope::User.flag_hint(), "");
        assert_eq!(Scope::User.other(), Scope::System);
    }

    #[test]
    fn privilege_hint_only_blocks_system_without_root() {
        let hint = privilege_hint(Scope::System, false, "start").expect("should block");
        assert!(hint.contains("sudo allmystuff service --system start"));
        assert!(privilege_hint(Scope::System, true, "start").is_none());
        assert!(privilege_hint(Scope::User, false, "install").is_none());
    }

    // ---- env policy ----

    #[test]
    fn system_env_pins_state_and_disables_autoupdate() {
        let (env, state) = compute_env(
            Manager::Systemd,
            Scope::System,
            Path::new("/root"),
            Some("debug".to_string()),
        );
        assert_eq!(state, Path::new("/var/lib/allmystuff"));
        assert!(env.contains(&(
            "MYOWNMESH_HOME".to_string(),
            "/var/lib/allmystuff".to_string()
        )));
        assert!(env.contains(&(
            "ALLMYSTUFF_HOME".to_string(),
            "/var/lib/allmystuff".to_string()
        )));
        assert!(env.contains(&("MYOWNMESH_AUTOUPDATE".to_string(), "0".to_string())));
        assert!(env.contains(&("ALLMYSTUFF_AUTOUPDATE".to_string(), "0".to_string())));
        assert!(env.contains(&("ALLMYSTUFF_LOG".to_string(), "debug".to_string())));
    }

    #[test]
    fn user_env_defaults_to_home_and_no_overrides() {
        let saved = (
            std::env::var("MYOWNMESH_HOME").ok(),
            std::env::var("ALLMYSTUFF_HOME").ok(),
        );
        std::env::remove_var("MYOWNMESH_HOME");
        std::env::remove_var("ALLMYSTUFF_HOME");

        let (env, state) = compute_env(Manager::Systemd, Scope::User, Path::new("/home/u"), None);
        assert_eq!(state, Path::new("/home/u/.myownmesh"));
        assert!(env.is_empty());

        if let Some(v) = saved.0 {
            std::env::set_var("MYOWNMESH_HOME", v);
        }
        if let Some(v) = saved.1 {
            std::env::set_var("ALLMYSTUFF_HOME", v);
        }
    }

    // ---- windows: binPath + sc argv builders ----

    #[test]
    fn win_binpath_quotes_exe_and_flags_service() {
        assert_eq!(
            win_binpath(
                Path::new("C:\\ProgramData\\AllMyStuff\\bin\\allmystuff-serve.exe"),
                None,
                Path::new("C:\\Users\\Chris Paul"),
                Some("S-1-5-21-1000"),
                Some(Path::new(
                    "C:\\ProgramData\\AllMyStuff\\service\\myownmesh.exe"
                ))
            ),
            "\"C:\\ProgramData\\AllMyStuff\\bin\\allmystuff-serve.exe\" --service \
             --state-home \"C:\\Users\\Chris Paul\" --client-sid \"S-1-5-21-1000\" \
             --mesh-bin \"C:\\ProgramData\\AllMyStuff\\service\\myownmesh.exe\""
        );
    }

    #[test]
    fn win_binpath_appends_log_filter() {
        assert_eq!(
            win_binpath(
                Path::new("C:\\PD\\AllMyStuff\\bin\\allmystuff-serve.exe"),
                Some("info,allmystuff_node=debug"),
                Path::new("C:\\Users\\u"),
                None,
                None
            ),
            "\"C:\\PD\\AllMyStuff\\bin\\allmystuff-serve.exe\" --service \
             --state-home \"C:\\Users\\u\" --log \"info,allmystuff_node=debug\""
        );
    }

    #[test]
    fn privileged_host_requires_state_and_mesh_payload_arguments() {
        let complete = r#"BINARY_PATH_NAME : "C:\ProgramData\AllMyStuff\service\allmystuff-serve.exe" --service --state-home "C:\Users\u" --mesh-bin "C:\ProgramData\AllMyStuff\service\myownmesh.exe""#;
        assert!(win_privileged_host_config_current(complete));

        // This was the broken migration shape: its local node could answer,
        // but the service had no deterministic daemon to put it on the mesh.
        let no_mesh = r#"BINARY_PATH_NAME : "C:\ProgramData\AllMyStuff\service\allmystuff-serve.exe" --service --state-home "C:\Users\u""#;
        assert!(!win_privileged_host_config_current(no_mesh));
        assert!(!win_privileged_host_config_current(
            r#"BINARY_PATH_NAME : "C:\old\allmystuff-serve.exe" --service"#
        ));
    }

    #[test]
    fn service_payload_version_comparison_accepts_current_or_newer() {
        assert!(version_at_least("0.2.67", "0.2.67"));
        assert!(version_at_least("v0.2.68", "0.2.67"));
        assert!(!version_at_least("0.2.61", "0.2.67"));
        assert!(!version_at_least("unknown", "0.2.67"));
    }

    #[test]
    fn sc_create_argv_has_spaced_keys_and_autostart() {
        let argv = sc_create("\"C:\\x\\allmystuff-serve.exe\" --service");
        assert_eq!(
            argv,
            vec![
                "sc",
                "create",
                "AllMyStuff",
                "binPath=",
                "\"C:\\x\\allmystuff-serve.exe\" --service",
                "start=",
                "auto",
                "DisplayName=",
                "AllMyStuff Mesh Node",
            ]
        );
    }

    #[test]
    fn sc_lifecycle_argv() {
        assert_eq!(sc_start(), vec!["sc", "start", "AllMyStuff"]);
        assert_eq!(sc_stop(), vec!["sc", "stop", "AllMyStuff"]);
        assert_eq!(sc_delete(), vec!["sc", "delete", "AllMyStuff"]);
        assert_eq!(sc_query(), vec!["sc", "query", "AllMyStuff"]);
        assert_eq!(sc_qc(), vec!["sc", "qc", "AllMyStuff"]);
    }

    // ---- windows: sc output parsers ----

    #[test]
    fn parse_sc_state_reads_running_word() {
        let out = "SERVICE_NAME: AllMyStuff\n        \
                   TYPE               : 10  WIN32_OWN_PROCESS\n        \
                   STATE              : 4  RUNNING\n        \
                   WIN32_EXIT_CODE    : 0  (0x0)\n";
        assert_eq!(parse_sc_state(out).as_deref(), Some("RUNNING"));
        assert!(sc_running(out));
    }

    #[test]
    fn parse_sc_state_reads_stopped_and_pending() {
        let stopped = "        STATE              : 1  STOPPED\n";
        assert_eq!(parse_sc_state(stopped).as_deref(), Some("STOPPED"));
        assert!(!sc_running(stopped));

        let pending = "        STATE              : 3  STOP_PENDING\n";
        assert_eq!(parse_sc_state(pending).as_deref(), Some("STOP_PENDING"));
    }

    #[test]
    fn parse_sc_start_type_reads_autostart() {
        let qc = "[SC] QueryServiceConfig SUCCESS\n\nSERVICE_NAME: AllMyStuff\n        \
                  TYPE               : 10  WIN32_OWN_PROCESS\n        \
                  START_TYPE         : 2   AUTO_START\n        \
                  ERROR_CONTROL      : 1   NORMAL\n";
        assert_eq!(parse_sc_start_type(qc).as_deref(), Some("AUTO_START"));
        assert!(sc_autostart(qc));

        let demand = "        START_TYPE         : 3   DEMAND_START\n";
        assert!(!sc_autostart(demand));
    }

    // ---- status word truthiness (drives the GUI's JSON) ----

    #[test]
    fn status_word_truthiness() {
        assert!(active_is_running("active")); // systemd
        assert!(active_is_running("running")); // launchd
        assert!(!active_is_running("inactive"));
        assert!(!active_is_running("stopped"));

        assert!(enabled_is_on("enabled")); // systemd
        assert!(enabled_is_on("loaded")); // launchd
        assert!(enabled_is_on("static"));
        assert!(!enabled_is_on("disabled"));
        assert!(!enabled_is_on("not loaded"));
    }
}
