//! AllMyStuff mobile (iOS / Android) — Tauri shell.
//!
//! The window is the **same** Svelte app the desktop ships (the graph/map,
//! Console, Files, Terminal, Rooms — see `../../gui/src`). The frontend is
//! built to run with or without a backend: every call goes through a
//! `tryInvoke` that falls back to demo data when a command is absent, so the
//! UI is fully navigable from the first build while the native planes land
//! one at a time.
//!
//! What differs from the desktop shell (`../src-tauri/src/main.rs`) is *how*
//! this Rust side answers. The desktop spawns a separate `allmystuff-serve`
//! node and drives it over a control socket. iOS forbids a sandboxed app
//! from spawning child processes, so the phone **embeds the whole stack
//! in-process** (see [`engine`]): the `myownmesh` daemon, the same
//! `allmystuff-node` engine the serve binary runs (built capture-less), and
//! the desktop's node-backed command surface dispatched straight into it —
//! presence, routing, video/terminal/files, sites, rooms, fleet admin, CEC.
//! See `docs/MOBILE.md` for the full architecture.
//!
//! [`allmystuff_mobile_core`] remains the phone's *model*: the
//! viewer/controller capability set and `NodeProfile` it puts on the graph.
//! [`scan_self`] answers from the live engine when it's up and falls back to
//! that model (under the `"this"` placeholder id the store re-homes) while
//! the engine is still booting — so the graph is honest from the first frame.
//! What stays desktop-only, deliberately: secondary windows (every surface is
//! in-app on a phone), the self-updater (the stores own updates), the
//! Always-On service, and tray/autostart behaviour.

use allmystuff_mobile_core::prelude::*;
use serde_json::{json, Value};
#[cfg(feature = "mesh")]
use tauri::Manager as _;

// The real stack, in-process (behind the default `mesh` feature so the
// NDK-free android CI check can build the shell with `--no-default-features`
// — the engine's C deps need the NDK on device): the embedded myownmesh
// daemon + the same `allmystuff-node` engine the desktop's serve binary
// runs, built capture-less, plus the desktop's command surface dispatching
// straight into it. See `engine.rs` for why the phone piles the three
// processes every other platform separates into one.
#[cfg(feature = "mesh")]
mod commands;
#[cfg(feature = "mesh")]
pub mod engine;
#[cfg(feature = "mesh")]
mod logging;

/// A friendly host-OS string for the node card — "Android", "iOS", or the
/// raw target name on a desktop smoke-test build.
pub(crate) fn os_label() -> &'static str {
    match std::env::consts::OS {
        "android" => "Android",
        "ios" => "iOS",
        other => other,
    }
}

/// Seconds since the Unix epoch — the phone's per-run boot id. A peer that
/// sees a new value knows the phone restarted, the same event-driven gossip
/// the desktop uses.
pub(crate) fn boot_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Scan "this machine" — the phone. Mirrors the desktop `scan_self`'s
/// `ScanResult` shape (`{ node_id, label, hostname, summary, capabilities }`)
/// so the store re-homes the local node with no mobile special case.
///
/// `node_id` is the engine's real ed25519 device id once the mesh is up, and
/// `"this"` before then — exactly the placeholder the desktop uses for its
/// offline/demo graph, which the store already understands (it re-homes the
/// local node when the real id arrives). The capabilities are real either way:
/// the viewer/controller set every phone advertises, built by
/// [`mobile_capabilities`] the same way a desktop's come from its hardware scan.
fn scan_self_impl(node_id: String) -> Result<Value, String> {
    let node = NodeId::from(node_id.as_str());
    let cfg = MobileNodeConfig {
        label: "My Phone".to_string(),
        os: os_label().to_string(),
        model: String::new(),
        ram_bytes: 0,
        scope: MobileScope::ViewerController,
    };
    let profile = mobile_profile(&node, &cfg, boot_id(), env!("CARGO_PKG_VERSION"));

    Ok(json!({
        "node_id": profile.node.as_str(),
        "label": profile.label,
        "hostname": "",
        "summary": profile.summary,
        "capabilities": profile.capabilities,
    }))
}

/// With the engine up, the node's own `scan_self` answers (real inventory,
/// real device id — the same reply a desktop gives). Before it's up — and if
/// the node scan ever fails — fall back to the mobile-core profile under the
/// `"this"` placeholder id, which the store already re-homes when the real
/// id arrives.
#[cfg(feature = "mesh")]
#[tauri::command]
async fn scan_self(state: tauri::State<'_, engine::EngineState>) -> Result<Value, String> {
    if let Ok(eng) = state.engine() {
        if let Ok(v) = eng.request("scan_self", serde_json::json!({})).await {
            return Ok(v);
        }
        if let Some(id) = eng.device_id().await {
            return scan_self_impl(id);
        }
    }
    scan_self_impl("this".to_string())
}

#[cfg(not(feature = "mesh"))]
#[tauri::command]
fn scan_self() -> Result<Value, String> {
    scan_self_impl("this".to_string())
}

