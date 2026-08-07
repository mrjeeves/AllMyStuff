//! # allmystuff-term — `amst`, the AllMyStuff terminal
//!
//! A real shell on any machine you own, opened from your own terminal over the
//! AllMyStuff mesh — no SSH daemon, keys, or port forwarding. It's the
//! command-line twin of the desktop app's "Open Terminal": the same
//! mesh-native PTY session, with your terminal standing in for the xterm.js
//! emulator.
//!
//! ```text
//! amst                 # open a shell on THIS machine (a mesh session your
//!                      #   fleet can attach to) — opening the app if no node yet
//! amst nas-01          # open a shell on the machine called nas-01
//! amst --list          # the machines you can open a terminal on
//! amst nas-01 -s       # the open shell sessions on nas-01 (to --attach)
//! amst nas-01 -a term-3  # attach to nas-01's existing shell `term-3` (shared)
//! ```
//!
//! `amst` is a thin **client** of this machine's AllMyStuff node (the
//! `allmystuff-serve` engine the desktop app and `allmystuff serve` both run).
//! If no node is running it opens the **desktop app**, so the node it brings up
//! has a visible owner. Where there's no app to open — a headless box, or the
//! app isn't installed — it falls back to starting a headless node directly, but
//! announces it on the terminal first: an ownerless node is fine when you watch
//! it start, the *silent* auto-boot is what `amst` avoids. (For an always-on
//! node across reboots, use `allmystuff service install`.) Reaching another
//! machine needs it to be online and yours (owner or same fleet) — the same
//! rule the desktop app's terminal enforces, re-checked on the far side.

mod attach;
mod client;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use allmystuff_protocol::{NodeProfile, FEATURE_TERMINAL};
use client::{wait_for_socket, NodeClient, NodeEvent};

