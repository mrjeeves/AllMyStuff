//! `allmystuff-serve` — run this machine as a mesh node with **no GUI**.
//!
//! This is the headless half of AllMyStuff. The desktop app's Tauri backend
//! and this binary link the very same engine ([`allmystuff_node`]); the only
//! difference is what's wired to [`UiSink`] — the app feeds events to its
//! Svelte front-end, and here they just go to the log, because there's no
//! front-end to feed. Everything a peer can do *to* this machine still works:
//! a console connecting to watch its screen (**monitor out**), a fleet member
//! opening a terminal, a room asking for its camera or system audio.
//!
//! It is also self-contained as a unit: AllMyStuff is a client of a
//! `myownmesh serve` daemon, so this binary **spawns and supervises that
//! daemon itself** (reusing the same logic the GUI uses). One process brings
//! up both — which is what lets a single `allmystuff service` install run the
//! whole node on a headless box. Users normally reach it as `allmystuff
//! serve`, which execs this binary.
//!
//! ```text
//! allmystuff serve                       # run this machine on the mesh, headless
//! ALLMYSTUFF_CLAIMABLE=1 allmystuff serve # …and let one of your machines adopt it
//! ALLMYSTUFF_LOG=debug allmystuff serve   # …with verbose logs
//! allmystuff serve --log debug            # …same, as a flag
//! ```
//!
//! **Windows service mode.** `allmystuff service install` registers this binary
//! with the Service Control Manager as `<exe> --service`. That flag flips it
//! into [`winsvc`] mode: it answers the SCM's control protocol (a plain console
//! binary would be killed for not doing so) and logs to a file (no console to
//! print to). systemd and launchd need no such mode — they run the binary as
//! an ordinary foreground process and signal it with SIGTERM.
//!
//! **Unattended self-update.** Headless and as a service the node is meant to
//! be "always on, always current": its background updater doesn't just stage a
//! release for the next launch (a service box might not restart for months) —
//! it applies the update and relaunches onto it, on all three OSes. See
//! [`allmystuff_updater::tick_forever_unattended`].

use std::future::Future;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use allmystuff_node::control_client::ControlClient;
use allmystuff_node::daemon_spawn::{self, DaemonChild};
use allmystuff_node::mesh::Mesh;
use allmystuff_node::networks_store::DisabledNetworks;
use allmystuff_node::node_control::{self, SocketSink};
use allmystuff_node::UiSink;

/// Headless event sink. The engine's events (`allmystuff://session`,
/// `…/video-ready`, room/owned updates, …) are all front-end concerns, so a
/// node with no webview logs them at `trace` for debugging and otherwise
/// lets them fall on the floor.
struct LogSink {
    /// How to relaunch onto a just-applied update — the SAME OS-aware strategy
    /// the unattended updater uses (`pick_relaunch(as_service)`): a re-exec on
    /// a shell/desktop process, but an **exit-for-the-SCM** under a Windows
    /// service (a service can't re-exec itself — the SCM only tracks the
    /// original process, so a spawned child orphans and the service reads as a
    /// clean stop and never restarts). The remote "upgrade this machine" path
    /// went through a hardcoded re-exec and so left a Windows-service node dead;
    /// carrying the strategy here makes the remote upgrade relaunch exactly like
    /// the unattended one that already works on all three OSes.
    relaunch: fn() -> !,
}

impl UiSink for LogSink {
    fn emit(&self, event: &str, _payload: serde_json::Value) {
        tracing::trace!(event, "node event (headless: no UI listening)");
    }

    fn restart(&self) -> ! {
        // The fleet "upgrade this machine" path applied a new build and wants us
        // to run it. The GUI (if attached) relaunches its window off the
        // `NodeEvent::Restart` the SocketSink fanned out; here we bring the node
        // itself up onto the new binary via the OS-aware relaunch.
        (self.relaunch)()
    }
}