/// The deep hardware scan, GUI-side exactly like the desktop shell.
#[tauri::command]
fn scan_full() -> Result<Value, String> {
    serde_json::to_value(allmystuff_inventory::scan()).map_err(|e| e.to_string())
}

/// Mirror one frontend diagnostic line into the app's log (the Xcode /
/// logcat console *and* the on-phone `allmystuff.log` — see [`logging`]), so
/// a call reads end to end — the mobile counterpart to the desktop's
/// `client_log`.
#[tauri::command]
fn client_log(line: String) {
    #[cfg(feature = "mesh")]
    tracing::info!("[ui] {line}");
    #[cfg(not(feature = "mesh"))]
    eprintln!("[ui] {line}");
}

/// The shared entry point. On iOS/Android the platform shell calls this
/// through the C ABI (`tauri::mobile_entry_point`); on a desktop smoke-test
/// build `main.rs` calls it directly.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_shell::init());

    // With the engine: boot the whole piled-together stack (embedded daemon
    // → node engine → presence) on a background task so the UI comes up
    // immediately, and answer the desktop's full command surface from it.
    #[cfg(feature = "mesh")]
    let builder = builder
        .manage(engine::EngineState::default())
        .setup(|app| {
            // Route the stack's `tracing` diagnostics (daemon bring-up, mDNS
            // attach failures, peer connects/drops, route lifecycles) to the
            // device console *and* an on-phone log file (see [`logging`]).
            // Without a subscriber they are dropped — and "why is discovery
            // dark" becomes undebuggable on a phone.
            match logging::init(app.handle()) {
                Some(path) => tracing::info!("logging to {}", path.display()),
                None => tracing::warn!("file log unavailable; stderr only"),
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tracing::info!("[boot] starting the in-process stack (daemon + node engine)…");
                match engine::boot(handle.clone()).await {
                    Ok(eng) => {
                        *handle.state::<engine::EngineState>().0.lock().unwrap() = Some(eng);
                        tracing::info!("[boot] node engine up");
                    }
                    Err(e) => tracing::error!("[boot] engine failed to start: {e}"),
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_self,
            scan_full,
            client_log,
            commands::connect_route,
            commands::disconnect_route,
            commands::claim_node,
            commands::upgrade_node,
            commands::restart_node,
            commands::restart_device,
            commands::refresh_node,
            commands::set_claimable,
            commands::set_public_claims,
            commands::claim_via_code,
            commands::kvm_attach,
            commands::kvm_detach,
            commands::kvm_mesh_add,
            commands::kvm_mesh_remove,
            commands::share_grant,
            commands::share_revoke,
            commands::share_stop,
            commands::send_input,
            commands::clipboard_paste,
            commands::clipboard_pull,
            commands::video_watch,
            commands::video_poll,
            commands::video_unwatch,
            commands::video_refresh,
            commands::video_feedback,
            commands::tune_route,
            commands::term_send,
            commands::term_watch,
            commands::term_poll,
            commands::term_unwatch,
            commands::terminal_sessions,
            commands::file_send,
            commands::file_watch,
            commands::file_poll,
            commands::file_unwatch,
            commands::file_download,
            commands::site_scan,
            commands::site_exposed,
            commands::site_set_exposed,
            commands::site_map,
            commands::site_unmap,
            commands::site_mappings,
            commands::site_remote_list,
            commands::site_remote_set_exposed,
            commands::session_snapshot,
            commands::room_send,
            commands::room_share_files,
            commands::room_set_share_peers,
            commands::room_unshare,
            commands::owned_roster,
            commands::fleet_leave,
            commands::fleet_kick,
            commands::fleet_set_name,
            commands::fleet_grant_role,
            commands::fleet_revoke_role,
            commands::fleet_set_hubs,
            commands::fleet_mfa_status,
            commands::fleet_mfa_enroll,
            commands::fleet_mfa_disable,
            commands::forget_node,
            commands::device_support,
            commands::cec_status,
            commands::cec_dial,
            commands::cec_dial_node,
            commands::cec_dialed,
            commands::cec_help_list,
            commands::cec_help_watch,
            commands::cec_cancel_dial,
            commands::cec_pending,
            commands::cec_approve,
            commands::cec_deny,
            commands::cec_revoke,
            commands::cec_grants,
            commands::cec_chat_send,
            commands::cec_chat_history,
            commands::mesh_status,
            commands::mesh_identity,
            commands::mesh_networks,
            commands::mesh_peers,
            commands::link_status,
            commands::mesh_network_add,
            commands::mesh_network_remove,
            commands::mesh_network_update,
            commands::disabled_networks,
            commands::network_set_enabled,
            commands::network_reconnect,
            commands::mesh_config_show,
            commands::mesh_network_id_generate,
            commands::mesh_roster_approve,
            commands::mesh_roster_remove,
            commands::mesh_roster_list,
            commands::mesh_identity_set_label,
        ]);

    // Without it (the NDK-free CI check): just the demo-capable shell commands.
    #[cfg(not(feature = "mesh"))]
    let builder =
        builder.invoke_handler(tauri::generate_handler![scan_self, scan_full, client_log]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running the AllMyStuff mobile app");
}
