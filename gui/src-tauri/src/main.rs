//! AllMyStuff GUI — Tauri shell.
//!
//! The window is a Svelte app; this Rust side:
//!
//!  1. **Scans the machine** (`scan_self`) so "this device" on the graph is
//!     real hardware.
//!  2. **Runs the live mesh** ([`mesh::Mesh`]) — a client of a `myownmesh
//!     serve` daemon over the control socket. Presence makes peers appear;
//!     the route handshake + `cpal` audio bridge make a connection actually
//!     stream. The front-end drives it via `connect_route` /
//!     `disconnect_route` and reads `allmystuff://session` snapshots.
//!  3. **Self-updates** via `allmystuff-updater` (its own release feed —
//!     not the daemon's).

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod audio;
mod byte_queues;
mod camera_capture;
mod cec;
mod clipboard;
mod control_client;
mod daemon_spawn;
mod files;
mod input_inject;
mod mesh;
mod networks_store;
mod ownership;
mod sites;
mod terminal;
mod video;
mod video_decode;
mod wake;
#[cfg(target_os = "linux")]
mod wayland_capture;
mod win_capture;

use std::sync::Arc;

use control_client::{ControlClient, Request, Response};
use mesh::Mesh;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{Manager, RunEvent, State};

struct AppState {
    client: Arc<ControlClient>,
    daemon_child: Mutex<Option<daemon_spawn::DaemonChild>>,
    /// Full configs of networks the user switched off — parked here so
    /// re-enabling re-joins with everything (servers, label, roster path)
    /// intact. See `network_set_enabled`.
    disabled_networks: networks_store::DisabledNetworks,
}

fn unwrap_response(resp: Response) -> Result<Value, String> {
    if !resp.ok {
        return Err(resp.error.unwrap_or_else(|| "(no error message)".into()));
    }
    Ok(resp.data.unwrap_or(Value::Null))
}

// ---- this machine -----------------------------------------------------