/// Replace this process with a fresh copy of itself, carrying the same args.
/// Returns only on failure (then exits), so the signature is `-> !`.
fn reexec_self() -> ! {
    match std::env::current_exe() {
        Ok(exe) => {
            let args: Vec<String> = std::env::args().skip(1).collect();
            tracing::info!("restarting onto the updated build: {}", exe.display());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt as _;
                // `exec` only returns if it failed to replace the image.
                let err = std::process::Command::new(&exe).args(&args).exec();
                tracing::error!("re-exec failed: {err}");
            }
            #[cfg(not(unix))]
            {
                // Windows cannot replace the current process image with
                // `execve`. Mark the child as our hand-off successor so it
                // waits for this process to release the one-node control
                // socket instead of stepping aside as an unrelated duplicate.
                if let Err(e) = std::process::Command::new(&exe)
                    .args(&args)
                    .env("ALLMYSTUFF_REEXEC_HANDOFF", "1")
                    .spawn()
                {
                    tracing::error!("couldn't relaunch: {e}");
                }
            }
        }
        Err(e) => tracing::error!("couldn't locate own executable to restart: {e}"),
    }
    std::process::exit(0);
}

/// Relaunch hook for the **Windows service** path. A service can't re-exec
/// itself the way a console process can — the SCM only tracks the original
/// process, so a spawned child would orphan and the SCM would think the
/// service died. Instead we exit non-zero: with the install's configured
/// restart-on-failure action, the SCM brings the service straight back up,
/// running the freshly-applied binary at the same ImagePath. The supervised
/// daemon dies with us (its kill-on-close job object) and the new process
/// respawns it.
#[cfg(windows)]
fn service_relaunch() -> ! {
    tracing::info!(
        "self-update applied; exiting so the Service Control Manager restarts the updated node"
    );
    std::process::exit(1);
}

/// Pick the relaunch the background updater uses when it applies a release.
fn pick_relaunch(as_service: bool) -> fn() -> ! {
    #[cfg(windows)]
    {
        if as_service {
            return service_relaunch;
        }
    }
    let _ = as_service; // unix services re-exec (execve keeps the PID)
    reexec_self
}

/// One-shot CLI verbs `allmystuff-serve` answers *before* it would bind the
/// control socket and become a node, keyed off the first argument:
///
///   * `--version` / `-V` — print `allmystuff-serve <version>`. A supervising
///     app (e.g. the CEC Support app checking a reused node against its pinned
///     AllMyStuff version) reads this the same way `daemon_spawn` reads
///     `myownmesh --version`.
///   * `update` — the self-updater: download the latest release and apply it in
///     place, the same one-shot the fleet/`myownmesh update` path uses (this is
///     what a below-pin `allmystuff-serve` is asked to run).
///
/// Returns `Some(code)` when a verb ran (main exits with it), `None` to carry on
/// and run the node. `--service` / `--log` are flags, not verbs, so they fall
/// through to the normal node path.
fn run_cli_verb() -> Option<ExitCode> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => {
            println!("allmystuff-serve {}", env!("CARGO_PKG_VERSION"));
            Some(ExitCode::SUCCESS)
        }
        Some("update") => Some(run_update_now()),
        _ => None,
    }
}