/// How long to wait for a freshly started node's mesh to come up.
const READY_TIMEOUT: Duration = Duration::from_secs(25);
/// How long to wait for the node to bind its control socket after we open the
/// desktop app. The app has to start its webview and spawn the node, so this is
/// longer than a bare node spawn would need.
const GUI_BOOT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for a headless `allmystuff-serve` node we spawned directly
/// to bind its control socket — no webview to start first, so shorter than the
/// app's.
const SERVE_BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for a named machine to appear in presence after a cold
/// start (adverts trickle in over the first few seconds).
const FIND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the far machine to accept the terminal offer.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Entry point for both the `amst` binary and `allmystuff term`. `args` is
/// everything after the program / subcommand name.
pub fn run(args: &[String]) -> ExitCode {
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("amst: {e}\n");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    if opts.help {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if opts.version {
        println!("amst {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("amst: couldn't start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(go(opts)) {
        Ok(code) => {
            // Mirror the far shell's exit status when we can (0..=255).
            ExitCode::from(u8::try_from(code).unwrap_or(0))
        }
        Err(e) => {
            eprintln!("amst: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The orchestration: ensure a node, resolve the target, open the route, and
/// hand off to the interactive loop. Returns the far shell's exit code.
async fn go(opts: Opts) -> Result<i32, String> {
    let client = Arc::new(NodeClient::new()?);
    ensure_node(&client).await?;
    let snap = wait_ready(&client, READY_TIMEOUT).await?;
    let me = snap
        .get("me")
        .and_then(Value::as_str)
        .ok_or("the node has no identity yet")?
        .to_string();

    if opts.list {
        let machines = collect_machines(&client, &me, &snap).await;
        print_machines(&machines);
        return Ok(0);
    }

    // Resolve the host node: no target → this machine (loopback).
    let host = match &opts.target {
        None => Machine::this(&client, &me).await,
        Some(query) => resolve_or_wait(&client, &me, query, FIND_TIMEOUT).await?,
    };
    if !host.terminal {
        return Err(format!(
            "{} doesn't advertise terminal support (an older AllMyStuff, perhaps).",
            host.label
        ));
    }

    // `--sessions`: just list the host's open shells and stop.
    if opts.sessions {
        return list_sessions(&client, &host).await.map(|_| 0);
    }

    // Subscribe to the node's events *before* offering, so the host's first
    // output poke and the shell's exit are never missed.
    let (ev_tx, ev_rx) = mpsc::channel::<NodeEvent>(256);
    client.subscribe_events(ev_tx).await?;

    let where_ = if host.is_me {
        "this machine".to_string()
    } else {
        host.label.clone()
    };
    match &opts.attach {
        Some(id) => eprintln!("amst: attaching to shell {id} on {where_}…"),
        None => eprintln!("amst: opening a terminal on {where_}…"),
    }

    let from = format!("{}:terminal", host.node);
    let to = format!(
        "{me}:term-view:amst-{}-{}",
        std::process::id(),
        now_millis()
    );
    let route_id = client
        .request(
            "connect_route",
            json!({
                "from": from,
                "to": to,
                "media": "generic",
                "video": [],
                "session": opts.attach,
            }),
        )
        .await?
        .as_str()
        .ok_or("the node didn't return a route id")?
        .to_string();

    if let Err(e) = wait_active(&client, &route_id, &where_, ACCEPT_TIMEOUT).await {
        // Don't leave a half-open offer dangling on the way out.
        let _ = client
            .request("disconnect_route", json!({ "route_id": route_id }))
            .await;
        return Err(e);
    }

    // `--cwd` (the "open a terminal here" launch) starts you in that folder by
    // injecting a `cd` once the prompt is up. It's a local path, so it only
    // makes sense for a terminal on *this* machine; ignored (with a note) for a
    // remote one.
    let initial_input = if opts.admin {
        if !host.os.to_ascii_lowercase().contains("windows") {
            return Err("--admin currently needs a Windows target".into());
        }
        Some(windows_admin_input(opts.run.as_deref()))
    } else if let Some(command) = &opts.run {
        Some(run_and_exit_command(
            command,
            host.os.to_ascii_lowercase().contains("windows"),
        ))
    } else {
        match &opts.cwd {
            Some(dir) if host.is_me => Some(cd_command(dir)),
            Some(_) => {
                eprintln!("amst: --cwd is ignored for a remote machine (it's a local path).");
                None
            }
            None => None,
        }
    };

    let code = attach::run(client.clone(), route_id, ev_rx, initial_input).await?;
    eprintln!("\r\namst: terminal closed.");
    Ok(code)
}

/// The `cd <dir>` keystrokes to drop the shell into `dir`, as the "open a
/// terminal here" integration wants. POSIX single-quoting on unix (every shell
/// from `sh` to `fish` honours it); a double-quoted `cd` on Windows (PowerShell
/// always, `cmd` for the same drive). Best-effort — a path the shell rejects
/// just leaves you at the prompt, nothing breaks.
fn cd_command(dir: &str) -> Vec<u8> {
    #[cfg(windows)]
    {
        format!("cd \"{}\"\r\n", dir.replace('"', "")).into_bytes()
    }
    #[cfg(not(windows))]
    {
        // Single-quote and escape embedded single quotes ('\'' closes, escapes,
        // reopens) so any path is passed literally.
        format!("cd '{}'\n", dir.replace('\'', "'\\''")).into_bytes()
    }
}

/// Run one command in the remote shell and close that shell with its status.
/// This is the scriptable twin of the interactive terminal; it travels through
/// the exact same owner/fleet route authorization.
fn run_and_exit_command(command: &str, windows_target: bool) -> Vec<u8> {
    if windows_target {
        format!("{command}\r\nexit $LASTEXITCODE\r\n").into_bytes()
    } else {
        format!("{command}\nexit $?\n").into_bytes()
    }
}

/// Require the shell we are already attached to to be an administrator shell.
/// There is deliberately no `RunAs`/UAC fallback: a remote terminal cannot
/// operate a consent prompt safely. On an Always On Windows node the terminal
/// host is the installed LocalSystem service, so the check succeeds and the
/// same PTY remains fully attached. An ordinary desktop node fails closed with
/// one actionable error instead of opening a disconnected elevated window.
fn windows_admin_input(command: Option<&str>) -> Vec<u8> {
    let guard = concat!(
        "$__amstAdmin = ([Security.Principal.WindowsPrincipal] ",
        "[Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(",
        "[Security.Principal.WindowsBuiltInRole]::Administrator); ",
        "if (-not $__amstAdmin) { Write-Error '",
        "Administrator terminal unavailable. Install and run AllMyStuff Always On on this machine.'; ",
        "exit 126 }; Remove-Variable __amstAdmin"
    );
    match command {
        Some(command) => format!("{guard}; {command}; exit $LASTEXITCODE\r\n").into_bytes(),
        None => {
            format!("{guard}; $Host.UI.RawUI.WindowTitle='AMST Administrator'\r\n").into_bytes()
        }
    }
}

// ---------------------------------------------------------------------------
// Node lifecycle
// ---------------------------------------------------------------------------

/// Make sure a node answers the control socket, starting one if not. A node is
/// this machine's mesh presence, and `amst` prefers to give it a visible owner:
/// when nothing is listening it **opens the desktop app** (`allmystuff-gui`),
/// which spawns and owns the node.
///
/// Where there's no app to open — a headless box, or the app simply isn't
/// installed — it falls back to starting a headless `allmystuff-serve` node
/// directly, but **announces it on the terminal** first. That's the line: the
/// thing `amst` never does is start a *silent* ownerless node behind your back;
/// a headless node you watched it start (with a pointer at the service for an
/// always-on one) is fine.
async fn ensure_node(_client: &Arc<NodeClient>) -> Result<(), String> {
    if NodeClient::probe().await {
        return Ok(());
    }

    // Prefer opening the desktop app, so the node it brings up has a visible
    // owner rather than running headless.
    let can_open_gui = gui_can_open();
    if can_open_gui {
        if let Some(bin) = allmystuff_service::find_gui_binary() {
            eprintln!("amst: no AllMyStuff node is running here — opening the AllMyStuff app…");
            spawn_detached(&bin)?;
            return if wait_for_socket(GUI_BOOT_TIMEOUT).await {
                Ok(())
            } else {
                Err(
                    "opened the AllMyStuff app, but its node didn't come up in time. Give \
                     it a moment and re-run amst, or start one yourself with `allmystuff \
                     serve`."
                        .into(),
                )
            };
        }
    }

    // No app to open — a headless box, or it isn't installed. Fall back to a
    // headless node, announced (never silent).
    let reason = if can_open_gui {
        "the desktop app isn't installed here"
    } else {
        "this looks like a headless box (no display)"
    };
    boot_headless_node(reason).await
}

/// Start a headless `allmystuff-serve` node directly, when the desktop app can't
/// be opened. Warns that the node has no GUI owner — the announcement is exactly
/// what makes this acceptable, versus the silent ownerless auto-boot `amst`
/// avoids. `reason` says why we fell back (headless box / app not installed).
async fn boot_headless_node(reason: &str) -> Result<(), String> {
    let bin = allmystuff_service::find_serve_binary().ok_or(
        "no AllMyStuff node is running here, and neither the desktop app nor the \
         `allmystuff-serve` node binary could be found to start one. Install AllMyStuff \
         (https://allmystuff.works), then re-run amst.",
    )?;
    eprintln!(
        "amst: no AllMyStuff node is running here and {reason} — starting a headless node \
         (`allmystuff-serve`, no app/GUI owner) on this machine."
    );
    eprintln!(
        "amst: it keeps running after amst exits. For an always-on node across reboots use \
         `allmystuff service install`; on a machine with a screen, the desktop app gives \
         the node an owner."
    );
    spawn_detached(&bin)?;
    if wait_for_socket(SERVE_BOOT_TIMEOUT).await {
        Ok(())
    } else {
        Err(
            "the node didn't come up in time. Try `allmystuff serve` in another terminal, \
             then re-run amst."
                .into(),
        )
    }
}

/// Whether the desktop app can be opened here. It needs a display: on Linux a
/// session with neither `DISPLAY` nor `WAYLAND_DISPLAY` is headless and there's
/// nothing to open (mirrors the CLI's bare-`allmystuff` guard). macOS and
/// Windows always have a window server for a logged-in user.
fn gui_can_open() -> bool {
    if cfg!(target_os = "linux") {
        !linux_is_headless(
            std::env::var_os("DISPLAY").is_some(),
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
        )
    } else {
        true
    }
}

/// A Linux session is headless when neither display server is advertised. Pure
/// so it's unit-testable without touching the environment.
fn linux_is_headless(has_x11: bool, has_wayland: bool) -> bool {
    !has_x11 && !has_wayland
}

/// Launch `bin` (the desktop app, or a headless node) detached, so it outlives
/// this short-lived `amst` process and keeps the machine on the mesh after
/// `amst` exits. No console of its own, stdio to the void so its logs don't
/// scribble over the shell. We deliberately don't keep or reap the child.
fn spawn_detached(bin: &std::path::Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW | DETACHED_PROCESS — no inherited console, survives
        // this process. The app is a windowed (windows-subsystem) binary, so it
        // still puts up its own window; the node has no window either way.
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }
    cmd.spawn()
        .map(|_child| ())
        .map_err(|e| format!("couldn't launch {}: {e}", bin.display()))
}

/// Poll the node until its mesh is ready (it has an identity), returning the
/// snapshot. A just-started node takes a beat to bring the daemon up.
async fn wait_ready(client: &Arc<NodeClient>, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(snap) = client.request("session_snapshot", Value::Null).await {
            if snap.get("ready").and_then(Value::as_bool) == Some(true) {
                return Ok(snap);
            }
        }
        if Instant::now() >= deadline {
            return Err("the node is up but the mesh isn't ready yet. Give it a \
                        moment and re-run amst."
                .into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Wait for `route_id` to go active (the host accepted), or surface why it
/// didn't (declined / torn down / timed out).
async fn wait_active(
    client: &Arc<NodeClient>,
    route_id: &str,
    label: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = client.request("session_snapshot", Value::Null).await?;
        if let Some(state) = route_state(&snap, route_id) {
            match state.as_str() {
                "active" => return Ok(()),
                "rejected" => {
                    let reason = route_reject_reason(&snap, route_id)
                        .unwrap_or_else(|| "declined".to_string());
                    return Err(format!("{label} declined the terminal: {reason}"));
                }
                "torn_down" => {
                    return Err(format!("{label} closed the connection before it opened."))
                }
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no response from {label} (it may be offline, not yours, or busy)."
            ));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

// ---------------------------------------------------------------------------
// Machines
// ---------------------------------------------------------------------------

/// A machine on the mesh as the picker sees it.
#[derive(Debug, Clone)]
struct Machine {
    /// The mesh node id used to build endpoints.
    node: String,
    /// A human name (presence label, else hostname, else short id).
    label: String,
    /// The machine's hostname, for matching.
    host: String,
    /// Host OS from presence (`Windows`, `macOS`, …), used to refuse a
    /// Windows-only administrator launch before typing nonsense into a shell.
    os: String,
    online: bool,
    /// Whether it advertises terminal support.
    terminal: bool,
    /// Whether it's the local machine (a loopback session).
    is_me: bool,
}

impl Machine {
    /// The local machine, named from its own inventory scan.
    async fn this(client: &Arc<NodeClient>, me: &str) -> Machine {
        let (label, host) = local_name(client, me).await;
        Machine {
            node: me.to_string(),
            label,
            host,
            os: std::env::consts::OS.to_string(),
            online: true,
            terminal: true, // we host the terminal ourselves
            is_me: true,
        }
    }
}

/// The local machine's (label, hostname) from `scan_self`, falling back to a
/// short id when the scan is unavailable.
async fn local_name(client: &Arc<NodeClient>, me: &str) -> (String, String) {
    if let Ok(v) = client.request("scan_self", Value::Null).await {
        let label = v
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let host = v
            .get("hostname")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let (Some(label), Some(host)) = (label.clone(), host.clone()) {
            return (label, host);
        }
        if let Some(label) = label {
            return (label.clone(), host.unwrap_or(label));
        }
    }
    let short = short_id(me);
    (short.clone(), short)
}

/// Every machine you could open a terminal on right now: the present peers plus
/// this machine.
async fn collect_machines(client: &Arc<NodeClient>, me: &str, snap: &Value) -> Vec<Machine> {
    let mut machines = vec![Machine::this(client, me).await];
    machines.extend(peer_machines(snap));
    machines
}

/// The present peers from a snapshot as machines (excludes this machine).
fn peer_machines(snap: &Value) -> Vec<Machine> {
    let peers: Vec<NodeProfile> = snap
        .get("peers")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    peers
        .into_iter()
        .map(|p| {
            let node = p.node.to_string();
            let label = if p.label.is_empty() {
                if p.hostname.is_empty() {
                    short_id(&node)
                } else {
                    p.hostname.clone()
                }
            } else {
                p.label.clone()
            };
            Machine {
                node,
                label,
                host: p.hostname,
                os: p.summary.os,
                online: true, // present in the snapshot ⇒ online
                terminal: p.features.iter().any(|f| f == FEATURE_TERMINAL),
                is_me: false,
            }
        })
        .collect()
}

/// Resolve `query` to a host machine, re-polling presence for `timeout` so a
/// machine that hasn't adverted yet after a cold start still gets found.
async fn resolve_or_wait(
    client: &Arc<NodeClient>,
    me: &str,
    query: &str,
    timeout: Duration,
) -> Result<Machine, String> {
    // Resolve this machine's name once (an inventory scan); only presence
    // (the peers) is re-polled below.
    let this = Machine::this(client, me).await;
    let deadline = Instant::now() + timeout;
    loop {
        let snap = client.request("session_snapshot", Value::Null).await?;
        let mut machines = vec![this.clone()];
        machines.extend(peer_machines(&snap));
        match resolve_match(&machines, query) {
            Resolution::One(i) => return Ok(machines[i].clone()),
            Resolution::Ambiguous(idxs) => {
                let names: Vec<String> = idxs.iter().map(|&i| machines[i].label.clone()).collect();
                return Err(format!(
                    "`{query}` matches more than one machine: {}. Be more specific.",
                    names.join(", ")
                ));
            }
            Resolution::None => {
                if Instant::now() >= deadline {
                    return Err(no_match_message(query, &machines));
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
    }
}

/// The outcome of matching a query against the machine list.
#[derive(Debug, PartialEq)]
enum Resolution {
    One(usize),
    None,
    Ambiguous(Vec<usize>),
}

/// Match `query` (case-insensitive) against the machines: an exact hit on id /
/// pubkey / label / hostname wins; otherwise a unique prefix on label / host /
/// id. Exact beats prefix, so `nas` reaches `nas` even when `nas-01` exists.
fn resolve_match(machines: &[Machine], query: &str) -> Resolution {
    let q = query.to_lowercase();
    let exact: Vec<usize> = machines
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.node.to_lowercase() == q
                || pubkey_part(&m.node).to_lowercase() == q
                || m.label.to_lowercase() == q
                || m.host.to_lowercase() == q
        })
        .map(|(i, _)| i)
        .collect();
    match exact.len() {
        1 => return Resolution::One(exact[0]),
        n if n > 1 => return Resolution::Ambiguous(exact),
        _ => {}
    }
    let prefix: Vec<usize> = machines
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.label.to_lowercase().starts_with(&q)
                || m.host.to_lowercase().starts_with(&q)
                || m.node.to_lowercase().starts_with(&q)
        })
        .map(|(i, _)| i)
        .collect();
    match prefix.len() {
        1 => Resolution::One(prefix[0]),
        0 => Resolution::None,
        _ => Resolution::Ambiguous(prefix),
    }
}

fn no_match_message(query: &str, machines: &[Machine]) -> String {
    let mut msg = format!("no machine matching `{query}`.");
    let others: Vec<&Machine> = machines.iter().filter(|m| !m.is_me).collect();
    if others.is_empty() {
        msg.push_str(
            " No other machines are on your mesh yet — install AllMyStuff on \
             another computer you own (or run `allmystuff serve` there), then \
             try again. (Run plain `amst` for a terminal on this machine.)",
        );
    } else {
        msg.push_str("\nMachines you can reach: ");
        let names: Vec<String> = others.iter().map(|m| m.label.clone()).collect();
        msg.push_str(&names.join(", "));
        msg.push_str("\nRun `amst --list` for the full list.");
    }
    msg
}

fn print_machines(machines: &[Machine]) {
    println!("Machines you can open a terminal on:\n");
    let width = machines
        .iter()
        .map(|m| m.label.len())
        .max()
        .unwrap_or(0)
        .max(4);
    for m in machines {
        let dot = if m.online { '●' } else { '○' };
        let mut tags = Vec::new();
        if m.is_me {
            tags.push("this machine".to_string());
        }
        if !m.terminal {
            tags.push("no terminal support".to_string());
        }
        let tag = if tags.is_empty() {
            String::new()
        } else {
            format!("  ({})", tags.join(", "))
        };
        println!(
            "  {dot}  {label:<width$}  {host:<20}  {id}{tag}",
            label = m.label,
            host = m.host,
            id = short_id(&m.node),
        );
    }
    println!("\nUse:  amst <name>   open a terminal on a machine");
    println!("      amst          open a terminal on this machine");
}

/// List the open shell sessions on `host` (for `--attach`). The local machine
/// answers at once; a remote host answers asynchronously over the mesh, so we
/// wait briefly on its `terminal-sessions` event.
async fn list_sessions(client: &Arc<NodeClient>, host: &Machine) -> Result<(), String> {
    let immediate = client
        .request("terminal_sessions", json!({ "node": host.node }))
        .await?;
    let sessions = if immediate.is_null() {
        // Remote: subscribe and wait for the host's answer.
        let (tx, mut rx) = mpsc::channel::<NodeEvent>(64);
        client.subscribe_events(tx).await?;
        // Re-ask now that we're listening (the first ask may have raced the
        // subscription; the node de-dups nothing here, it just re-requests).
        let _ = client
            .request("terminal_sessions", json!({ "node": host.node }))
            .await?;
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("{} didn't answer in time.", host.label));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(NodeEvent::Emit { event, payload }))
                    if event == "allmystuff://terminal-sessions" =>
                {
                    let from = payload.get("from").and_then(Value::as_str).unwrap_or("");
                    if pubkey_part(from) == pubkey_part(&host.node) {
                        break payload.get("sessions").cloned().unwrap_or(json!([]));
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return Err(format!("{} didn't answer in time.", host.label)),
            }
        }
    } else {
        immediate
    };

    let rows = sessions.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No open shell sessions on {}.", host.label);
        return Ok(());
    }
    println!("Open shell sessions on {}:\n", host.label);
    for s in rows {
        let id = s.get("session_id").and_then(Value::as_str).unwrap_or("?");
        let attachers = s.get("attachers").and_then(Value::as_u64).unwrap_or(0);
        let shared = match attachers {
            0 | 1 => String::new(),
            n => format!("  (shared with {})", n - 1),
        };
        println!("  {id}{shared}");
    }
    let target = if host.is_me {
        String::new()
    } else {
        format!(" {}", host.label)
    };
    println!("\nAttach with:  amst{target} --attach <id>");
    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot helpers
// ---------------------------------------------------------------------------

/// The `state` string of `route_id` in a snapshot (`active` / `offered` /
/// `rejected` / `torn_down` / …), if the route is present.
fn route_state(snap: &Value, route_id: &str) -> Option<String> {
    find_route(snap, route_id)?
        .get("state")?
        .get("state")?
        .as_str()
        .map(str::to_string)
}

fn route_reject_reason(snap: &Value, route_id: &str) -> Option<String> {
    find_route(snap, route_id)?
        .get("state")?
        .get("reason")?
        .as_str()
        .map(str::to_string)
}

fn find_route<'a>(snap: &'a Value, route_id: &str) -> Option<&'a Value> {
    snap.get("routes")?.as_array()?.iter().find(|r| {
        r.get("route")
            .and_then(|rt| rt.get("id"))
            .and_then(Value::as_str)
            == Some(route_id)
    })
}

// ---------------------------------------------------------------------------
// Small id + time helpers (mirror the node's)
// ---------------------------------------------------------------------------

/// Strip a node id's trailing `-XXXXX` network suffix (5 alnum chars) to its
/// canonical body — mirrors the node's `pubkey_part`, so two ids for the same
/// machine on different networks compare equal.
fn pubkey_part(id: &str) -> &str {
    if let Some((body, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 5 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return body;
        }
    }
    id
}

/// A short, readable form of a node id for display.
fn short_id(id: &str) -> String {
    let body = pubkey_part(id);
    if body.len() > 12 {
        format!("{}…", &body[..12])
    } else {
        body.to_string()
    }
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
struct Opts {
    target: Option<String>,
    attach: Option<String>,
    /// Start the shell in this directory (a local path; only meaningful for a
    /// terminal on this machine) — what the "open a terminal here" shell
    /// integration passes.
    cwd: Option<String>,
    /// Run one command instead of staying interactive.
    run: Option<String>,
    /// On a Windows target, require the service-hosted administrator terminal.
    /// This is OS posture, not a new mesh grant, and never opens a UAC prompt.
    admin: bool,
    list: bool,
    sessions: bool,
    help: bool,
    version: bool,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => o.help = true,
            "-V" | "--version" => o.version = true,
            "-l" | "--list" => o.list = true,
            "-s" | "--sessions" => o.sessions = true,
            "--admin" => o.admin = true,
            "-r" | "--run" => {
                i += 1;
                match args.get(i) {
                    Some(v) => o.run = Some(v.clone()),
                    None => return Err("--run needs a command".into()),
                }
            }
            s if s.starts_with("--run=") => {
                o.run = Some(s["--run=".len()..].to_string());
            }
            "-a" | "--attach" => {
                i += 1;
                match args.get(i) {
                    Some(v) if !v.starts_with('-') => o.attach = Some(v.clone()),
                    _ => return Err("--attach needs a session id".into()),
                }
            }
            s if s.starts_with("--attach=") => {
                o.attach = Some(s["--attach=".len()..].to_string());
            }
            "-C" | "--cwd" => {
                i += 1;
                match args.get(i) {
                    Some(v) => o.cwd = Some(v.clone()),
                    None => return Err("--cwd needs a directory".into()),
                }
            }
            s if s.starts_with("--cwd=") => {
                o.cwd = Some(s["--cwd=".len()..].to_string());
            }
            s if s.starts_with('-') && s != "-" => {
                return Err(format!("unknown option `{s}`"));
            }
            s => {
                if o.target.is_some() {
                    return Err(format!("unexpected extra argument `{s}`"));
                }
                o.target = Some(s.to_string());
            }
        }
        i += 1;
    }
    if o.attach.is_some() && (o.admin || o.run.is_some()) {
        return Err("--attach cannot be combined with --admin or --run".into());
    }
    if (o.list || o.sessions) && (o.admin || o.run.is_some()) {
        return Err("--admin/--run cannot be combined with --list or --sessions".into());
    }
    if o.cwd.is_some() && (o.admin || o.run.is_some()) {
        return Err("--cwd cannot be combined with --admin or --run".into());
    }
    Ok(o)
}

const USAGE: &str =
    "USAGE:\n    amst [OPTIONS] [MACHINE]\n\nRun `amst --help` for the full reference.";

const HELP: &str = "amst — open a real shell on any machine you own, over the AllMyStuff mesh.

USAGE:
    amst [OPTIONS] [MACHINE]

    MACHINE is the name, hostname, or id of a machine on your mesh (a unique
    prefix is enough). With no MACHINE, opens a terminal on THIS machine — a
    mesh session your other machines can attach to.

OPTIONS:
    -l, --list           List the machines you can open a terminal on.
    -s, --sessions       List the open shell sessions on MACHINE (to --attach).
    -a, --attach <ID>    Attach to an existing shell session (shared, tmux-style)
                         instead of opening a new one.
    -C, --cwd <DIR>      Start the shell in DIR (a terminal on this machine) —
                         what the 'open a terminal here' shell integration uses.
    -r, --run <COMMAND>  Run one command through the terminal and return its
                         exit status instead of staying interactive.
    --admin              Windows: open the attached administrator shell on
                         MACHINE, or run --run there. Requires the installed
                         Always On service; never opens a UAC prompt.
    -h, --help           Show this help.
    -V, --version        Print the version.

NOTES:
    Needs a running AllMyStuff node on this machine. If none is running, amst
    opens the desktop app, so the node has a visible owner. Where there's no app
    to open — a headless box, or it isn't installed — amst starts a headless node
    directly instead, and says so (it never starts one silently). For an
    always-on node across reboots, use `allmystuff service install`.

    Reaching another machine needs it online and yours — owner or same fleet —
    the same rule the desktop app's terminal enforces.";

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(node: &str, label: &str, host: &str, terminal: bool, is_me: bool) -> Machine {
        Machine {
            node: node.into(),
            label: label.into(),
            host: host.into(),
            os: "windows".into(),
            online: true,
            terminal,
            is_me,
        }
    }

    #[test]
    fn parse_admin_and_run_as_general_terminal_options() {
        let a: Vec<String> = ["desk", "--admin", "--run", "sfc /scannow"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_args(&a).unwrap();
        assert_eq!(opts.target.as_deref(), Some("desk"));
        assert!(opts.admin);
        assert_eq!(opts.run.as_deref(), Some("sfc /scannow"));

        let a: Vec<String> = ["--run=whoami", "desk"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_args(&a).unwrap().run.as_deref(), Some("whoami"));
    }

    #[test]
    fn parse_rejects_conflicting_terminal_modes() {
        for args in [
            &["--attach", "term-1", "--admin"][..],
            &["--list", "--run", "whoami"][..],
            &["--cwd", "C:\\\\", "--admin"][..],
        ] {
            let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert!(parse_args(&args).is_err());
        }
    }

    #[test]
    fn one_shot_exit_syntax_follows_the_target_os() {
        assert_eq!(
            run_and_exit_command("whoami", true),
            b"whoami\r\nexit $LASTEXITCODE\r\n".to_vec()
        );
        assert_eq!(run_and_exit_command("id", false), b"id\nexit $?\n".to_vec());
    }

    #[test]
    fn administrator_command_stays_in_the_attached_service_shell() {
        let input = String::from_utf8(windows_admin_input(Some("sfc /scannow"))).unwrap();
        assert!(input.contains("IsInRole"));
        assert!(input.contains("Always On"));
        assert!(input.contains("sfc /scannow"));
        assert!(!input.contains("RunAs"));
        assert!(!input.contains("Start-Process"));
    }

    #[test]
    fn parse_target_and_flags_in_any_order() {
        let a: Vec<String> = ["-l"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            parse_args(&a).unwrap(),
            Opts {
                list: true,
                ..Default::default()
            }
        );

        let a: Vec<String> = ["nas-01", "--attach", "term-3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_args(&a).unwrap(),
            Opts {
                target: Some("nas-01".into()),
                attach: Some("term-3".into()),
                ..Default::default()
            }
        );

        // Flags may precede the machine, and `--attach=` works too.
        let a: Vec<String> = ["--attach=t9", "desk"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_args(&a).unwrap(),
            Opts {
                target: Some("desk".into()),
                attach: Some("t9".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn parse_cwd_flag_forms() {
        let a: Vec<String> = ["--cwd", "/home/u/proj"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_args(&a).unwrap().cwd.as_deref(), Some("/home/u/proj"));
        let a: Vec<String> = ["-C", "/tmp"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_args(&a).unwrap().cwd.as_deref(), Some("/tmp"));
        let a: Vec<String> = ["--cwd=/x/y"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_args(&a).unwrap().cwd.as_deref(), Some("/x/y"));
        let a: Vec<String> = ["--cwd"].iter().map(|s| s.to_string()).collect();
        assert!(parse_args(&a).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn cd_command_single_quotes_and_escapes() {
        assert_eq!(cd_command("/home/u"), b"cd '/home/u'\n".to_vec());
        // An embedded single quote is closed/escaped/reopened so the path is
        // passed literally.
        assert_eq!(
            cd_command("/a/it's here"),
            b"cd '/a/it'\\''s here'\n".to_vec()
        );
    }

    #[test]
    fn linux_headless_only_when_no_display_server() {
        // A display server of either kind means we can open the desktop app.
        assert!(!linux_is_headless(true, false));
        assert!(!linux_is_headless(false, true));
        assert!(!linux_is_headless(true, true));
        // Neither ⇒ headless, so amst must not try to open the app (it points
        // the user at `allmystuff serve` / the service instead).
        assert!(linux_is_headless(false, false));
    }

    #[test]
    fn parse_rejects_dangling_attach_and_extra_args() {
        let a: Vec<String> = ["--attach"].iter().map(|s| s.to_string()).collect();
        assert!(parse_args(&a).is_err());
        let a: Vec<String> = ["one", "two"].iter().map(|s| s.to_string()).collect();
        assert!(parse_args(&a).is_err());
        let a: Vec<String> = ["--nope"].iter().map(|s| s.to_string()).collect();
        assert!(parse_args(&a).is_err());
    }

    #[test]
    fn pubkey_part_strips_only_a_5char_suffix() {
        assert_eq!(pubkey_part("abcdef"), "abcdef");
        assert_eq!(pubkey_part("abcdef-12ab3"), "abcdef");
        // A non-5 suffix (or non-alnum) is left alone.
        assert_eq!(pubkey_part("abcdef-12a"), "abcdef-12a");
        assert_eq!(pubkey_part("abc-de_fg"), "abc-de_fg");
    }

    #[test]
    fn exact_match_beats_prefix() {
        let ms = vec![
            machine("n1", "nas", "nas.local", true, false),
            machine("n2", "nas-01", "nas-01.local", true, false),
        ];
        // `nas` is an exact label hit on the first, even though it also prefixes
        // the second.
        assert_eq!(resolve_match(&ms, "nas"), Resolution::One(0));
        // A prefix that only one machine starts with resolves uniquely.
        assert_eq!(resolve_match(&ms, "nas-"), Resolution::One(1));
        // Case-insensitive.
        assert_eq!(resolve_match(&ms, "NAS-01"), Resolution::One(1));
    }

    #[test]
    fn ambiguous_and_missing_resolve_as_such() {
        let ms = vec![
            machine("n1", "desk-a", "desk-a", true, false),
            machine("n2", "desk-b", "desk-b", true, false),
        ];
        assert!(matches!(
            resolve_match(&ms, "desk"),
            Resolution::Ambiguous(_)
        ));
        assert_eq!(resolve_match(&ms, "laptop"), Resolution::None);
    }

    #[test]
    fn match_resolves_by_id_and_pubkey() {
        let ms = vec![machine("pubkeybody-1a2b3", "box", "box.local", true, false)];
        assert_eq!(resolve_match(&ms, "pubkeybody-1a2b3"), Resolution::One(0));
        assert_eq!(resolve_match(&ms, "pubkeybody"), Resolution::One(0));
    }

    #[test]
    fn route_state_reads_the_nested_tag() {
        let snap = json!({
            "routes": [
                { "route": { "id": "route:a→b" }, "state": { "state": "active" } },
                { "route": { "id": "route:c→d" }, "state": { "state": "rejected", "reason": "not yours" } },
            ]
        });
        assert_eq!(route_state(&snap, "route:a→b").as_deref(), Some("active"));
        assert_eq!(route_state(&snap, "route:c→d").as_deref(), Some("rejected"));
        assert_eq!(
            route_reject_reason(&snap, "route:c→d").as_deref(),
            Some("not yours")
        );
        assert!(route_state(&snap, "route:none").is_none());
    }

    #[test]
    fn peer_machines_reads_presence_and_terminal_feature() {
        let snap = json!({
            "peers": [
                { "protocol": 1, "node": "n1", "label": "nas", "hostname": "nas.local",
                  "summary": { "os": "linux", "cpu": "x", "ram_bytes": 0, "device_count": 0 },
                  "capabilities": [], "claimable": false, "boot": 0,
                  "features": ["terminal"], "sites": [], "version": "" },
                { "protocol": 1, "node": "n2", "label": "old", "hostname": "old.local",
                  "summary": { "os": "linux", "cpu": "x", "ram_bytes": 0, "device_count": 0 },
                  "capabilities": [], "claimable": false, "boot": 0,
                  "features": [], "sites": [], "version": "" },
            ]
        });
        let ms = peer_machines(&snap);
        assert_eq!(ms.len(), 2);
        assert!(ms[0].terminal && !ms[0].is_me);
        assert!(!ms[1].terminal);
    }
}