/// Scan this machine: `{ node_id, label, summary, capabilities }`. `node_id`
/// is the mesh device id once the session is up (so capabilities match what
/// peers see), else `"this"` for the offline/demo graph; `label` is the
/// hostname shown on the local node.
#[tauri::command]
async fn scan_self(mesh: State<'_, Arc<Mesh>>) -> Result<Value, String> {
    let me = mesh
        .resolve_local_id()
        .await
        .unwrap_or_else(|| "this".to_string());
    let node = allmystuff_graph::NodeId::from(me.as_str());
    let inv = allmystuff_inventory::scan();
    serde_json::to_value(json!({
        "node_id": me,
        "label": inv.host.hostname,
        "hostname": inv.host.hostname,
        "summary": allmystuff_bridge::node_summary(&inv),
        "capabilities": allmystuff_bridge::capabilities_with_screens(
            &inv,
            &node,
            &video::extra_screens(),
        ),
    }))
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn scan_full() -> Result<Value, String> {
    serde_json::to_value(allmystuff_inventory::scan()).map_err(|e| e.to_string())
}

// ---- live mesh (presence + routing + audio) ---------------------------

/// Offer a connection from one capability to another. Returns the route id.
#[tauri::command]
async fn connect_route(
    mesh: State<'_, Arc<Mesh>>,
    from: String,
    to: String,
    media: String,
    video: Option<Vec<String>>,
) -> Result<String, String> {
    mesh.inner()
        .connect(from, to, media, video.unwrap_or_default())
        .await
}

#[tauri::command]
async fn disconnect_route(mesh: State<'_, Arc<Mesh>>, route_id: String) -> Result<(), String> {
    mesh.inner().disconnect(route_id).await
}

/// Mirror one frontend diagnostic line into the GUI's `tracing` log. The
/// call plane decides who to wire entirely in the webview (online/claimed
/// gates, sink lookup, presence) — decisions the Rust side never sees — so
/// a toggle that wires *nothing* is indistinguishable from one the mesh
/// dropped. Routing those lines here puts them in the same
/// `ALLMYSTUFF_GUI_LOG` stream as the backend's route lifecycle, so one
/// capture reads a call end to end.
#[tauri::command]
fn client_log(line: String) {
    tracing::info!("{line}");
}

/// Claim a device as one of yours. Only takes if the target is in claim
/// mode; the target's next presence advert (owner = us) confirms it.
#[tauri::command]
async fn claim_node(mesh: State<'_, Arc<Mesh>>, node: String) -> Result<(), String> {
    mesh.inner().claim(node).await
}

/// Ask one of your fleet machines to update its AllMyStuff to the channel's
/// latest release and restart. The target enforces owner/fleet before acting;
/// its next presence advert (the new version) confirms it landed.
#[tauri::command]
async fn upgrade_node(mesh: State<'_, Arc<Mesh>>, node: String) -> Result<(), String> {
    mesh.inner().request_upgrade(node).await
}

/// Put this device into / out of claim mode so another of your machines can
/// adopt it. Returns whether it's now claimable.
#[tauri::command]
async fn set_claimable(mesh: State<'_, Arc<Mesh>>, claimable: bool) -> Result<bool, String> {
    mesh.inner().set_claimable(claimable).await
}

/// Forward one keyboard/mouse event down an active outbound input route —
/// the console window's control stream.
#[tauri::command]
async fn send_input(
    mesh: State<'_, Arc<Mesh>>,
    route_id: String,
    action: serde_json::Value,
) -> Result<(), String> {
    let action: allmystuff_session::InputAction =
        serde_json::from_value(action).map_err(|e| e.to_string())?;
    mesh.inner().send_input(route_id, action).await
}

/// Read this machine's clipboard and push it down an active outbound
/// clipboard route — the console calls this the moment it forwards a paste.
/// The backend does the read (the only place that can see file references on
/// the OS clipboard) and streams text, an image, or files.
#[tauri::command]
async fn clipboard_paste(mesh: State<'_, Arc<Mesh>>, route_id: String) -> Result<(), String> {
    mesh.inner().clipboard_paste(route_id).await
}

/// Register the calling window's interest in a route's inbound video.
/// Packets queue backend-side from this moment; the window drains them
/// with `video_poll` once per display refresh. (Pull, not push: a missed
/// poll costs one tick, where a lost push on Tauri's ordered IPC channel
/// silently froze the stream for good.) `decode` asks the backend to run
/// inbound H.264 through the native decoder and queue ready-to-paint RGBA
/// frames — for webviews without WebCodecs, and the bottom rung of the
/// console's decode ladder.
#[tauri::command]
fn video_watch(mesh: State<'_, Arc<Mesh>>, route_id: String, decode: Option<bool>) -> u64 {
    mesh.video_watch(route_id, decode.unwrap_or(false))
}

/// Drain the queued packets for a route as one raw batch:
/// `[u32 len][28-byte header + payload]…`, empty when nothing arrived.
#[tauri::command]
fn video_poll(mesh: State<'_, Arc<Mesh>>, route_id: String) -> tauri::ipc::Response {
    tauri::ipc::Response::new(mesh.video_poll(&route_id))
}

/// Stop streaming a route's frames to the front-end (console closed or
/// switched input). The token scopes the release to the claim that made
/// it, so a late unwatch can't tear down a newer watcher of the same
/// route. Idempotent.
#[tauri::command]
fn video_unwatch(mesh: State<'_, Arc<Mesh>>, route_id: String, token: u64) {
    mesh.video_unwatch(&route_id, token);
}

/// Ask the sender of an inbound display route for a clean decode entry
/// (IDR) now — the console's decoder hit an error. Rate-limited backend-
/// side; safe to call from a decode-error handler.
#[tauri::command]
async fn video_refresh(mesh: State<'_, Arc<Mesh>>, route_id: String) -> Result<(), String> {
    mesh.inner().request_refresh(route_id).await
}

/// Ask the sender of an inbound display route to stream with these
/// quality picks; absent values mean "automatic". The console's pills.
#[tauri::command]
async fn tune_route(
    mesh: State<'_, Arc<Mesh>>,
    route_id: String,
    max_edge: Option<u32>,
    bitrate: Option<u32>,
    fps: Option<u32>,
) -> Result<(), String> {
    mesh.inner()
        .request_tune(route_id, max_edge, bitrate, fps)
        .await
}

// ---- terminal (the mesh-native shell) ----------------------------------

/// Forward keystrokes or a resize from a terminal window down its active
/// terminal route (the viewer side of a mesh-native shell).
#[tauri::command]
async fn term_send(
    mesh: State<'_, Arc<Mesh>>,
    route_id: String,
    event: serde_json::Value,
) -> Result<(), String> {
    let event: allmystuff_session::TermEvent =
        serde_json::from_value(event).map_err(|e| e.to_string())?;
    mesh.inner().term_send(route_id, event).await
}

/// Register the calling terminal window's interest in a route's output.
/// Bytes buffer backend-side from route-activation (so the shell's first
/// prompt is never lost); the window drains them with `term_poll` on each
/// `allmystuff://term-ready` poke. Same pull-not-push shape as video.
#[tauri::command]
fn term_watch(mesh: State<'_, Arc<Mesh>>, route_id: String) -> u64 {
    mesh.term_watch(&route_id)
}

/// Drain the queued output for a terminal route as one raw batch:
/// `[u32 le len][bytes]…`, empty when nothing arrived.
#[tauri::command]
fn term_poll(mesh: State<'_, Arc<Mesh>>, route_id: String) -> tauri::ipc::Response {
    tauri::ipc::Response::new(mesh.term_poll(&route_id))
}

/// Release a terminal window's claim on a route's output (tab closed).
/// Token-scoped and idempotent, like `video_unwatch`.
#[tauri::command]
fn term_unwatch(mesh: State<'_, Arc<Mesh>>, route_id: String, token: u64) {
    mesh.term_unwatch(&route_id, token);
}

/// Open (or focus) the dedicated terminal window for `node` — one OS
/// window per machine, holding that machine's terminal tabs. The window
/// loads the same app with `?terminal=<node>`.
#[tauri::command]
async fn open_terminal_window(app: tauri::AppHandle, node: String) -> Result<(), String> {
    let label = format!("terminal-{}", window_slug(&node));
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App(format!("index.html?terminal={node}").into()),
    )
    .title("AllMyStuff terminal")
    .inner_size(940.0, 600.0)
    .min_inner_size(480.0, 320.0)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- files (the mesh-native file manager) -------------------------------

/// Forward one file request from a files window down its active files
/// route (the viewer side of a mesh-native file session).
#[tauri::command]
async fn file_send(
    mesh: State<'_, Arc<Mesh>>,
    route_id: String,
    event: serde_json::Value,
) -> Result<(), String> {
    let event: allmystuff_session::FileEvent =
        serde_json::from_value(event).map_err(|e| e.to_string())?;
    mesh.inner().file_send(route_id, event).await
}

/// Register the calling files window's interest in a route's responses.
/// Frames buffer backend-side from route-activation; the window drains
/// them with `file_poll` on each `allmystuff://file-ready` poke. Same
/// pull-not-push shape as the terminal and video planes.
#[tauri::command]
fn file_watch(mesh: State<'_, Arc<Mesh>>, route_id: String) -> u64 {
    mesh.file_watch(&route_id)
}

/// Drain the queued responses for a files route as one raw batch:
/// `[u32 le len][frame json]…`, empty when nothing arrived.
#[tauri::command]
fn file_poll(mesh: State<'_, Arc<Mesh>>, route_id: String) -> tauri::ipc::Response {
    tauri::ipc::Response::new(mesh.file_poll(&route_id))
}

/// Release a files window's claim on a route's responses (window closed).
/// Token-scoped and idempotent, like `term_unwatch`.
#[tauri::command]
fn file_unwatch(mesh: State<'_, Arc<Mesh>>, route_id: String, token: u64) {
    mesh.file_unwatch(&route_id, token);
}

/// Route the coming `Read` request's chunks straight into this machine's
/// Downloads folder (instead of the window). Returns the destination path;
/// completion lands as `allmystuff://file-saved`. Call *before* sending
/// the request so the first chunk can't race the registration.
#[tauri::command]
fn file_download(
    mesh: State<'_, Arc<Mesh>>,
    route_id: String,
    req: u64,
    name: String,
) -> Result<String, String> {
    mesh.file_download(route_id, req, &name)
}

/// Open (or focus) the dedicated files window for `node` — one OS window
/// per machine, the finder-like view of its disk. The window loads the
/// same app with `?files=<node>`.
#[tauri::command]
async fn open_files_window(app: tauri::AppHandle, node: String) -> Result<(), String> {
    let label = format!("files-{}", window_slug(&node));
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App(format!("index.html?files={node}").into()),
    )
    .title("AllMyStuff files")
    .inner_size(940.0, 640.0)
    .min_inner_size(480.0, 320.0)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- sites (the reverse proxy) -----------------------------------------

/// This machine's discovered listening TCP services (with an active banner
/// probe), so the Sites tab can offer each to expose. The probe does
/// blocking socket I/O, so it runs off the command executor.
#[tauri::command]
async fn site_scan(
    mesh: State<'_, Arc<Mesh>>,
) -> Result<Vec<allmystuff_inventory::ListeningService>, String> {
    let mesh = mesh.inner().clone();
    tokio::task::spawn_blocking(move || mesh.site_scan())
        .await
        .map_err(|e| e.to_string())
}

/// The services this machine currently advertises, as id → display name
/// (empty name = the classified default).
#[tauri::command]
fn site_exposed(mesh: State<'_, Arc<Mesh>>) -> std::collections::BTreeMap<String, String> {
    mesh.site_exposed()
}

/// Set which listening services this machine advertises (id → display name).
/// Re-broadcasts presence so peers' Sites tabs update; returns the new set.
#[tauri::command]
async fn site_set_exposed(
    mesh: State<'_, Arc<Mesh>>,
    exposed: std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    Ok(mesh.inner().site_set_exposed(exposed).await)
}

/// Map a peer's site to a local port — set up the reverse-proxy route and
/// bind a local listener. Returns `{ localPort }`.
#[tauri::command]
async fn site_map(mesh: State<'_, Arc<Mesh>>, node: String, port: u16) -> Result<Value, String> {
    let local_port = mesh.inner().site_map(node, port).await?;
    Ok(json!({ "localPort": local_port }))
}

/// Tear a site mapping down (unbind the local listener, drop the route).
#[tauri::command]
async fn site_unmap(mesh: State<'_, Arc<Mesh>>, node: String, port: u16) -> Result<(), String> {
    mesh.inner().site_unmap(node, port).await
}

/// Every site this machine currently has mapped: `{ node, port, localPort }`.
#[tauri::command]
fn site_mappings(mesh: State<'_, Arc<Mesh>>) -> Vec<Value> {
    mesh.site_mappings()
        .into_iter()
        .map(|(node, port, local_port)| json!({ "node": node, "port": port, "localPort": local_port }))
        .collect()
}

/// Ask a co-owned fleet machine for its full site list, to manage its
/// exposure from its drawer. The reply arrives as `allmystuff://node-sites`.
#[tauri::command]
async fn site_remote_list(mesh: State<'_, Arc<Mesh>>, node: String) -> Result<(), String> {
    mesh.inner().site_remote_list(node).await
}

/// Tell a co-owned fleet machine to advertise exactly `exposed` (id → name).
#[tauri::command]
async fn site_remote_set_exposed(
    mesh: State<'_, Arc<Mesh>>,
    node: String,
    exposed: std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    mesh.inner().site_remote_set_exposed(node, exposed).await
}

/// Open (or focus) a dedicated console window for `node` — its own OS
/// window, so several remote consoles can be on screen at once. The window
/// loads the same app with `?console=<node>`, which renders just the
/// console for that machine.
#[tauri::command]
async fn open_console_window(app: tauri::AppHandle, node: String) -> Result<(), String> {
    let label = format!("console-{}", window_slug(&node));
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App(format!("index.html?console={node}").into()),
    )
    .title("AllMyStuff console")
    .inner_size(1100.0, 740.0)
    .min_inner_size(560.0, 380.0)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or focus) the dedicated window for one virtual room — the call