/// Run `allmystuff_updater::update_now()` on a throwaway current-thread runtime
/// and map the outcome to an exit code. Output goes to stdout so a supervising
/// parent can fold it into its own log (the way `daemon_spawn::run_daemon_update`
/// folds `myownmesh update`'s output).
fn run_update_now() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("allmystuff serve update: couldn't build a runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        match allmystuff_updater::update_now().await {
            Ok(outcome) => {
                println!("{}", render_update_now(&outcome));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("allmystuff serve update failed: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

/// A human line for an `update` outcome (also what the supervising parent logs).
fn render_update_now(outcome: &allmystuff_updater::UpdateNowOutcome) -> String {
    use allmystuff_updater::UpdateNowOutcome::*;
    match outcome {
        PackageManager => "installed by a package manager — update it through that".into(),
        UpToDate { current, latest } => {
            format!("already up to date (have {current}, latest {latest})")
        }
        Updated { to, components } => {
            format!(
                "updated to {to} ({}); restart to run it",
                components.join(", ")
            )
        }
    }
}

fn main() -> ExitCode {
    // One-shot CLI verbs (`--version`, `update`) run before we touch logging or
    // the control socket, and exit with their own code.
    if let Some(code) = run_cli_verb() {
        return code;
    }

    // Windows registers this binary as `<exe> --service`; that flag is what
    // tells us to speak the SCM control protocol instead of running in the
    // foreground. Off Windows there's no such mode.
    #[cfg(windows)]
    let as_service = std::env::args().skip(1).any(|a| a == "--service");
    #[cfg(not(windows))]
    let as_service = false;

    // Apply any update staged on a previous run before binding anything —
    // same "stage now, apply on next launch" model as the GUI and the daemon.
    allmystuff_updater::apply_pending_if_any();

    init_logging(as_service);

    // As a Windows service, hand off to the SCM dispatcher: it runs the node on
    // its own thread (see `winsvc::run_service`) and blocks until stopped.
    #[cfg(windows)]
    if as_service {
        return winsvc::dispatch();
    }

    // Foreground (a console, or a systemd/launchd-supervised process): run
    // until a stop signal arrives.
    run_blocking(as_service, wait_for_shutdown_signal())
}

/// Build the async runtime and run the node to completion, stopping when
/// `shutdown` resolves. Shared by the foreground path (a signal future) and
/// the Windows service path (an SCM-stop future).
fn run_blocking<F: Future<Output = ()>>(as_service: bool, shutdown: F) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("allmystuff serve: failed to build async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(as_service, shutdown))
}

async fn run<F: Future<Output = ()>>(as_service: bool, shutdown: F) -> ExitCode {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "allmystuff node starting"
    );

    // One node per machine. The node control socket is the guard: bind it
    // before bringing up any mesh. A *live* node already holding it means this
    // machine is already served (a running Always-On service, or the desktop
    // app's spawned node) — starting a second mesh would put two nodes under
    // one identity and then nothing connects, so step aside cleanly. Binding
    // before the mesh starts is also what makes two simultaneously-starting
    // nodes safe (see `bind_control_socket`).
    let shutdown = std::pin::pin!(shutdown);
    // A Windows re-exec has to spawn before the old process can exit. The
    // replacement can therefore reach this bind while its parent still owns
    // the socket. Only a child explicitly marked by `reexec_self` retries;
    // ordinary duplicate launches retain the immediate step-aside behavior.
    #[cfg(windows)]
    let mut handoff_retries = if std::env::var_os("ALLMYSTUFF_REEXEC_HANDOFF").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        100u8 // 5 seconds at 50 ms — generous for loaded Windows hosts.
    } else {
        0
    };
    #[cfg(not(windows))]
    let mut handoff_retries = 0u8;

    let control_listener = loop {
        match node_control::bind_control_socket().await {
            Ok(listener) => break listener,
            Err(_) if handoff_retries > 0 => {
                if handoff_retries == 100 {
                    tracing::debug!("waiting for the previous node to release its control socket");
                }
                handoff_retries -= 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                tracing::info!("not starting a second node ({e:#})");
                return ExitCode::SUCCESS;
            }
        }
    };

    let client = match ControlClient::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!("couldn't resolve the control socket path: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Bring up the mesh daemon this node rides on, and *keep* it up: the
    // supervisor task below polls the spawned child and respawns it (capped
    // backoff) if it dies — previously the handle was held but never
    // watched, so a crashed daemon left a permanently meshless node behind a
    // running service. An already-running daemon (someone else's, or the
    // GUI's) is reused and left untouched — but if the socket later goes
    // dead with nothing of ours to watch, the supervisor spawns our own.
    // Ownership stays with `main` (the mutex) so shutdown still drops the
    // child and one service unit really does run both.
    let daemon: Arc<std::sync::Mutex<Option<DaemonChild>>> = Arc::new(std::sync::Mutex::new(
        match daemon_spawn::ensure_daemon_running(&client).await {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!(
                    "couldn't start the myownmesh daemon ({e:#}); will keep trying / use one if \
                     it appears. Install it (so it's on PATH), set MYOWNMESH_BIN, or run \
                     `myownmesh serve` yourself."
                );
                None
            }
        },
    ));
    tokio::spawn(supervise_daemon(daemon.clone(), client.clone()));

    // Wire the engine to a sink that both logs (the headless `LogSink`) and
    // broadcasts every event to clients of the node control socket — the seam
    // a thin GUI drives the node over (Phase A). The broadcaster is shared
    // with the control server spawned below, and `DisabledNetworks` is the
    // park store the server's `network_set_enabled` command needs.
    let broadcaster = node_control::new_broadcaster();
    let (event_tx, event_rx) = node_control::event_channel();
    let disabled = Arc::new(DisabledNetworks::load());
    let sink = SocketSink::new(
        Arc::new(LogSink {
            relaunch: pick_relaunch(as_service),
        }),
        event_tx,
    );
    let mesh = Mesh::new(client.clone(), Arc::new(sink));
    // Share the park store with the mesh so a switched-off local claim
    // network stays off across ownership checks (it can't be left, so the
    // park store is its only off switch).
    mesh.attach_disabled_networks(disabled.clone());
    mesh.clone().start().await;

    // Serve the node control + event socket (on the listener bound up front) so
    // the desktop app drives this node over it instead of running its own mesh.
    tokio::spawn({
        let mesh = mesh.clone();
        let client = client.clone();
        let disabled = disabled.clone();
        async move {
            if let Err(e) = node_control::serve(
                control_listener,
                mesh,
                client,
                disabled,
                broadcaster,
                event_rx,
            )
            .await
            {
                tracing::warn!("node control socket stopped: {e:#}");
            }
        }
    });

    // Self-update ticker: a headless node checks the release feed on its own
    // and, unlike the desktop app's "stage + offer relaunch", *applies* what
    // its policy permits and relaunches onto it — so an always-on box that
    // never gets manually restarted still keeps every half (CLI/GUI/node)
    // current. No-ops when auto-update is off or this is a package-managed
    // install. Under a Windows service the relaunch hands back to the SCM.
    tokio::spawn(allmystuff_updater::tick_forever_unattended(pick_relaunch(
        as_service,
    )));

    match mesh.resolve_local_id().await {
        Some(id) => tracing::info!(device_id = %id, "serving this machine on the mesh"),
        None => tracing::warn!(
            "serving, but couldn't read this device's mesh identity yet — \
             is the daemon up? (the event pump will keep retrying)"
        ),
    }
    if allmystuff_node::ownership::Ownership::load().claimable() {
        tracing::info!(
            "claim mode is on (ALLMYSTUFF_CLAIMABLE) — one of your machines can adopt this one"
        );
    }
    tracing::info!("node is up — press Ctrl-C to stop");

    // Run until asked to stop. Holding `mesh` and `daemon` in scope keeps the
    // pump alive and the supervised daemon running for the node's whole life.
    shutdown.await;
    tracing::info!("shutdown requested — stopping");

    // Take the child out of the supervisor's hands and drop it here, killing
    // the daemon we spawned (if any) — the supervisor never respawns after
    // this because the process is exiting.
    drop(daemon.lock().ok().and_then(|mut d| d.take()));
    drop(mesh);
    ExitCode::SUCCESS
}

/// Keep the mesh daemon alive for the node's whole life. Two duties:
///
/// - **Our child died** → respawn it on a capped backoff (2s → 60s). The
///   engine's daemon-link loop reconnects the moment the socket answers, so
///   the node heals end to end with no restart.
/// - **Nothing of ours to watch** (we reused someone else's daemon, or never
///   managed to spawn one) → probe the control socket on a slow cadence and,
///   when it stops answering, try to bring up our own. This covers "the
///   GUI's daemon died after this service reused it" and "the binary
///   appeared on PATH after we started".
///
/// The child lives in `slot` (owned by `main`), so shutdown still kills it.
async fn supervise_daemon(
    slot: Arc<std::sync::Mutex<Option<DaemonChild>>>,
    client: Arc<ControlClient>,
) {
    use std::time::Duration;
    const CHILD_POLL: Duration = Duration::from_secs(2);
    const SOCKET_POLL: Duration = Duration::from_secs(30);
    let mut backoff = Duration::from_secs(2);
    loop {
        let have_child = { slot.lock().ok().and_then(|g| g.as_ref().map(|_| ())) }.is_some();
        if have_child {
            tokio::time::sleep(CHILD_POLL).await;
            let exited = slot
                .lock()
                .ok()
                .and_then(|mut g| g.as_mut().and_then(|d| d.try_exited()));
            let Some(status) = exited else { continue };
            tracing::warn!("myownmesh daemon exited ({status}) — respawning in {backoff:?}");
            // Drop the dead handle now (its Drop is a no-op wait on a
            // reaped child) so the slot honestly reads "nothing running".
            if let Ok(mut g) = slot.lock() {
                g.take();
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        } else {
            tokio::time::sleep(SOCKET_POLL).await;
            if daemon_spawn::probe(&client).await {
                // A daemon (ours-before-us, the GUI's, a manual one) answers:
                // healthy, nothing to do. A live socket also means any
                // earlier spawn trouble is moot — reset the backoff.
                backoff = Duration::from_secs(2);
                continue;
            }
            tracing::warn!("no daemon answering the control socket — bringing one up");
        }
        match daemon_spawn::ensure_daemon_running(&client).await {
            Ok(child) => {
                if let Ok(mut g) = slot.lock() {
                    *g = child;
                }
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                tracing::warn!("daemon respawn failed ({e:#}); will retry");
            }
        }
    }
}

/// Wait for SIGINT (Ctrl-C) or SIGTERM (service stop), mirroring the daemon.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = sigint.recv().await;
                return;
            }
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Initialise tracing. A console build logs to stderr; a Windows service has no
/// console, so it logs to a file under `%ProgramData%\AllMyStuff\logs\`.
fn init_logging(as_service: bool) {
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::EnvFilter::new(resolve_log_filter());

    // Windows service: no console, so keep logging to the dedicated service
    // file (system-wide under %ProgramData%) exactly as before.
    #[cfg(windows)]
    if as_service {
        if let Some(make) = winsvc::log_writer() {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(false)
                        .with_ansi(false)
                        .with_writer(make),
                )
                .init();
            return;
        }
    }

    #[cfg(not(windows))]
    let _ = as_service;

    // Foreground or — crucially — spawned by the desktop app as a console-less
    // child (CREATE_NO_WINDOW): log to stdout (visible when there *is* a
    // console, e.g. `allmystuff serve` in a terminal or `just dev`) AND always
    // to a file under ~/.myownmesh/logs/, so the node leaves a findable log
    // even when nothing's watching its stdout. That file is the fix for the
    // long-standing "I can't see the node's logs on Windows when it doesn't
    // work" gap — the node now runs out-of-process, so its log has to land
    // somewhere on disk. If the home dir can't be resolved we just use stdout.
    let file_layer = node_log_writer().map(|make| {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(make)
    });
    // Colour only when a human is actually looking. Every *file* layer here
    // already sets `with_ansi(false)`; stdout was left on the default, which is
    // unconditionally ON. Under systemd that stdout is captured by
    // journald/rsyslog, so each line lands in /var/log/syslog wrapped in
    // escape sequences (`#033[2m…#033[0m`) — tens of wasted bytes per line on
    // a box that logs continuously, and grep stops matching what you can see.
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal());

    // The field-test log: a second file, in the directory the node was
    // launched from, at full debug verbosity for our crates regardless of
    // the quiet console filter — targets kept ON so every line names its
    // module (which device, which lane, which rung). This is what "send
    // me the log" points at; soft-absent when the cwd isn't writable
    // (Program Files installs), and `ALLMYSTUFF_CWD_LOG=0` turns it off.
    let (verbose_layer, verbose_paths) = match cwd_log_writer() {
        Some((make, paths)) => (
            Some(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_ansi(false)
                    .with_writer(make)
                    .with_filter(tracing_subscriber::EnvFilter::new(
                        "warn,allmystuff_node=debug,allmystuff_serve=debug",
                    )),
            ),
            paths,
        ),
        None => (None, Vec::new()),
    };

    tracing_subscriber::registry()
        .with(verbose_layer)
        .with(stdout_layer.and_then(file_layer).with_filter(filter))
        .init();
    // Name the log's homes IN the log (and on stdout/the GUI console):
    // "where are the logs" must be answerable from any surface. The
    // ProgramData path is the guaranteed machine-wide one.
    for p in &verbose_paths {
        tracing::info!("verbose field log: {}", p.display());
    }
    // The field-test telemetry line rides the log just initialized —
    // CPU + per-engine GPU busy + VRAM every 5 s, vendor-neutral.
    #[cfg(windows)]
    allmystuff_node::telemetry::start();
}

