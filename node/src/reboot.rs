//! OS-level reboot of *this* machine — the action behind the gear menu's
//! "Restart this device" and the [`AppControl::RestartDevice`] command a
//! fleet peer sends. One step heavier than relaunching the app: for the
//! wedge an app restart can't clear (a hung compositor, a stuck driver, a
//! box that needs its pending OS updates applied).
//!
//! The reboot is *asked of the OS*, never forced from here: each platform's
//! ordinary reboot mechanism runs with this process's privileges, so the
//! OS's own rules stay in charge (an unprivileged Linux session goes
//! through logind/polkit, macOS through System Events, Windows through
//! `shutdown.exe`'s SeShutdownPrivilege). Attempts run in order and the
//! first success wins; if they all refuse, the caller gets every reason —
//! a refusal must be visible, not a silent nothing-happened.
//!
//! [`AppControl::RestartDevice`]: allmystuff_protocol::AppControl::RestartDevice

/// Ask the OS to reboot this machine. Returns once the reboot is *accepted*
/// (the commands schedule it and exit); `Err` carries every attempt's
/// refusal when none was. Blocking (waits on each command) — call it off
/// the async runtime via `spawn_blocking`.
pub fn restart_device() -> Result<(), String> {
    let mut refusals = Vec::new();
    for (bin, args) in attempts() {
        match crate::child_process::blocking_command(bin).args(args).status() {
            Ok(status) if status.success() => {
                tracing::info!("device reboot accepted by `{bin}`");
                return Ok(());
            }
            Ok(status) => refusals.push(format!("{bin}: {status}")),
            Err(e) => refusals.push(format!("{bin}: {e}")),
        }
    }
    Err(format!(
        "the OS refused the reboot ({})",
        refusals.join("; ")
    ))
}

/// The platform's reboot avenues, most appropriate first.
#[cfg(target_os = "linux")]
fn attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        // logind first: works for an active local seat *and* for root, and
        // gives other sessions the ordinary shutdown courtesy.
        ("systemctl", vec!["reboot"]),
        ("shutdown", vec!["-r", "now"]),
        ("reboot", vec![]),
    ]
}

/// The platform's reboot avenues, most appropriate first.
#[cfg(target_os = "macos")]
fn attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        // The logged-in user's polite path (no root): System Events restarts
        // the session the way the  menu does.
        (
            "osascript",
            vec!["-e", "tell application \"System Events\" to restart"],
        ),
        // Root's direct path — the headless service case.
        ("shutdown", vec!["-r", "now"]),
    ]
}

/// The platform's reboot avenues, most appropriate first.
#[cfg(windows)]
fn attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![(
        "shutdown",
        // A short fuse instead of /t 0: long enough for the node's ack and
        // logs to flush, short enough to still read as "it rebooted".
        vec![
            "/r",
            "/t",
            "5",
            "/c",
            "AllMyStuff: device restart requested",
        ],
    )]
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn attempts() -> Vec<(&'static str, Vec<&'static str>)> {
    Vec::new()
}