/// itself, in its own OS window like the console / terminal / files
/// sessions, so it can be moved, resized and full-screened. The window
/// loads the same app with `?room=<room id>`.
#[tauri::command]
async fn open_room_window(app: tauri::AppHandle, room: String) -> Result<(), String> {
    let label = format!("room-{}", window_slug(&room));
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App(format!("index.html?room={room}").into()),
    )
    .title("AllMyStuff room")
    .inner_size(1180.0, 760.0)
    .min_inner_size(640.0, 440.0)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or focus) the popout window for one video stream — a console
/// input or a room share lifted out of its tab into its own OS window
/// (movable, resizable, fullscreen-able), so several streams can be on
/// screen at once. The window loads the same app with `?video=<key>`;
/// the key (`cap:<capability id>` / `share:<route id>`) tells the popout
/// what to wire or watch. `title` names the stream until the popout
/// retitles itself with resolved labels.
#[tauri::command]
async fn open_video_window(
    app: tauri::AppHandle,
    key: String,
    title: String,
) -> Result<(), String> {
    let label = format!("video-{}", window_slug(&key));
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        // The key carries capability/route ids (colons, the route arrow) —
        // percent-encode so the query survives; URLSearchParams decodes.
        tauri::WebviewUrl::App(format!("index.html?video={}", query_encode(&key)).into()),
    )
    .title(&title)
    .inner_size(880.0, 560.0)
    .min_inner_size(380.0, 260.0)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// A node id reduced to the characters Tauri allows in a window label —