/// A `MakeWriter` over the verbose field-test log, plus the paths it
/// writes so the caller can announce them in-band once tracing is up.
///
/// **The guaranteed spot (Windows):
/// `C:\ProgramData\AllMyStuff\logs\allmystuff-serve.log`** — machine-wide
/// and identical whether serve runs from a console, as the GUI's
/// sidecar, or as the installed SERVICE. The old per-user
/// `%LOCALAPPDATA%` home is kept as a tee, but it was the field's
/// "logs aren't showing up": a service resolves `%LOCALAPPDATA%` to the
/// SYSTEM profile (`…\System32\config\systemprofile\AppData\Local`),
/// which no one thinks to open. The launch-directory copy stays too
/// when writable (the console-run habit).
///
/// Rotation, not truncation: the previous session survives as
/// `allmystuff-serve.prev.log` — a restart used to wipe exactly the
/// evidence a field report needed. `None` when nothing opens, when the local
/// development toggle is off, or `ALLMYSTUFF_CWD_LOG=0`.
///
/// Ordinary builds default off. The in-app development setting persists a
/// local opt-in for the next backend start; `ALLMYSTUFF_CWD_LOG` remains the
/// highest-priority one-shot override. Compiling the `field-telemetry` feature
/// does not implicitly enable this verbose file.
fn cwd_log_writer() -> Option<(
    impl Fn() -> TeeHandle + Send + Sync + 'static,
    Vec<std::path::PathBuf>,
)> {
    if !allmystuff_node::diagnostics::debug_logging_enabled() {
        return None;
    }
    fn open_rotating(dir: std::path::PathBuf) -> Option<(std::fs::File, std::path::PathBuf)> {
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("allmystuff-serve.log");
        // Best-effort rotate; a locked/missing file just means no .prev.
        let _ = std::fs::rename(&path, dir.join("allmystuff-serve.prev.log"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        eprintln!("verbose log: {}", path.display());
        Some((file, path))
    }
    let mut files = Vec::new();
    let mut paths = Vec::new();
    #[cfg(windows)]
    {
        let program_data = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
        if let Some((f, p)) = open_rotating(program_data.join("AllMyStuff").join("logs")) {
            files.push(f);
            paths.push(p);
        }
    }
    if let Some(d) = dirs::data_local_dir() {
        if let Some((f, p)) = open_rotating(d.join("AllMyStuff").join("logs")) {
            files.push(f);
            paths.push(p);
        }
    }
    if let Some((f, p)) = open_rotating(std::path::PathBuf::from(".")) {
        files.push(f);
        paths.push(p);
    }
    if files.is_empty() {
        return None;
    }
    Some((TeeWriter(files).into_make(), paths))
}

/// N file handles, one write — every open log location stays identical.
struct TeeWriter(Vec<std::fs::File>);

impl TeeWriter {
    fn into_make(self) -> impl Fn() -> TeeHandle + Send + Sync + 'static {
        move || TeeHandle(self.0.iter().filter_map(|f| f.try_clone().ok()).collect())
    }
}

struct TeeHandle(Vec<std::fs::File>);

impl std::io::Write for TeeHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut any = false;
        for f in &mut self.0 {
            if f.write_all(buf).is_ok() {
                any = true;
            }
        }
        if any {
            Ok(buf.len())
        } else {
            Err(std::io::Error::other("no log sink accepted the write"))
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        for f in &mut self.0 {
            let _ = f.flush();
        }
        Ok(())
    }
}

