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
//! from spawning child processes, so the phone instead **embeds** its
//! viewer/controller brain in-process: this shell is built on
//! [`allmystuff_mobile_core`], and the live mesh engine drops in behind that
//! crate's `MeshClient` seam (the embedded `myownmesh-core` binding — the
//! next slice; see `docs/MOBILE.md`).
//!
//! Today this shell answers exactly the calls it can answer *honestly* from
//! `mobile-core` — chiefly [`scan_self`], which puts a real phone node on the
//! graph with the real viewer/controller capability set. Calls that need a
//! live mesh (presence, routing, the media planes) are intentionally not
//! registered yet, so the frontend keeps its demo behaviour for them until
//! the engine binding makes them real, rather than this side faking a mesh.

use allmystuff_mobile_core::prelude::*;
use serde_json::{json, Value};

/// A friendly host-OS string for the node card — "Android", "iOS", or the
/// raw target name on a desktop smoke-test build.
fn os_label() -> &'static str {
    match std::env::consts::OS {
        "android" => "Android",
        "ios" => "iOS",
        other => other,
    }
}

/// Seconds since the Unix epoch — the phone's per-run boot id. A peer that
/// sees a new value knows the phone restarted, the same event-driven gossip
/// the desktop uses.
fn boot_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Scan "this machine" — the phone. Mirrors the desktop `scan_self`'s
/// `ScanResult` shape (`{ node_id, label, hostname, summary, capabilities }`)
/// so the store re-homes the local node with no mobile special case.
///
/// `node_id` is `"this"` until the embedded engine assigns the phone its real
/// ed25519 device id — exactly the placeholder the desktop uses for its
/// offline/demo graph, which the store already understands. The capabilities,
/// though, are real: the viewer/controller set every phone advertises, built
/// by [`mobile_capabilities`] the same way a desktop's come from its hardware
/// scan.
#[tauri::command]
fn scan_self() -> Result<Value, String> {
    // "this" mirrors the desktop's offline-graph placeholder; the engine
    // swaps in the real device id once it's up.
    let node = NodeId::from("this");
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

/// Mirror one frontend diagnostic line into the app's stderr log, so a
/// `adb logcat` / Xcode console reads a call end to end — the mobile
/// counterpart to the desktop's `client_log`.
#[tauri::command]
fn client_log(line: String) {
    eprintln!("[ui] {line}");
}

/// The shared entry point. On iOS/Android the platform shell calls this
/// through the C ABI (`tauri::mobile_entry_point`); on a desktop smoke-test
/// build `main.rs` calls it directly.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![scan_self, client_log])
        .run(tauri::generate_context!())
        .expect("error while running the AllMyStuff mobile app");
}