/// one stable label per machine, so re-opening focuses instead of stacking.
fn window_slug(node: &str) -> String {
    node.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Percent-encode `s` for a URL query value (RFC 3986 unreserved kept,
/// everything else `%XX`) — what a popout key needs to ride
/// `?video=<key>` intact. The front-end's `URLSearchParams` decodes it.
fn query_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Current peers + live route states.
#[tauri::command]
fn session_snapshot(mesh: State<'_, Arc<Mesh>>) -> Value {
    mesh.snapshot()
}

/// The owned-fleet roster: the shared key and the devices this owner has
/// claimed (and that have converged via gossip). Drives the Fleet settings
/// view; updated live by the `allmystuff://owned` event.
#[tauri::command]
fn owned_roster(mesh: State<'_, Arc<Mesh>>) -> Value {
    mesh.owned_roster_value()
}

/// Leave the fleet this device belongs to (and release its owner) — the
/// remaining members converge on the bumped roster without us.
#[tauri::command]
async fn fleet_leave(mesh: State<'_, Arc<Mesh>>) -> Result<(), String> {
    mesh.inner().fleet_leave().await
}

/// Kick a device out of the fleet. Only a member may kick (the backend
/// enforces it), and never itself — that's `fleet_leave`.
#[tauri::command]
async fn fleet_kick(mesh: State<'_, Arc<Mesh>>, device: String) -> Result<(), String> {
    mesh.inner().fleet_kick(device).await
}

/// Name (or rename) the fleet this device belongs to. Members only; the
/// renamed roster gossips out and converges like any membership change.
#[tauri::command]
async fn fleet_set_name(mesh: State<'_, Arc<Mesh>>, name: String) -> Result<(), String> {
    mesh.inner().fleet_set_name(name).await
}

/// Fan one room-plane message (invite / join / leave / chat) out to the
/// given members. Best-effort per member; returns how many the daemon
/// actually dispatched to, so the UI can say when a line reached nobody.
#[tauri::command]
async fn room_send(
    mesh: State<'_, Arc<Mesh>>,
    members: Vec<String>,
    message: serde_json::Value,
) -> Result<u32, String> {
    let message: allmystuff_protocol::RoomMessage =
        serde_json::from_value(message).map_err(|e| e.to_string())?;
    mesh.inner().room_send(members, message).await
}

// ---- Shared Files (the call's shared-download area) ---------------------

/// Offer files into a room's Shared Files area — register each path with
/// the members allowed to fetch it, returning the `{ token, name, size }`
/// the GUI hands to the room's host for its shared list. The bytes never
/// leave this machine until a member fetches them by token.
#[tauri::command]
fn room_share_files(
    mesh: State<'_, Arc<Mesh>>,
    members: Vec<String>,
    paths: Vec<String>,
) -> Vec<allmystuff_protocol::SharedFileMeta> {
    mesh.room_share_files(members, paths)
}

/// Refresh the members allowed to fetch a set of shared tokens (the room's
/// roster changed while the files were on offer).
#[tauri::command]
fn room_set_share_peers(mesh: State<'_, Arc<Mesh>>, tokens: Vec<String>, members: Vec<String>) {
    mesh.room_set_share_peers(tokens, members);
}

/// Stop offering a set of shared files (the uploader removed them or left).
#[tauri::command]
fn room_unshare(mesh: State<'_, Arc<Mesh>>, tokens: Vec<String>) {
    mesh.room_unshare(tokens);
}

// ---- mesh control passthroughs ----------------------------------------

#[tauri::command]
async fn mesh_status(state: State<'_, AppState>) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::Status)
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_identity(state: State<'_, AppState>) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::IdentityShow)
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_networks(state: State<'_, AppState>) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::NetworksList)
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_peers(state: State<'_, AppState>, network: String) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::PeersList { network })
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_network_add(
    state: State<'_, AppState>,
    mesh: State<'_, Arc<Mesh>>,
    config: Value,
) -> Result<Value, String> {
    let data = unwrap_response(
        state
            .client
            .request(&Request::NetworkAdd { config })
            .await
            .map_err(|e| e.to_string())?,
    )?;
    // Subscribe + advertise on the freshly-joined network now, not just at
    // next launch — so a network joined mid-session lights up immediately.
    mesh.inner().sync_networks().await;
    Ok(data)
}

/// The whole daemon config — every network with its full signaling / STUN /
/// TURN settings. The Servers settings pane reads this to populate its editor
/// (`NetworksList` only carries summaries).
#[tauri::command]
async fn mesh_config_show(state: State<'_, AppState>) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::ConfigShow)
            .await
            .map_err(|e| e.to_string())?,
    )
}

/// Replace one network's config (its signaling / STUN / TURN servers, label,
/// etc.). The daemon hot-applies cosmetic changes and restarts the transport
/// for server changes; we re-subscribe afterwards so the session reconnects.
#[tauri::command]
async fn mesh_network_update(
    state: State<'_, AppState>,
    mesh: State<'_, Arc<Mesh>>,
    config: Value,
) -> Result<Value, String> {
    let data = unwrap_response(
        state
            .client
            .request(&Request::NetworkUpdate { config })
            .await
            .map_err(|e| e.to_string())?,
    )?;
    mesh.inner().sync_networks().await;
    Ok(data)
}

#[tauri::command]
async fn mesh_roster_approve(
    state: State<'_, AppState>,
    network: String,
    device_id: String,
    label: Option<String>,
) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::RosterApprove {
                network,
                device_id,
                label,
            })
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_roster_remove(
    state: State<'_, AppState>,
    network: String,
    device_id: String,
) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::RosterRemove { network, device_id })
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_roster_list(state: State<'_, AppState>, network: String) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::RosterList { network })
            .await
            .map_err(|e| e.to_string())?,
    )
}

/// Ask the daemon for a fresh, valid network id (the shareable handle peers
/// join with). Used by the "create network" flow.
#[tauri::command]
async fn mesh_network_id_generate(state: State<'_, AppState>) -> Result<Value, String> {
    unwrap_response(
        state
            .client
            .request(&Request::NetworkIdGenerate)
            .await
            .map_err(|e| e.to_string())?,
    )
}

#[tauri::command]
async fn mesh_network_remove(
    state: State<'_, AppState>,
    mesh: State<'_, Arc<Mesh>>,
    network: String,
) -> Result<Value, String> {
    let data = unwrap_response(
        state
            .client
            .request(&Request::NetworkRemove { network })
            .await
            .map_err(|e| e.to_string())?,
    )?;
    mesh.inner().sync_networks().await;
    Ok(data)
}

/// The networks currently switched off (their full parked configs), for
/// the pill menu's disabled rows.
#[tauri::command]
fn disabled_networks(state: State<'_, AppState>) -> Vec<Value> {
    state.disabled_networks.list()
}

/// Switch a network off or back on without deleting it. Off = leave the
/// daemon (peers drop, nothing is advertised there any more) but park the
/// full config locally; on = hand the parked config back to the daemon.
/// The network's roster file survives on disk either way, so approvals
/// aren't lost in between. `network` may be the config id or network id.
#[tauri::command]
async fn network_set_enabled(
    state: State<'_, AppState>,
    mesh: State<'_, Arc<Mesh>>,
    network: String,
    enabled: bool,
) -> Result<Value, String> {
    if enabled {
        let config = state
            .disabled_networks
            .take(&network)
            .ok_or_else(|| format!("'{network}' isn't a disabled network here"))?;
        let rejoin = state
            .client
            .request(&Request::NetworkAdd {
                config: config.clone(),
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(unwrap_response);
        match rejoin {
            Ok(data) => {
                mesh.inner().sync_networks().await;
                Ok(data)
            }
            Err(e) => {
                // Park it back so a failed re-join (daemon down, say) never
                // loses the config.
                state.disabled_networks.park(config);
                Err(e)
            }
        }
    } else {
        // Snapshot the full config *before* leaving — `config_show` is the
        // only place the daemon hands the whole thing back.
        let shown = unwrap_response(
            state
                .client
                .request(&Request::ConfigShow)
                .await
                .map_err(|e| e.to_string())?,
        )?;
        let config = shown
            .pointer("/config/networks")
            .and_then(|v| v.as_array())
            .and_then(|nets| {
                nets.iter()
                    .find(|n| {
                        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                        let nid = n
                            .get("network_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        id == network || nid == network
                    })
                    .cloned()
            })
            .ok_or_else(|| format!("unknown network: {network}"))?;
        if !state.disabled_networks.park(config) {
            return Err("couldn't save the network for later — not disabling it".into());
        }
        let left = state
            .client
            .request(&Request::NetworkRemove {
                network: network.clone(),
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(unwrap_response);
        match left {
            Ok(data) => {
                mesh.inner().sync_networks().await;
                Ok(data)
            }
            Err(e) => {
                // Still joined — un-park so the books match reality.
                let _ = state.disabled_networks.take(&network);
                Err(e)
            }
        }
    }
}

/// Write a network-settings envelope (the GUI's flat, shareable JSON for a
/// network's handle + servers) to disk. Pretty-printed so it's easy to read
/// by hand. Import is a renderer-side `<input type="file">` read, so there's
/// no symmetric import command here.
#[tauri::command]
async fn mesh_network_export_file(path: String, config: Value) -> Result<(), String> {
    let body = serde_json::to_string_pretty(&config).map_err(|e| format!("serialise: {e}"))?;
    std::fs::write(&path, body).map_err(|e| format!("write {path}: {e}"))?;
    Ok(())
}

/// Set this device's display-name override. Persists in the daemon identity
/// and updates the live presence profile so peers see the new name on the
/// next broadcast. An empty string resets the name to the hostname.
#[tauri::command]
async fn mesh_identity_set_label(
    state: State<'_, AppState>,
    mesh: State<'_, Arc<Mesh>>,
    label: String,
) -> Result<Value, String> {
    let data = unwrap_response(
        state
            .client
            .request(&Request::IdentitySetLabel {
                label: label.clone(),
            })
            .await
            .map_err(|e| e.to_string())?,
    )?;
    mesh.inner().set_label(label).await;
    Ok(data)
}

// ---- self-update (AllMyStuff's own updater, not the daemon's) ----------

// ---- CEC service ------------------------------------------------------
//
// The optional Critical Error Computing account and the two services it
// unlocks: Ask-for-Help (Concierge) and the Private Line. State lives in
// `cec::Cec`; the front-end orchestrates the mesh side (join the CEC network,
// approve the service node, mint the help room) on top of these.

#[tauri::command]
fn cec_state(cec: State<'_, cec::Cec>) -> Value {
    cec.snapshot()
}

#[tauri::command]
fn cec_set_backend_url(cec: State<'_, cec::Cec>, url: String) -> Result<Value, String> {
    cec.set_backend_url(url)
}

#[tauri::command]
async fn cec_start_sign_in(cec: State<'_, cec::Cec>, email: String) -> Result<Value, String> {
    cec.start_sign_in(email).await
}

#[tauri::command]
async fn cec_verify_sign_in(
    cec: State<'_, cec::Cec>,
    email: String,
    code: String,
    device_id: Option<String>,
    device_label: Option<String>,
) -> Result<Value, String> {
    cec.verify_sign_in(email, code, device_id, device_label)
        .await
}

#[tauri::command]
async fn cec_refresh(cec: State<'_, cec::Cec>) -> Result<Value, String> {
    cec.refresh().await
}

#[tauri::command]
async fn cec_sign_out(cec: State<'_, cec::Cec>) -> Result<Value, String> {
    cec.sign_out().await
}

#[tauri::command]
async fn cec_provision_mesh(cec: State<'_, cec::Cec>, device_id: String) -> Result<Value, String> {
    cec.provision_mesh(device_id).await
}

#[tauri::command]
async fn cec_rent_private_line(
    cec: State<'_, cec::Cec>,
    label: Option<String>,
) -> Result<Value, String> {
    cec.rent_private_line(label).await
}

#[tauri::command]
async fn cec_list_private_lines(cec: State<'_, cec::Cec>) -> Result<Value, String> {
    cec.list_private_lines().await
}

#[tauri::command]
async fn cec_cancel_private_line(cec: State<'_, cec::Cec>, id: String) -> Result<Value, String> {
    cec.cancel_private_line(id).await
}

#[tauri::command]
async fn cec_ask_for_help(
    cec: State<'_, cec::Cec>,
    network_id: String,
    room_id: String,
    device_id: String,
    topic: Option<String>,
) -> Result<Value, String> {
    cec.ask_for_help(network_id, room_id, device_id, topic)
        .await
}

#[tauri::command]
async fn cec_help_status(cec: State<'_, cec::Cec>, id: String) -> Result<Value, String> {
    cec.help_status(id).await
}

#[tauri::command]
async fn cec_cancel_help(cec: State<'_, cec::Cec>, id: String) -> Result<Value, String> {
    cec.cancel_help(id).await
}

#[tauri::command]
async fn update_status() -> Result<Value, String> {
    serde_json::to_value(allmystuff_updater::status().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_check() -> Result<Value, String> {
    let outcome = allmystuff_updater::check_now(true)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(outcome).map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_apply() -> Result<Value, String> {
    let applied = allmystuff_updater::apply_now().map_err(|e| e.to_string())?;
    Ok(json!({ "applied": applied }))
}

#[tauri::command]
async fn update_set_prefs(prefs: Value) -> Result<Value, String> {
    let prefs: allmystuff_updater::UpdatePrefs =
        serde_json::from_value(prefs).map_err(|e| e.to_string())?;
    serde_json::to_value(allmystuff_updater::set_prefs(prefs).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// The latest release version on the configured channel (read-only — no
/// staging). The graph compares it to each remote's advertised version to
/// decide whether to offer that machine an upgrade.
#[tauri::command]
async fn update_latest_version() -> Result<Option<String>, String> {
    allmystuff_updater::latest_version()
        .await
        .map_err(|e| e.to_string())
}

/// Pi / aarch64 Linux WebKitGTK rendering workaround — paint on the CPU so
/// the animated SVG graph doesn't corrupt or wedge the compositor. Kept in
/// sync with MyOwnMesh and MyOwnLLM.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn workaround_pi_webkit_rendering() {
    for (key, value) in [
        ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
        ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
    ] {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}

fn main() {
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    workaround_pi_webkit_rendering();

    // Apply any update staged on the previous run before anything else.
    allmystuff_updater::apply_pending_if_any();

    let log_level = std::env::var("ALLMYSTUFF_GUI_LOG")
        .unwrap_or_else(|_| "info,allmystuff_gui=info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_level))
        .with_target(false)
        .init();

    let client = Arc::new(ControlClient::new().expect("resolve control socket path"));

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Terminal copy/paste: the async clipboard API is unreliable in
        // WebKitGTK, so the terminal windows use the plugin instead.
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState {
            client: client.clone(),
            daemon_child: Mutex::new(None),
            disabled_networks: networks_store::DisabledNetworks::load(),
        })
        .manage(cec::Cec::load())
        .invoke_handler(tauri::generate_handler![
            scan_self,
            scan_full,
            connect_route,
            disconnect_route,
            client_log,
            claim_node,
            upgrade_node,
            set_claimable,
            send_input,
            clipboard_paste,
            video_watch,
            video_poll,
            video_unwatch,
            video_refresh,
            tune_route,
            open_console_window,
            open_video_window,
            term_send,
            term_watch,
            term_poll,
            term_unwatch,
            open_terminal_window,
            file_send,
            file_watch,
            file_poll,
            file_unwatch,
            file_download,
            open_files_window,
            site_scan,
            site_exposed,
            site_set_exposed,
            site_map,
            site_unmap,
            site_mappings,
            site_remote_list,
            site_remote_set_exposed,
            session_snapshot,
            room_send,
            room_share_files,
            room_set_share_peers,
            room_unshare,
            open_room_window,
            owned_roster,
            fleet_leave,
            fleet_kick,
            fleet_set_name,
            mesh_status,
            mesh_identity,
            mesh_networks,
            mesh_peers,
            mesh_network_add,
            mesh_network_remove,
            mesh_network_update,
            disabled_networks,
            network_set_enabled,
            mesh_config_show,
            mesh_network_export_file,
            mesh_network_id_generate,
            mesh_roster_approve,
            mesh_roster_remove,
            mesh_roster_list,
            mesh_identity_set_label,
            cec_state,
            cec_set_backend_url,
            cec_start_sign_in,
            cec_verify_sign_in,
            cec_refresh,
            cec_sign_out,
            cec_provision_mesh,
            cec_rent_private_line,
            cec_list_private_lines,
            cec_cancel_private_line,
            cec_ask_for_help,
            cec_help_status,
            cec_cancel_help,
            update_status,
            update_check,
            update_apply,
            update_set_prefs,
            update_latest_version,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let mesh = Mesh::new(client.clone(), handle.clone());
            app.manage(mesh.clone());
            let client = client.clone();
            tauri::async_runtime::spawn(async move {
                match daemon_spawn::ensure_daemon_running(&client).await {
                    Ok(child) => {
                        if let Some(child) = child {
                            handle
                                .state::<AppState>()
                                .daemon_child
                                .lock()
                                .replace(child);
                        }
                    }
                    Err(e) => tracing::warn!("daemon auto-spawn skipped: {e:#}"),
                }
                // Bring the live session online (subscribes, advertises
                // presence, starts the event pump).
                mesh.start().await;
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the AllMyStuff GUI")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                let state = app.state::<AppState>();
                let child = state.daemon_child.lock().take();
                if let Some(c) = child {
                    drop(c);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encode_round_trips_popout_keys() {
        // RFC 3986 unreserved characters pass through untouched…
        assert_eq!(query_encode("abc-XYZ_0.9~"), "abc-XYZ_0.9~");
        // …while a popout key's colons and the route arrow are escaped so
        // the `?video=<key>` query survives (URLSearchParams decodes).
        assert_eq!(
            query_encode("cap:desk:cam:video0"),
            "cap%3Adesk%3Acam%3Avideo0"
        );
        assert_eq!(
            query_encode("share:route:a→b"),
            "share%3Aroute%3Aa%E2%86%92b"
        );
    }

    #[test]
    fn window_slug_flattens_to_label_charset() {
        assert_eq!(window_slug("cap:desk:cam/0"), "cap_desk_cam_0");
        assert_eq!(window_slug("plain-id_9"), "plain-id_9");
    }
}