/// A `MakeWriter` over `~/.myownmesh/logs/node.log` (honoring `MYOWNMESH_HOME`,
/// beside the node socket and the ownership/shares stores), truncated once per
/// start so each node run leaves a clean, bounded log. `None` — logging falls
/// back to stdout only — if the home dir can't be resolved or the file opened.
fn node_log_writer() -> Option<impl Fn() -> std::fs::File + Send + Sync + 'static> {
    let dir = std::env::var_os("MYOWNMESH_HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)?
        .join(".myownmesh")
        .join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("node.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    Some(move || file.try_clone().expect("clone node log file handle"))
}

/// The log filter: `--log <filter>` wins, then `ALLMYSTUFF_LOG`, then a quiet
/// default (our crates at info, everything else off).
fn resolve_log_filter() -> String {
    if let Some(f) = arg_value("--log") {
        return f;
    }
    std::env::var("ALLMYSTUFF_LOG")
        .unwrap_or_else(|_| "info,allmystuff_node=info,allmystuff_serve=info".to_string())
}

/// The value following `flag` in this process's argv, if present.
fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with('-'))
        .cloned()
}

// ---------------------------------------------------------------------------
// Windows Service Control Manager glue
// ---------------------------------------------------------------------------

/// Speaks the SCM control protocol so `allmystuff-serve --service` runs as a
/// real Windows service. `allmystuff service install` registers the binary
/// with `sc.exe`; the SCM then launches it, and [`dispatch`] connects it to
/// the service control dispatcher. The node itself is unchanged — it just runs
/// under an SCM-stop future instead of a signal one.
#[cfg(windows)]
mod winsvc {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    /// Must match the SCM service name `allmystuff service install` creates
    /// (`WINDOWS_SERVICE_NAME` in the CLI's `service.rs`); the control handler
    /// can't bind otherwise.
    const SERVICE_NAME: &str = "AllMyStuff";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    define_windows_service!(ffi_service_main, service_main);

    /// Hand this process to the SCM. The dispatcher runs the service on its own
    /// thread (calling [`service_main`]) and blocks until the service stops.
    pub fn dispatch() -> ExitCode {
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("service dispatcher failed to start: {e}");
                ExitCode::FAILURE
            }
        }
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!("windows service stopped with error: {e}");
        }
    }

    fn run_service() -> windows_service::Result<()> {
        // The SCM control handler runs on its own thread. Bridge a Stop into
        // the node's async shutdown with a tokio channel: an unbounded sender
        // can fire from sync code with no runtime in scope, so the handler is
        // free to call it directly.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let handler = move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

        // Report running straight away (the node starts asynchronously), then
        // run it until the SCM asks us to stop.
        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        // `as_service = true`: the self-updater relaunches by exiting for the
        // SCM to restart, not by re-execing.
        super::run_blocking(true, async move {
            let _ = rx.recv().await;
        });

        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;
        Ok(())
    }

    /// A `MakeWriter` over `%ProgramData%\AllMyStuff\logs\service.log` (append),
    /// so a service with no console still leaves a log. `None` if the file
    /// can't be opened, in which case logging falls back to stderr.
    pub fn log_writer() -> Option<impl Fn() -> std::fs::File + Send + Sync + 'static> {
        let dir = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
            .join("AllMyStuff")
            .join("logs");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("service.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(move || file.try_clone().expect("clone service log file handle"))
    }
}
